//! macOS Now Playing and the media keys (⏯ ⏮ ⏭).
//!
//! macOS does not send media keys to the terminal but to the Now Playing player, so
//! Deck registers itself as a player (`MPRemoteCommandCenter` and
//! `MPNowPlayingInfoCenter`). Commands arrive through the main thread's event loop
//! (CFRunLoop), and Deck's main thread runs tokio, so Now Playing is a helper process of
//! its own: Deck starts itself with `deck now-playing`, writes the playing track's info
//! to its stdin as JSON lines (`Option<Info>`, `null` clears) and reads the keys from
//! its stdout as lines (`Command`). The helper exits when its stdin closes.
//!
//! macOS gives the keys to the player that most recently started playing. If Spotify or
//! Music plays after Deck, the keys move to it, and they come back to Deck when Deck
//! starts playing again. While paused, Deck keeps the keys, so ⏯ resumes playback.
//!
//! The info is sent only when something other than the position changes, or the
//! position differs from the predicted one (`DRIFT`): macOS works out the position
//! itself from the playback rate.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// How far the position may drift from the predicted one before it is sent again.
const DRIFT: Duration = Duration::from_secs(2);

/// A failed cover image download is retried no sooner than this (with the next
/// update), so that while the network is down, not every update waits for the
/// download timeout.
const COVER_RETRY: Duration = Duration::from_secs(30);

/// The playing track's info for Now Playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub position_ms: u64,
    pub playing: bool,
    /// Cover image URL (`https://i.scdn.co/image/…`).
    pub cover: Option<String>,
}

/// A media key or a Control Center button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Play,
    Pause,
    Next,
    Previous,
}

impl Command {
    const ALL: [Self; 5] = [
        Self::Toggle,
        Self::Play,
        Self::Pause,
        Self::Next,
        Self::Previous,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Play => "play",
            Self::Pause => "pause",
            Self::Next => "next",
            Self::Previous => "previous",
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == line.trim())
    }
}

/// Deck's side: the helper process that is told the playing track and sends the keys.
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The info sent last, and when it was sent.
    sent: Option<(Option<Info>, Instant)>,
}

impl Session {
    /// Starts the helper process (`deck now-playing`). Keys are read in a thread of
    /// their own and passed to `on_command`.
    pub fn start(on_command: impl Fn(Command) + Send + 'static) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot find the deck executable")?;
        let mut child = std::process::Command::new(exe)
            .arg("now-playing")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot start now-playing")?;
        let stdout = child.stdout.take().context("now-playing has no stdout")?;
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(command) = Command::parse(&line) {
                    on_command(command);
                }
            }
        });
        Ok(Self {
            stdin: child.stdin.take(),
            child,
            sent: None,
        })
    }

    /// Reports the playing track (`None`: nothing is playing) if it has changed.
    pub fn update(&mut self, info: Option<Info>, now: Instant) {
        if !changed(self.sent.as_ref(), info.as_ref(), now) {
            return;
        }
        let Some(stdin) = &mut self.stdin else { return };
        let written = serde_json::to_string(&info)
            .map_err(anyhow::Error::from)
            .and_then(|line| Ok(writeln!(stdin, "{line}")?));
        if let Err(e) = written {
            // The helper crashed: Deck works without Now Playing.
            log::warn!("now-playing stopped: {e:#}");
            self.stdin = None;
        }
        self.sent = Some((info, now));
    }
}

impl Drop for Session {
    /// Stops the helper process, which clears Now Playing. Killed directly rather than
    /// by closing stdin, because a cover image download may delay reading.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the info must be sent: something other than the position changed, or the
/// position differs from the one predicted from the sent info by more than `DRIFT`
/// (e.g. ← to the start of the same track).
fn changed(sent: Option<&(Option<Info>, Instant)>, info: Option<&Info>, now: Instant) -> bool {
    let Some((sent, at)) = sent else {
        return info.is_some();
    };
    let (sent, info) = match (sent, info) {
        (None, None) => return false,
        (Some(sent), Some(info)) => (sent, info),
        _ => return true,
    };
    let unpositioned = |info: &Info| Info {
        position_ms: 0,
        ..info.clone()
    };
    if unpositioned(sent) != unpositioned(info) {
        return true;
    }
    let mut expected = Duration::from_millis(sent.position_ms);
    if sent.playing {
        expected += now.saturating_duration_since(*at);
    }
    let expected = expected.min(Duration::from_millis(sent.duration_ms));
    expected.abs_diff(Duration::from_millis(info.position_ms)) > DRIFT
}

/// Whether to download the cover image `url`: it is not already loaded (`loaded`),
/// and its download did not fail just now (`failed`, see [`COVER_RETRY`]).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn should_download(
    url: &str,
    loaded: Option<&str>,
    failed: Option<&(String, Instant)>,
    now: Instant,
) -> bool {
    if loaded == Some(url) {
        return false;
    }
    match failed {
        Some((failed_url, at)) if failed_url == url => {
            now.saturating_duration_since(*at) >= COVER_RETRY
        }
        _ => true,
    }
}

