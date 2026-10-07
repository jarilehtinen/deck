//! Playback state across restarts (`~/.local/state/deck/playback.json`): the playing
//! album or genre radio (genre, seeds and tracks), the track, the position and the queue.
//!
//! The state is saved on quit and whenever the track changes or the queue changes, so
//! that not even a crash loses it. When nothing is playing, the file is removed. Saving
//! is atomic (temp + rename) as with the shelf. A broken file is a load error: Deck
//! starts empty and the next save replaces the file. This is deliberate and differs
//! from the user's files (shelf, genres, Curated), which are never overwritten when
//! broken: the playback state is Deck's own state that nobody fixes by hand, and
//! losing it only loses the position on the album.
//!
//! The file lives in `~/.local/state` (the XDG state directory) and not in `~/.config`,
//! because it is not data the user put together like the shelf, nor in `~/.cache`,
//! because the cache may be cleared at any time.

use std::{fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};

use crate::{
    app::SavedPlayback,
    config,
    store::{read_json, save_json},
};

#[derive(Debug, Clone)]
pub struct PlaybackFile {
    path: PathBuf,
}

impl PlaybackFile {
    pub fn default_path() -> Result<Self> {
        Ok(Self::at(config::state_dir()?.join("playback.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Reads the saved playback state. A missing file is `None`.
    pub fn load(&self) -> Result<Option<SavedPlayback>> {
        read_json(&self.path, "playback")
    }

    /// Saves the playback state, or removes the file when nothing is playing (`None`).
    pub fn save(&self, playback: Option<&SavedPlayback>) -> Result<()> {
        match playback {
            Some(playback) => save_json(&self.path, playback),
            None => match fs::remove_file(&self.path) {
                Err(e) if e.kind() != ErrorKind::NotFound => {
                    Err(e).with_context(|| format!("cannot remove {}", self.path.display()))
                }
                _ => Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYBACK_JSON: &str = r#"{
  "album": { "uri": "spotify:album:nevermind", "len": 12, "index": 1 },
  "current": "album",
  "played": [],
  "queue": [
    { "uri": "spotify:track:breed", "name": "Breed", "artist": "Nirvana",
      "duration_ms": 183000, "album_uri": "spotify:album:nevermind", "index": 3 }
  ],
  "now_playing": { "artist": "Nirvana", "album": "Nevermind", "track": "In Bloom",
    "duration_ms": 254000 },
  "position_ms": 83000
}"#;

    #[test]
    fn missing_file_is_nothing_to_restore() {
        let dir = tempfile::tempdir().unwrap();
        let file = PlaybackFile::at(dir.path().join("deck/playback.json"));
        assert_eq!(file.load().unwrap(), None);
        // Saving an empty state does not require the file to exist.
        file.save(None).unwrap();
    }

    #[test]
    fn save_writes_atomically_and_nothing_playing_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck/playback.json");
        let file = PlaybackFile::at(path.clone());
        let saved: SavedPlayback = serde_json::from_str(PLAYBACK_JSON).unwrap();

        file.save(Some(&saved)).unwrap();
        assert_eq!(file.load().unwrap(), Some(saved));
        assert!(!dir.path().join("deck/playback.json.tmp").exists());

        file.save(None).unwrap();
        assert!(!path.exists());
        assert_eq!(file.load().unwrap(), None);
    }

    #[test]
    fn broken_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playback.json");
        let file = PlaybackFile::at(path.clone());
        for text in ["{", r#"{"current": "album"}"#] {
            fs::write(&path, text).unwrap();
            let error = format!("{:#}", file.load().unwrap_err());
            assert!(error.contains("playback file"), "{error}");
            assert!(error.contains("is broken"), "{error}");
        }
    }
}
