//! Project destinations and the FILE page's persistent, ordered recent list.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const RECENT_LIMIT: usize = 20;

#[derive(Default)]
struct History {
    paths: Vec<PathBuf>,
    error: Option<String>,
}

pub struct Projects {
    file: Option<PathBuf>,
    history: Mutex<History>,
}

impl Projects {
    pub fn new() -> Self {
        Self::load(crate::settings::config_dir().map(|p| p.join("maschine/recent-projects.json")))
    }

    fn load(file: Option<PathBuf>) -> Self {
        let mut history = History::default();
        if let Some(path) = &file {
            match read_history(path) {
                Ok(mut paths) => {
                    paths.truncate(RECENT_LIMIT);
                    history.paths = paths;
                }
                Err(error) => history.error = Some(error),
            }
        } else {
            history.error = Some("No configuration directory for Maschine project history".into());
        }
        Self {
            file,
            history: Mutex::new(history),
        }
    }

    pub fn recent(&self) -> Vec<PathBuf> {
        self.history.lock().unwrap().paths.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.history.lock().unwrap().error.clone()
    }

    /// Call only after the engine has opened or saved the project successfully.
    pub fn remember(&self, path: &Path) -> Result<(), String> {
        // Relative sample references use the opened path's directory, including aliases.
        let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file = self
            .file
            .as_ref()
            .ok_or("No Maschine project history directory")?;
        let parent = file
            .parent()
            .ok_or("No Maschine project history directory")?;
        private_directory(parent)?;
        let lock_path = file.with_extension("json.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("{}: {e}", lock_path.display()))?;
        lock.lock()
            .map_err(|e| format!("{}: {e}", lock_path.display()))?;
        let mut paths = read_history(file)?;
        paths.retain(|p| p != &path);
        paths.insert(0, path);
        paths.truncate(RECENT_LIMIT);
        let bytes = serde_json::to_vec_pretty(&paths).map_err(|e| e.to_string())?;
        save_history(file, &bytes)?;
        let mut history = self.history.lock().unwrap();
        history.paths = paths;
        history.error = None;
        Ok(())
    }

    pub fn folder(&self) -> Result<PathBuf, String> {
        let parent = self
            .file
            .as_ref()
            .and_then(|p| p.parent())
            .ok_or("No directory for Maschine projects")?;
        let folder = parent.join("projects");
        private_directory(&folder)?;
        Ok(folder)
    }
}

fn read_history(path: &Path) -> Result<Vec<PathBuf>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("Invalid Maschine project history {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!(
            "Couldn't read Maschine project history {}: {e}",
            path.display()
        )),
    }
}

pub fn recent_name(path: &Path) -> String {
    path.file_stem()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Resolve against the list that was displayed, before the editor can reorder it.
pub fn recent_path(paths: &[PathBuf], index: usize) -> Result<PathBuf, String> {
    paths
        .get(index)
        .cloned()
        .ok_or_else(|| "The selected recent Maschine project is unavailable".into())
}

fn private_directory(folder: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(folder)
        .map_err(|e| format!("{}: {e}", folder.display()))
}

fn save_history(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("No Maschine project history directory")?;
    private_directory(parent)?;
    let temp = path.with_extension(format!("json.part-{}-{}", std::process::id(), nonce()));
    write_new(&temp, bytes)?;
    let result = std::fs::rename(&temp, path);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    Ok(())
}

pub fn fresh_path(folder: &Path, name: &str, extension: &str) -> PathBuf {
    let stem: String = name
        .chars()
        .take(48)
        .map(|c| {
            if c.is_alphanumeric() || " -_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if stem.trim().is_empty() {
        "Maschine"
    } else {
        stem.trim()
    };
    // A prefix also makes Windows device names safe in projects exchanged there.
    let upper = stem.to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'));
    let stem = if reserved {
        format!("Maschine {stem}")
    } else {
        stem.to_string()
    };
    for index in 1.. {
        let name = if index == 1 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem} {index}.{extension}")
        };
        let path = folder.join(name);
        if std::fs::symlink_metadata(&path).is_err()
            && std::fs::symlink_metadata(path.with_extension(format!("{extension}.saving")))
                .is_err()
        {
            return path;
        }
    }
    unreachable!()
}

/// Claim a new destination before save_as, which replaces its destination.
pub struct NewProject {
    pub path: PathBuf,
}

