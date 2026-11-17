//! Looks for album art and the bitrate in the collection's files on a thread
//! of its own, one file at a time, for Collection Health's "No cover" and
//! "Low bitrate" checks. Each answer is
//! kept in the library with the file's modification time, so a file is
//! only looked at again once it changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::UNIX_EPOCH;

use opendj_engine::{cover, tags};
use opendj_library::health::FileLook;

/// A file to look at with any previously cached result.
type Request = (usize, PathBuf, String, Option<FileLook>);

pub struct CoverScan {
    requests: Sender<Request>,
    /// Id and what was found.
    done: Receiver<(usize, Option<FileLook>)>,
    /// Ids sent and not yet back.
    queued: HashSet<usize>,
}

/// A file's modification time as a Unix time, 0 if it can't be read.
pub fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Whether iCloud has taken the file's contents off this computer, leaving
/// only its name: reading it would download it, so it is left alone.
#[cfg(target_os = "macos")]
pub fn offloaded(path: &Path) -> bool {
    use std::os::macos::fs::MetadataExt;
    /// `SF_DATALESS` in `<sys/stat.h>`.
    const DATALESS: u32 = 0x4000_0000;
    std::fs::metadata(path).is_ok_and(|m| m.st_flags() & DATALESS != 0)
}

#[cfg(not(target_os = "macos"))]
pub fn offloaded(_path: &Path) -> bool {
    false
}

impl CoverScan {
    pub fn start() -> Self {
        let (requests, rx) = channel::<Request>();
        let (tx, done) = channel();
        std::thread::Builder::new()
            .name("cover-scan".into())
            .spawn(move || {
                for (id, path, album, cached) in rx {
                    let look = if offloaded(&path) {
                        None
                    } else {
                        let modified = mtime(&path);
                        if cached
                            .is_some_and(|look| look.mtime == modified && look.bitrate.is_some())
                        {
                            None
                        } else {
                            Some(FileLook {
                                mtime: modified,
                                cover: cover::has_cover(&path, &album),
                                // 0: looked, but the file doesn't say.
                                bitrate: Some(tags::file_info(&path).bitrate.unwrap_or(0)),
                            })
                        }
                    };
                    if tx.send((id, look)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn cover scan thread");
        Self {
            requests,
            done,
            queued: HashSet::new(),
        }
    }

    /// Queues a file to look at, unless it is queued already.
    pub fn add(&mut self, id: usize, path: &Path, album: &str) {
        self.add_cached(id, path, album, None);
    }

    /// Checks a cached result on the worker, including the filesystem metadata.
    pub fn add_cached(&mut self, id: usize, path: &Path, album: &str, cached: Option<FileLook>) {
        if self.queued.insert(id) {
            let _ = self
                .requests
                .send((id, path.to_path_buf(), album.to_string(), cached));
        }
    }

    /// Files looked at since the last call.
    pub fn take(&mut self) -> Vec<(usize, FileLook)> {
        let done: Vec<_> = self.done.try_iter().collect();
        for (id, ..) in &done {
            self.queued.remove(id);
        }
        done.into_iter()
            .filter_map(|(id, look)| look.map(|look| (id, look)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime};

    #[test]
    fn cached_checks_complete_without_replacing_unchanged_facts() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("opendj-cover-cache-{unique}.txt"));
        std::fs::write(&path, b"not an audio file").unwrap();
        let mut scanner = CoverScan::start();
        scanner.add_cached(
            7,
            &path,
            "",
            Some(FileLook {
                mtime: mtime(&path),
                cover: true,
                bitrate: Some(320),
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !scanner.queued.is_empty() {
            assert!(scanner.take().is_empty());
            assert!(
                Instant::now() < deadline,
                "cached request never acknowledged"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        scanner.add(7, &path, "");
        let mut result = Vec::new();
        while result.is_empty() && Instant::now() < deadline {
            result.extend(scanner.take());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(result.len(), 1, "an explicit scan must remain available");
        assert!(!result[0].1.cover);
        std::fs::remove_file(path).unwrap();
    }
}
