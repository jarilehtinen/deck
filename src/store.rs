//! Reading files, saving them atomically, and timestamps for caches.
//!
//! Saving first writes a temporary file next to the target and renames it over the
//! target, so a crash never leaves half a file. The temporary file name is unique per
//! process and call (`.<name>.<pid>.<n>.tmp`), so Deck and a `deck curate submit` running at
//! the same time never write to the same temporary file.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

/// Reads a JSON file. A missing file is `None`, a broken one is an error.
pub fn read_json<T: DeserializeOwned>(path: &Path, what: &str) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("{what} file {} is broken", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot open {}", path.display())),
    }
}

/// Saves JSON in a readable format, atomically ([`write_atomic`]).
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    write_atomic(path, json.as_bytes()).with_context(|| format!("cannot save {}", path.display()))
}

/// Writes a file atomically: the directory is created if needed, the contents are
/// written to a temporary file in the same directory, all the way to disk
/// (`sync_all`), and the file is renamed over the target. A failed write removes
/// the temporary file and leaves the target untouched.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = temp_path(path);
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Path of the temporary file next to the target: `.<name>.<pid>.<n>.tmp`.
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()))
}

/// The current time in Unix seconds, for cache timestamps.
pub fn unix_now() -> u64 {
    jiff::Timestamp::now()
        .as_second()
        .try_into()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn missing_file_is_none_and_broken_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        assert_eq!(read_json::<Vec<u8>>(&path, "test").unwrap(), None);

        fs::write(&path, "[1,").unwrap();
        let err = read_json::<Vec<u8>>(&path, "test").unwrap_err();
        assert!(format!("{err:#}").contains("test file"), "{err:#}");
        assert!(format!("{err:#}").contains("is broken"), "{err:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "[1,");
    }

    #[test]
    fn save_creates_dir_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck/x.json");
        save_json(&path, &vec![1, 2]).unwrap();
        save_json(&path, &vec![3]).unwrap();
        assert_eq!(read_json::<Vec<u8>>(&path, "test").unwrap(), Some(vec![3]));
        assert_eq!(fs::read_to_string(&path).unwrap(), "[\n  3\n]\n");
        assert_eq!(files_in(&dir.path().join("deck")), ["x.json"]);
    }

    #[test]
    fn temp_names_are_unique_and_hidden_next_to_target() {
        let path = Path::new("/tmp/deck/shelf.json");
        let a = temp_path(path);
        let b = temp_path(path);
        assert_ne!(a, b);
        assert_eq!(a.parent(), path.parent());
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".shelf.json."), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(
            name.contains(&format!(".{}.", std::process::id())),
            "{name}"
        );
    }

    #[test]
    fn failed_write_keeps_target_and_removes_temp() {
        let dir = tempfile::tempdir().unwrap();
        // A directory sits where the target should be, so the rename fails.
        let path = dir.path().join("x.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), "keep").unwrap();
        assert!(write_atomic(&path, b"[]").is_err());
        assert_eq!(fs::read_to_string(path.join("inside")).unwrap(), "keep");
        assert_eq!(files_in(dir.path()), ["x.json"]);
    }
}
