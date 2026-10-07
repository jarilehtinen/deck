//! Spotify's 429 block shared by all Deck processes (`~/.cache/deck/rate-limit.json`).
//!
//! After too many Web API requests Spotify blocks all of the app's requests for hours.
//! Deck, `deck curate` and `deck genre add` use the same app, so a block that one of
//! them meets holds for the others too: [`crate::catalog::Catalog`] checks the file
//! before every request and records a 429 in it, and the subcommands check it before
//! their first search. The block is kept in memory as well, so it holds even if the
//! file cannot be written.
//!
//! The file is Deck's own state: a broken one is logged and replaced.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    config,
    store::{read_json, save_json},
};

#[derive(Debug, Default)]
pub struct RateLimit {
    /// `None` keeps the block in memory only.
    path: Option<PathBuf>,
    /// The latest block this process knows of (Unix seconds).
    until: Mutex<Option<u64>>,
}

#[derive(Serialize, Deserialize)]
struct RateLimitFile {
    blocked_until: u64,
}

impl RateLimit {
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::cache_dir()?.join("rate-limit.json"))
    }

    /// The block shared through the file at `path`.
    pub fn at(path: &Path) -> Self {
        Self {
            path: Some(path.to_owned()),
            until: Mutex::new(None),
        }
    }

    /// Until when (Unix seconds) requests are blocked, if they are at `now`.
    pub fn blocked_until(&self, now: u64) -> Option<u64> {
        let until = (*self.lock()).max(self.read());
        until.filter(|&until| until > now)
    }

    /// Blocks requests until `until` (Unix seconds). A later block already recorded,
    /// by this or another process, is kept. A failed save is only logged: the block
    /// still holds in this process.
    pub fn block(&self, until: u64) {
        let until = {
            let mut memory = self.lock();
            let until = (*memory).max(self.read()).max(Some(until));
            *memory = until;
            until
        };
        let (Some(path), Some(blocked_until)) = (&self.path, until) else {
            return;
        };
        if let Err(e) = save_json(path, &RateLimitFile { blocked_until }) {
            log::warn!("{e:#}");
        }
    }

    /// The block in the file. A missing file is no block, and a broken one is logged
    /// and treated as none: the next block replaces it.
    fn read(&self) -> Option<u64> {
        let path = self.path.as_ref()?;
        match read_json::<RateLimitFile>(path, "rate limit") {
            Ok(file) => file.map(|f| f.blocked_until),
            Err(e) => {
                log::warn!("{e:#}");
                None
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.until.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn block_holds_until_the_given_time() {
        let limit = RateLimit::default();
        assert_eq!(limit.blocked_until(NOW), None);
        limit.block(NOW + 120);
        assert_eq!(limit.blocked_until(NOW), Some(NOW + 120));
        assert_eq!(limit.blocked_until(NOW + 119), Some(NOW + 120));
        assert_eq!(limit.blocked_until(NOW + 120), None);
    }

    #[test]
    fn a_shorter_block_does_not_shorten_a_longer_one() {
        let limit = RateLimit::default();
        limit.block(NOW + 600);
        limit.block(NOW + 10);
        assert_eq!(limit.blocked_until(NOW + 300), Some(NOW + 600));
    }

    #[test]
    fn block_is_shared_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rate-limit.json");
        let deck = RateLimit::at(&path);
        let curate = RateLimit::at(&path);
        deck.block(NOW + 600);
        assert_eq!(curate.blocked_until(NOW), Some(NOW + 600));

        // The other process keeps the later block when it records its own.
        curate.block(NOW + 60);
        assert_eq!(RateLimit::at(&path).blocked_until(NOW), Some(NOW + 600));
        curate.block(NOW + 3600);
        assert_eq!(deck.blocked_until(NOW), Some(NOW + 3600));
    }

    #[test]
    fn broken_file_is_no_block_and_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rate-limit.json");
        std::fs::write(&path, "{\"blocked_until\": ").unwrap();
        let limit = RateLimit::at(&path);
        assert_eq!(limit.blocked_until(NOW), None);
        limit.block(NOW + 60);
        assert_eq!(RateLimit::at(&path).blocked_until(NOW), Some(NOW + 60));
    }

    #[test]
    fn unwritable_file_keeps_the_block_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        // A directory sits where the file should be, so saving fails.
        let path = dir.path().join("rate-limit.json");
        std::fs::create_dir(&path).unwrap();
        let limit = RateLimit::at(&path);
        limit.block(NOW + 60);
        assert_eq!(limit.blocked_until(NOW), Some(NOW + 60));
    }
}
