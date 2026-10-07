//! Sign-in and playback with librespot.
//!
//! `Player` takes commands and sends events through a channel. It plays one track at a
//! time: the play order (album and queue) is decided by `app`. When a track ends, the
//! player reports it (`Event::EndOfTrack`) and waits for the next command. For
//! preloading it asks for the next track (`Event::PreloadNext`), so that an album plays
//! without gaps. At startup, the previous session's track is loaded paused at the
//! saved position (`Command::Restore`).
//!
//! The album's track list is fetched from `catalog` for playback and kept in memory:
//! jumping to a track on the same album does not fetch the list again, and after the
//! queue the album continues from the same list.
//!
//! Spotify's AP server closes the connection three minutes after sign-in, and
//! librespot does not reconnect: a closed session no longer gets track keys, and
//! playback would stop at the next track. The player, the radio and the Web API share
//! one session through [`SharedSession`], which signs in again when the session is
//! closed, once for all of them. The player takes a fresh session before loading a
//! track if its own is closed. When resuming from pause on a closed session, the
//! track is reloaded at the same position, because a paused track gets no more
//! data.
//! The key request also has a tight timeout (1.5 s), so a failed track is retried once
//! before it is skipped.

use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use librespot_core::{
    Session, SessionConfig, SpotifyUri, authentication::Credentials, cache::Cache,
};
use librespot_metadata::audio::{UniqueFields, item::CoverImage};
use librespot_playback::{
    audio_backend::{self, SinkBuilder},
    config::{AudioFormat, Bitrate, PlayerConfig},
    mixer::NoOpVolume,
    player::{Player as Librespot, PlayerEvent, PlayerEventChannel},
};
use tokio::sync::{
    Mutex,
    mpsc::{self, UnboundedReceiver, UnboundedSender},
};

use crate::{catalog::Catalog, config};

/// librespot's sign-in is only used for playback: search and likes go through the Web
/// API with their own token (`catalog`).
const OAUTH_SCOPES: &[&str] = &["streaming"];

/// Signs in with cached credentials, or the first time through the browser (OAuth).
/// Credentials are saved in `~/.cache/deck/`, which only the owner can read.
pub async fn connect() -> Result<Session> {
    connect_with(true).await
}

/// Signs in again while the UI is running: only with cached credentials, because
/// opening the browser and writing to stderr would mess up the UI.
async fn reconnect() -> Result<Session> {
    connect_with(false).await
}

/// The Spotify session shared by the player, the radio and the Web API. Clones share
/// the session, so when the server closes it, it is reopened only once.
#[derive(Clone)]
pub struct SharedSession {
    session: Arc<Mutex<Session>>,
}

impl SharedSession {
    pub fn new(session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
        }
    }

    /// An open session: a closed one is reopened with cached credentials
    /// ([`reconnect`]), never through the browser.
    pub async fn get(&self) -> Result<Session> {
        let mut session = self.session.lock().await;
        if session.is_invalid() {
            log::warn!("the Spotify connection was closed, signing in again");
            *session = reconnect().await?;
        }
        Ok(session.clone())
    }
}

async fn connect_with(sign_in: bool) -> Result<Session> {
    // librespot creates the directory and `credentials.json` with default permissions
    // (readable by everyone), so the directory is restricted to the owner first.
    let cache_dir = config::private_cache_dir()?;
    let cache = Cache::new(Some(&cache_dir), None, None, None)
        .with_context(|| format!("cannot open cache {}", cache_dir.display()))?;
    let config = SessionConfig::default();

    let credentials = match cache.credentials() {
        Some(credentials) => credentials,
        None if !sign_in => {
            anyhow::bail!("Spotify credentials are missing – restart Deck to sign in")
        }
        None => {
            eprintln!("Sign in to Spotify in the browser that opens…");
            let client = config::oauth_client(&config.client_id, OAUTH_SCOPES)?;
            let token = client
                .get_access_token_async()
                .await
                .context("Spotify sign-in failed")?;
            Credentials::with_access_token(token.access_token)
        }
    };

    let session = Session::new(config, Some(cache));
    session
        .connect(credentials, true)
        .await
        .context("could not connect to Spotify")?;
    restrict_credentials(&cache_dir.join("credentials.json"));
    Ok(session)
}