impl NewProject {
    pub fn reserve(folder: &Path, name: &str) -> Result<Self, String> {
        loop {
            let path = fresh_path(folder, name, "mxprj");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

impl Drop for NewProject {
    fn drop(&mut self) {
        // A failed save may leave just our empty reservation. Keep written data.
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!("saving-{}-{}", std::process::id(), nonce()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temp, path)
    })();
    drop(file);
    let _ = std::fs::remove_file(&temp);
    result.map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> PathBuf {
        let root = std::env::var_os("OPENDJ_PROJECT_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!("{name}-{}-{}", std::process::id(), nonce()));
        private_directory(&path).unwrap();
        path
    }

    #[test]
    fn history_roundtrips_and_load_uses_the_displayed_zero_based_order() {
        let root = directory("history");
        let file = root.join("recent.json");
        let projects = Projects::load(Some(file.clone()));
        let first = root.join("First.mxprj");
        let second = root.join("Second.mxprj");
        write_new(&first, b"first fixture").unwrap();
        write_new(&second, b"second fixture").unwrap();
        assert!(projects.recent().is_empty());
        projects.remember(&first).unwrap();
        projects.remember(&second).unwrap();
        let shown = projects.recent();
        assert_eq!(
            shown.iter().map(|p| recent_name(p)).collect::<Vec<_>>(),
            ["Second", "First"]
        );
        projects.remember(&first).unwrap();
        assert_eq!(recent_path(&shown, 0).unwrap(), second);
        assert_eq!(recent_path(&shown, 1).unwrap(), first);
        assert!(recent_path(&shown, 2).is_err());
        std::fs::remove_file(&second).unwrap();
        assert_eq!(recent_path(&shown, 0).unwrap(), second);
        assert!(!recent_path(&shown, 0).unwrap().is_file());
        let reopened = Projects::load(Some(file));
        assert_eq!(reopened.recent(), [first.clone(), second]);
        #[cfg(unix)]
        {
            let alias = root.join("Alias.mxprj");
            std::os::unix::fs::symlink(first, &alias).unwrap();
            reopened.remember(&alias).unwrap();
            assert_eq!(reopened.recent()[0], alias);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_or_unwritable_history_preserves_data_and_can_retry() {
        let root = directory("history-errors");
        let settings = root.join("settings.json");
        let legacy = br#"{"old_setting":true,"instruments_recent":["groove"]}"#;
        write_new(&settings, legacy).unwrap();
        let file = root.join("recent.json");
        write_new(&file, b"broken history").unwrap();
        let projects = Projects::load(Some(file.clone()));
        assert!(projects.error().is_some());
        let project = root.join("Track.mxprj");
        write_new(&project, b"fixture").unwrap();
        assert!(projects.remember(&project).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"broken history");
        assert!(projects.recent().is_empty());
        std::fs::write(&file, b"[]").unwrap();
        projects.remember(&project).unwrap();
        let before = projects.recent();
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(projects.remember(&project).is_err());
        assert_eq!(projects.recent(), before);
        assert!(file.is_dir());
        assert!(save_history(&file, b"[]").is_err());
        assert!(file.is_dir());
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("part-")));
        assert_eq!(std::fs::read(settings).unwrap(), legacy);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_writers_merge_history_and_keep_the_limit() {
        let root = directory("history-writers");
        let file = root.join("recent.json");
        let projects = std::sync::Arc::new(Projects::load(Some(file.clone())));
        let other = Projects::load(Some(file.clone()));
        let mut paths = Vec::new();
        for i in 0..RECENT_LIMIT + 2 {
            let path = root.join(format!("Project {i}.mxprj"));
            write_new(&path, b"fixture").unwrap();
            projects.remember(&path).unwrap();
            paths.push(path);
        }
        assert_eq!(projects.recent().len(), RECENT_LIMIT);
        let newest = paths.last().unwrap().clone();
        let worker = std::thread::spawn(move || other.remember(&newest).unwrap());
        projects.remember(&paths[0]).unwrap();
        worker.join().unwrap();
        let persisted = Projects::load(Some(file)).recent();
        assert_eq!(persisted.len(), RECENT_LIMIT);
        assert!(persisted.contains(&paths[0]));
        assert!(persisted.contains(paths.last().unwrap()));
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("saving")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_destinations_are_safe_unique_and_preserve_existing_files() {
        let root = directory("destinations");
        let projects = Projects::load(Some(root.join("maschine/recent-projects.json")));
        let private = projects.folder().unwrap();
        assert_eq!(private, root.join("maschine/projects"));
        let existing = root.join("Song.mxprj");
        write_new(&existing, b"keep project").unwrap();
        let saving = root.join("Song 2.mxprj.saving");
        write_new(&saving, b"keep unfinished save").unwrap();
        let first = NewProject::reserve(&root, "Song").unwrap();
        assert_eq!(first.path, root.join("Song 3.mxprj"));
        let empty = NewProject::reserve(&root, "Song").unwrap();
        assert_eq!(empty.path, root.join("Song 4.mxprj"));
        let empty_path = empty.path.clone();
        drop(empty);
        assert!(!empty_path.exists());
        std::fs::write(&first.path, b"completed fixture").unwrap();
        let completed = first.path.clone();
        drop(first);
        assert_eq!(std::fs::read(completed).unwrap(), b"completed fixture");
        assert!(write_new(&existing, b"replace").is_err());
        assert_eq!(std::fs::read(existing).unwrap(), b"keep project");
        assert_eq!(std::fs::read(saving).unwrap(), b"keep unfinished save");
        for name in ["../../escape", "", "NUL", "COM1", &"🎵".repeat(100)] {
            let path = fresh_path(&root, name, "mxprj");
            assert_eq!(path.parent(), Some(root.as_path()));
            assert!(path.file_name().unwrap().len() < 255);
            assert!(!["NUL", "COM1"].contains(&recent_name(&path).as_str()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
                0o700
            );
            std::os::unix::fs::symlink(root.join("absent"), root.join("Link.mxprj")).unwrap();
            let reserved = NewProject::reserve(&root, "Link").unwrap();
            assert_eq!(reserved.path, root.join("Link 2.mxprj"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
