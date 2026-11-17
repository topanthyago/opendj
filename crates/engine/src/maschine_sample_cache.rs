//! Atomic, non-overwriting publication of content-addressed Groove samples.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const LIMIT: u32 = 1000;

struct Staged {
    path: PathBuf,
    file: Option<File>,
    owned: bool,
}

impl Staged {
    fn create(root: &Path, stem: &str) -> Result<Self, String> {
        for _ in 0..LIMIT {
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(".{stem}.{}.{}.tmp", std::process::id(), nonce));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        owned: true,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Err("too many Groove sample temporary filename collisions".into())
    }

    fn remove(mut self) -> Result<(), String> {
        drop(self.file.take());
        fs::remove_file(&self.path).map_err(|error| format!("{}: {error}", self.path.display()))?;
        self.owned = false;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_directory(root: &Path) -> Result<(), String> {
    File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("{}: {error}", root.display()))
}

#[cfg(not(unix))]
fn sync_directory(_root: &Path) -> Result<(), String> {
    Ok(())
}

/// Existing names and their contents are never removed or overwritten. A
/// matching file is reused, including one published by another process.
pub(super) fn publish(root: &Path, stem: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    publish_with(root, stem, bytes, |file, bytes| file.write_all(bytes))
}

fn publish_with(
    root: &Path,
    stem: &str,
    bytes: &[u8],
    write: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
    // Reuse without rewriting; a pre-existing different or partial file is a
    // collision, not ours to repair or remove.
    let mut first_free = None;
    for suffix in 0..LIMIT {
        let path = root.join(format!("{stem}-{suffix}.wav"));
        match fs::read(&path) {
            Ok(existing) if existing == bytes => {
                sync_directory(root)?;
                return Ok(path);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                first_free = Some(suffix);
                break;
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    let first_free = first_free.ok_or("too many Groove sample filename collisions")?;
    let mut staged = Staged::create(root, stem)?;
    let file = staged.file.as_mut().expect("staged file");
    write(file, bytes)
        .and_then(|()| {
            if file.metadata()?.len() != bytes.len() as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "incomplete staged sample",
                ));
            }
            file.sync_all()
        })
        .map_err(|error| format!("{}: {error}", staged.path.display()))?;
    for suffix in first_free..LIMIT {
        let path = root.join(format!("{stem}-{suffix}.wav"));
        match fs::hard_link(&staged.path, &path) {
            Ok(()) => {
                staged.remove()?;
                sync_directory(root)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))? == bytes {
                    staged.remove()?;
                    sync_directory(root)?;
                    return Ok(path);
                }
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err("too many Groove sample filename collisions".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opendj-atomic-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn failed_partial_write_leaves_existing_file_and_retry_succeeds() {
        let root = root("fault");
        let old = root.join("crc-9-0.wav");
        fs::write(&old, b"").unwrap();
        let data = b"new-sound";
        let result = publish_with(&root, "crc-9", data, |file, bytes| {
            file.write_all(&bytes[..3])?;
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "injected write failure",
            ))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&old).unwrap(), b"");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            1,
            "only the original file remains"
        );
        let short = publish_with(&root, "crc-9", data, |file, bytes| {
            file.write_all(&bytes[..2])
        });
        assert!(short.is_err(), "a short successful writer must not publish");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        #[cfg(unix)]
        {
            let full = publish_with(&root, "crc-9", data, |file, bytes| {
                file.write_all(&bytes[..2])?;
                Err(io::Error::from_raw_os_error(28)) // ENOSPC without filling the disk.
            });
            assert!(full.is_err());
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        }
        let path = publish(&root, "crc-9", data).unwrap();
        assert_eq!(path, root.join("crc-9-1.wav"));
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(publish(&root, "crc-9", data).unwrap(), path);
        assert_eq!(fs::read(&old).unwrap(), b"");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_identical_producers_publish_once() {
        let root = root("same");
        let gate = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (root, gate) = (root.clone(), gate.clone());
                std::thread::spawn(move || {
                    publish_with(&root, "same", b"complete audio", |file, bytes| {
                        file.write_all(bytes)?;
                        gate.wait();
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect();
        let paths: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(fs::read(&paths[0]).unwrap(), b"complete audio");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_collisions_preserve_both_complete_files() {
        let root = root("different");
        let gate = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [b"first".as_slice(), b"other".as_slice()]
            .into_iter()
            .map(|bytes| {
                let (root, gate) = (root.clone(), gate.clone());
                std::thread::spawn(move || {
                    publish_with(&root, "same", bytes, |file, bytes| {
                        file.write_all(bytes)?;
                        gate.wait();
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect();
        let paths: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_ne!(paths[0], paths[1]);
        assert_eq!(fs::read(&paths[0]).unwrap(), b"first");
        assert_eq!(fs::read(&paths[1]).unwrap(), b"other");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }
}