#[cfg(target_os = "macos")]
pub use mac::serve;

#[cfg(target_os = "macos")]
mod mac {
    use std::{
        cell::RefCell,
        io::{BufRead, Write},
        ptr::NonNull,
        time::Instant,
    };

    use anyhow::{Context, Result};
    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::{AllocAnyThread, MainThreadMarker, rc::Retained, runtime::AnyObject};
    use objc2_app_kit::NSImage;
    use objc2_core_foundation::{CFRunLoop, CGSize};
    use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString, NSURL};
    use objc2_media_player::{
        MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
        MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
        MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
        MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommandCenter,
        MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
    };

    use super::{Command, Info, should_download};

    thread_local! {
        /// The latest cover image with its URL (main thread only).
        static ARTWORK: RefCell<Option<(String, Retained<MPMediaItemArtwork>)>> =
            const { RefCell::new(None) };
    }

    /// `deck now-playing`: registers the keys and runs the event loop on the main
    /// thread. Info is read in a thread of its own and applied on the main thread.
    pub fn serve() -> Result<()> {
        MainThreadMarker::new().context("now-playing must run on the main thread")?;
        register_commands();
        std::thread::spawn(read_updates);
        CFRunLoop::run();
        Ok(())
    }

    fn register_commands() {
        // SAFETY: MediaPlayer calls on the main thread; the commands live until exit.
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            let commands = [
                (center.togglePlayPauseCommand(), Command::Toggle),
                (center.playCommand(), Command::Play),
                (center.pauseCommand(), Command::Pause),
                (center.nextTrackCommand(), Command::Next),
                (center.previousTrackCommand(), Command::Previous),
            ];
            for (remote, command) in commands {
                let handler = RcBlock::new(move |_: NonNull<MPRemoteCommandEvent>| {
                    send(command);
                    MPRemoteCommandHandlerStatus::Success
                });
                remote.setEnabled(true);
                remote.addTargetWithHandler(&handler);
            }
        }
    }

    /// Writes the key to Deck. If the write fails, Deck is gone.
    fn send(command: Command) {
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{}", command.name())
            .and_then(|()| out.flush())
            .is_err()
        {
            std::process::exit(0);
        }
    }

    /// Reads the info from stdin. A new cover image is downloaded in this thread, so
    /// the main thread never waits for the network. A failed download is retried with
    /// a later update.
    fn read_updates() {
        // URL of the downloaded cover image, and the last failed download.
        let mut cover_url: Option<String> = None;
        let mut failed: Option<(String, Instant)> = None;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let info: Option<Info> = match serde_json::from_str(&line) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("now-playing: invalid update: {e}");
                    continue;
                }
            };
            let url = info.as_ref().and_then(|info| info.cover.clone());
            let image = match url {
                Some(url)
                    if should_download(
                        &url,
                        cover_url.as_deref(),
                        failed.as_ref(),
                        Instant::now(),
                    ) =>
                {
                    match download(&url) {
                        Some(bytes) => {
                            cover_url = Some(url.clone());
                            failed = None;
                            Some((url, bytes))
                        }
                        None => {
                            failed = Some((url, Instant::now()));
                            None
                        }
                    }
                }
                _ => None,
            };
            DispatchQueue::main().exec_async(move || publish(info.as_ref(), image));
        }
        // Deck exited or started a new helper process.
        std::process::exit(0);
    }

    fn download(url: &str) -> Option<Vec<u8>> {
        let url = NSURL::URLWithString(&NSString::from_str(url))?;
        NSData::dataWithContentsOfURL(&url).map(|data| data.to_vec())
    }

    fn publish(info: Option<&Info>, image: Option<(String, Vec<u8>)>) {
        // SAFETY: called on the main thread; the keys and values are MediaPlayer types.
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            let Some(info) = info else {
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                return;
            };
            if let Some((url, bytes)) = image {
                ARTWORK.set(artwork(&bytes).map(|artwork| (url, artwork)));
            }
            let dict = NSMutableDictionary::<NSString, AnyObject>::new();
            let text = |key: &NSString, value: &str| {
                dict.insert(key, &*NSString::from_str(value));
            };
            text(MPMediaItemPropertyTitle, &info.title);
            text(MPMediaItemPropertyArtist, &info.artist);
            text(MPMediaItemPropertyAlbumTitle, &info.album);
            let number = |key: &NSString, value: f64| {
                dict.insert(key, &*NSNumber::new_f64(value));
            };
            number(
                MPMediaItemPropertyPlaybackDuration,
                info.duration_ms as f64 / 1000.0,
            );
            number(
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                info.position_ms as f64 / 1000.0,
            );
            number(
                MPNowPlayingInfoPropertyPlaybackRate,
                if info.playing { 1.0 } else { 0.0 },
            );
            ARTWORK.with_borrow(|artwork| {
                if let Some((url, artwork)) = artwork
                    && info.cover.as_ref() == Some(url)
                {
                    dict.insert(MPMediaItemPropertyArtwork, &**artwork);
                }
            });
            center.setNowPlayingInfo(Some(&dict));
            center.setPlaybackState(if info.playing {
                MPNowPlayingPlaybackState::Playing
            } else {
                MPNowPlayingPlaybackState::Paused
            });
        }
    }

    fn artwork(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
        let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes))?;
        let size = image.size();
        let handler = RcBlock::new(move |_: CGSize| NonNull::from(&*image));
        // SAFETY: the block returns a live image, which the block itself keeps alive.
        Some(unsafe {
            MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                MPMediaItemArtwork::alloc(),
                size,
                &handler,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(position_ms: u64, playing: bool) -> Info {
        Info {
            title: "Syndicate".into(),
            artist: "The Fall".into(),
            album: "Hex".into(),
            duration_ms: 200_000,
            position_ms,
            playing,
            cover: None,
        }
    }

    #[test]
    fn first_info_is_sent_but_empty_is_not() {
        let now = Instant::now();
        assert!(changed(None, Some(&info(0, true)), now));
        assert!(!changed(None, None, now));
    }

    #[test]
    fn playing_progress_is_not_resent() {
        let at = Instant::now();
        let sent = (Some(info(10_000, true)), at);
        let now = at + Duration::from_secs(30);
        assert!(!changed(Some(&sent), Some(&info(40_000, true)), now));
        // At the end of the track, the position stops at the duration.
        let late = at + Duration::from_secs(300);
        assert!(!changed(Some(&sent), Some(&info(200_000, true)), late));
    }

    #[test]
    fn pause_track_change_and_jump_are_sent() {
        let at = Instant::now();
        let sent = (Some(info(10_000, true)), at);
        let now = at + Duration::from_secs(5);
        assert!(changed(Some(&sent), Some(&info(15_000, false)), now));
        let other = Info {
            title: "Hey! Luciferase".into(),
            ..info(15_000, true)
        };
        assert!(changed(Some(&sent), Some(&other), now));
        // ← to the start of the same track.
        assert!(changed(Some(&sent), Some(&info(0, true)), now));
        assert!(changed(Some(&sent), None, now));
    }

    #[test]
    fn position_does_not_advance_when_paused() {
        let at = Instant::now();
        let sent = (Some(info(10_000, false)), at);
        let now = at + Duration::from_secs(60);
        assert!(!changed(Some(&sent), Some(&info(10_000, false)), now));
    }

    #[test]
    fn button_name_parses_back_to_command() {
        for command in Command::ALL {
            assert_eq!(
                Command::parse(&format!("{}\n", command.name())),
                Some(command)
            );
        }
        assert_eq!(Command::parse("seek"), None);
    }

    #[test]
    fn cover_is_downloaded_once_and_retried_after_a_failure() {
        let now = Instant::now();
        let url = "https://i.scdn.co/image/a";
        assert!(should_download(url, None, None, now));
        assert!(should_download(
            url,
            Some("https://i.scdn.co/image/b"),
            None,
            now
        ));
        assert!(!should_download(url, Some(url), None, now));
        // A failed download is retried only after a while.
        let failed = (url.to_owned(), now);
        assert!(!should_download(url, None, Some(&failed), now));
        assert!(should_download(url, None, Some(&failed), now + COVER_RETRY));
        // Another track's cover is downloaded right away.
        assert!(should_download(
            "https://i.scdn.co/image/b",
            None,
            Some(&failed),
            now
        ));
    }
}