/// Restricts the credentials saved by librespot to the owner (0600). librespot rewrites
/// the file with `File::create`, which keeps the permissions of an existing file.
/// A failure is only logged: the directory is already 0700.
fn restrict_credentials(path: &Path) {
    if !path.exists() {
        return;
    }
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        log::warn!("cannot restrict {}: {e}", path.display());
    }
}

/// A track to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Track `track` (0 is the first) of the album (URI as `spotify:album:<id>`).
    Album { uri: String, track: usize },
    /// A single track (URI as `spotify:track:<id>`), e.g. from the queue.
    Track(String),
}

#[derive(Debug)]
pub enum Command {
    Play(Item),
    /// Preloads the next track after `Event::PreloadNext`.
    Preload(Item),
    /// Loads a track paused at `position`: `TogglePause` resumes from there.
    Restore {
        item: Item,
        position: Duration,
    },
    TogglePause,
    Stop,
}

#[derive(Debug, Clone)]
pub enum Event {
    TrackChanged {
        /// The track URI (`spotify:track:<id>`).
        uri: String,
        artist: String,
        album: String,
        track: String,
        duration: Duration,
        /// Cover image URL for Now Playing.
        cover: Option<String>,
    },
    Position(u32),
    /// Playback is paused at this position (ms).
    Paused(u32),
    /// The track ended or was skipped because it could not be played: the player waits
    /// for the next command.
    EndOfTrack,
    /// The next track can be preloaded (`Command::Preload`).
    PreloadNext,
    /// Playback stopped for the player's own reasons (signing in again failed).
    Stopped,
    Error(String),
}

#[derive(Clone)]
pub struct Player {
    commands: UnboundedSender<Command>,
}

impl Player {
    pub fn spawn(
        shared: SharedSession,
        session: Session,
        catalog: Catalog,
        bitrate: config::Bitrate,
    ) -> (Self, UnboundedReceiver<Event>) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();

        let backend = audio_backend::find(None).expect("no audio backend compiled in");
        let librespot = new_librespot(session.clone(), backend, bitrate);
        let player_events = librespot.get_player_event_channel();

        let worker = Worker {
            shared,
            session,
            backend,
            bitrate,
            librespot,
            player_events,
            catalog,
            events,
            album: String::new(),
            tracks: Vec::new(),
            current: None,
            start: Start::BEGINNING,
            paused: None,
            retried: false,
        };
        tokio::spawn(worker.run(command_rx));
        (Self { commands }, event_rx)
    }

    pub fn send(&self, command: Command) {
        // Sending fails only if the worker has already ended, i.e. the program is exiting.
        let _ = self.commands.send(command);
    }
}

fn new_librespot(
    session: Session,
    backend: SinkBuilder,
    bitrate: config::Bitrate,
) -> Arc<Librespot> {
    let config = PlayerConfig {
        bitrate: match bitrate {
            config::Bitrate::Kbps96 => Bitrate::Bitrate96,
            config::Bitrate::Kbps160 => Bitrate::Bitrate160,
            config::Bitrate::Kbps320 => Bitrate::Bitrate320,
        },
        position_update_interval: Some(Duration::from_millis(500)),
        ..PlayerConfig::default()
    };
    Librespot::new(config, session, Box::new(NoOpVolume), move || {
        backend(None, AudioFormat::default())
    })
}

struct Worker {
    shared: SharedSession,
    /// The session of `librespot`: the player is tied to it.
    session: Session,
    backend: SinkBuilder,
    /// Audio quality, also for the player created when signing in again.
    bitrate: config::Bitrate,
    librespot: Arc<Librespot>,
    /// Replaced together with `librespot` when reconnecting.
    player_events: PlayerEventChannel,
    catalog: Catalog,
    events: UnboundedSender<Event>,
    /// URI of the album played last, and its track URIs for librespot.
    album: String,
    tracks: Vec<SpotifyUri>,
    /// The playing track; `None` when nothing is playing.
    current: Option<SpotifyUri>,
    /// How the playing track was loaded: a retry loads it the same way.
    start: Start,
    /// The pause position (ms); `None` when the track is playing.
    paused: Option<u32>,
    /// The playing track has already been retried once.
    retried: bool,
}

impl Worker {
    async fn run(mut self, mut commands: UnboundedReceiver<Command>) {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                Some(event) = self.player_events.recv() => self.player_event(event).await,
            }
        }
        self.librespot.stop();
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::Play(item) => match self.resolve(&item).await {
                Ok(track) => self.load(track, Start::BEGINNING).await,
                Err(e) => self.emit(Event::Error(format!("{e:#}"))),
            },
            Command::Restore { item, position } => match self.resolve(&item).await {
                Ok(track) => {
                    let position = u32::try_from(position.as_millis()).unwrap_or(u32::MAX);
                    self.load(
                        track,
                        Start {
                            play: false,
                            position,
                        },
                    )
                    .await;
                }
                Err(e) => self.emit(Event::Error(format!("{e:#}"))),
            },
            // On a closed session the preload would fail; the track is loaded in its
            // turn after reconnecting.
            Command::Preload(item) if !self.session.is_invalid() => {
                if let Some(track) = self.cached(&item) {
                    self.librespot.preload(track);
                }
            }
            Command::TogglePause if self.current.is_some() => match self.paused {
                // A session closed by the server gets no more track data (see the module
                // docs): the track is reloaded at the same position, and `load` signs in
                // again.
                Some(position) if self.session.is_invalid() => {
                    if let Some(track) = self.current.clone() {
                        self.load(
                            track,
                            Start {
                                play: true,
                                position,
                            },
                        )
                        .await;
                    }
                }
                Some(_) => self.librespot.play(),
                None => self.librespot.pause(),
            },
            Command::Stop => self.stop(),
            _ => {}
        }
    }

    /// The track URI for librespot. The album's tracks are fetched if needed.
    async fn resolve(&mut self, item: &Item) -> Result<SpotifyUri> {
        match item {
            Item::Album { uri, track } => {
                self.open_album(uri).await?;
                Ok(self.tracks[(*track).min(self.tracks.len() - 1)].clone())
            }
            Item::Track(uri) => {
                SpotifyUri::from_uri(uri).map_err(|e| anyhow!("invalid track URI: {e}"))
            }
        }
    }

    /// The track URI without the network: for album tracks, only from the album in memory.
    fn cached(&self, item: &Item) -> Option<SpotifyUri> {
        match item {
            Item::Album { uri, track } if *uri == self.album => self.tracks.get(*track).cloned(),
            Item::Album { .. } => None,
            Item::Track(uri) => SpotifyUri::from_uri(uri).ok(),
        }
    }

    /// Fetches the album's tracks for playback. A list already in memory is used as
    /// is. On failure, the playing track keeps playing.
    async fn open_album(&mut self, uri: &str) -> Result<()> {
        if self.album == uri && !self.tracks.is_empty() {
            return Ok(());
        }
        let id = uri
            .strip_prefix("spotify:album:")
            .ok_or_else(|| anyhow!("not an album URI: {uri}"))?;
        let tracks = self
            .catalog
            .album_tracks(id)
            .await?
            .iter()
            .map(|t| SpotifyUri::from_uri(&t.uri))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("invalid track URI: {e}"))?;
        if tracks.is_empty() {
            return Err(anyhow!("the album has no tracks"));
        }
        self.album = uri.to_owned();
        self.tracks = tracks;
        Ok(())
    }

    async fn load(&mut self, track: SpotifyUri, start: Start) {
        if let Err(e) = self.reconnect_if_closed().await {
            self.stop();
            self.emit(Event::Error(format!("{e:#}")));
            self.emit(Event::Stopped);
            return;
        }
        self.current = Some(track.clone());
        self.start = start;
        self.retried = false;
        self.paused = (!start.play).then_some(start.position);
        self.librespot.load(track, start.play, start.position);
    }

    /// Replaces the librespot player if the server has closed its session. The new
    /// session comes from the shared one, which the radio or the Web API may already
    /// have reopened. The player is tied to its session, so it is created anew.
    async fn reconnect_if_closed(&mut self) -> Result<()> {
        if !self.session.is_invalid() {
            return Ok(());
        }
        let session = self.shared.get().await?;
        self.librespot.stop();
        self.librespot = new_librespot(session.clone(), self.backend, self.bitrate);
        self.player_events = self.librespot.get_player_event_channel();
        self.session = session;
        Ok(())
    }

    async fn player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::TrackChanged { audio_item } => {
                let (artist, album) = match &audio_item.unique_fields {
                    UniqueFields::Track { artists, album, .. } => (
                        artists
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        album.clone(),
                    ),
                    _ => (String::new(), String::new()),
                };
                self.emit(Event::TrackChanged {
                    uri: audio_item.track_id.to_uri().unwrap_or_default(),
                    artist,
                    album,
                    track: audio_item.name.clone(),
                    duration: Duration::from_millis(audio_item.duration_ms.into()),
                    cover: cover_url(&audio_item.covers),
                });
            }
            PlayerEvent::Playing { position_ms, .. } => {
                self.paused = None;
                self.emit(Event::Position(position_ms));
            }
            PlayerEvent::PositionChanged { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. } => {
                self.emit(Event::Position(position_ms));
            }
            PlayerEvent::Paused { position_ms, .. } => {
                self.paused = Some(position_ms);
                self.emit(Event::Paused(position_ms));
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => self.emit(Event::PreloadNext),
            // The track URI is not compared with the playing one: librespot may replace
            // an unavailable track with an alternative.
            PlayerEvent::EndOfTrack { .. } => {
                self.current = None;
                self.emit(Event::EndOfTrack);
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                match on_unavailable(self.current.as_ref(), &track_id, self.retried) {
                    Unavailable::Ignore => {}
                    Unavailable::Retry => {
                        if let Some(track) = self.current.clone() {
                            self.load(track, self.start).await;
                            self.retried = true;
                        }
                    }
                    Unavailable::Skip => {
                        self.current = None;
                        self.emit(Event::Error(format!("track not available: {track_id}")));
                        self.emit(Event::EndOfTrack);
                    }
                }
            }
            _ => {}
        }
    }

    /// Stops playback. The album's track list stays in memory.
    fn stop(&mut self) {
        self.current = None;
        self.librespot.stop();
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// The smallest cover image of at least 300 pixels (Now Playing shows about 224 pixels),
/// or the largest if all are smaller. librespot orders the images largest first.
fn cover_url(covers: &[CoverImage]) -> Option<String> {
    covers
        .iter()
        .rfind(|cover| cover.width >= 300)
        .or(covers.first())
        .map(|cover| cover.url.clone())
}

/// How a track is loaded: playing or paused, and from which position (ms).
#[derive(Debug, Clone, Copy)]
struct Start {
    play: bool,
    position: u32,
}

impl Start {
    /// Playing from the beginning.
    const BEGINNING: Self = Self {
        play: true,
        position: 0,
    };
}

#[derive(Debug, PartialEq)]
enum Unavailable {
    /// The notice is about a track other than the playing one: librespot also reports a
    /// failed preload this way while the previous track is still playing.
    Ignore,
    /// The first failure: the cause may be a closed session or the key request
    /// timeout, so the track is reloaded (`load` reconnects if needed).
    Retry,
    /// The retry failed too: playback continues from the next track.
    Skip,
}

fn on_unavailable(current: Option<&SpotifyUri>, track: &SpotifyUri, retried: bool) -> Unavailable {
    if current != Some(track) {
        Unavailable::Ignore
    } else if retried {
        Unavailable::Skip
    } else {
        Unavailable::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> SpotifyUri {
        SpotifyUri::from_uri(&format!("spotify:track:{id}")).unwrap()
    }

    #[test]
    fn failed_track_is_retried_once() {
        let current = track("6hSkmGq0LBhq9XxvM9NYhK");
        assert_eq!(
            on_unavailable(Some(&current), &current, false),
            Unavailable::Retry
        );
    }

    #[test]
    fn track_still_unavailable_after_retry_is_skipped() {
        let current = track("6hSkmGq0LBhq9XxvM9NYhK");
        assert_eq!(
            on_unavailable(Some(&current), &current, true),
            Unavailable::Skip
        );
    }

    #[test]
    fn failed_preload_does_not_skip_the_playing_track() {
        let current = track("6hSkmGq0LBhq9XxvM9NYhK");
        let next = track("3lWiW8EUBPJpyUHQLPjbyE");
        assert_eq!(
            on_unavailable(Some(&current), &next, false),
            Unavailable::Ignore
        );
        assert_eq!(on_unavailable(None, &next, true), Unavailable::Ignore);
    }
}
