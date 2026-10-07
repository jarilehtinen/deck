//! Application state and key logic: view stack, selections, search text, now playing,
//! play order (album and queue) and errors.
//!
//! Play order works the Spotify way: the queue plays after the current track, and once
//! the queue is empty the album continues where it left off. The player plays one track
//! at a time and reports when it ends; `app` picks the next one.
//!
//! Playback state (album, track, position and queue) survives restarts
//! ([`SavedPlayback`]): the event loop saves it on quit and whenever
//! [`State::playback_changed`] reports a change, and on startup `Event::Restore`
//! loads the track paused at the saved position.
//!
//! `State::update` is pure: it touches neither the terminal, the network nor files.
//! It changes the state and returns actions (`Action`) as data. The event loop
//! carries them out and reports the result back as an event (e.g. `Event::Searched`,
//! `Event::ShelfChanged`). Time is a parameter so that the search delay is testable.
//!
//! Above the shelf is a group of lists: Curated, favourite genres and Genres. ↑/↓
//! moves over the list rows and artists as one list. The lists are reread
//! (`Action::LoadLists`) when the shelf comes into view or a list view is opened.
//!
//! A genre plays as a radio: the radio is a playback context like an album, and its
//! track list lives only here (`RadioPosition`). The `radio` module fetches pages
//! (`Action::LoadRadio`), and the result comes back as `Event::RadioLoaded`, from which
//! only tracks not yet in the radio are taken. A refill starts when fewer than
//! `RADIO_REFILL` tracks remain after the playing one, and when the station no longer
//! brings anything new (fewer than `RADIO_FRESH_MIN`), the radio moves on to the next
//! unplayed seed.
//!
//! Mac media keys arrive as `Event::Media` (`now_playing`) and work like Space and
//! ← / → in any view. `State::now_playing_info` reports the playing track to Now
//! Playing.
//!
//! Ctrl+N (n outside search) opens the album or radio of the playing track
//! (`State::open_playing`): that is why album details travel in the playback state.
//!
//! Ctrl+L likes the playing track or removes the like (Liked Songs). Likes live in the
//! Web API, whose limits are strict, so their state is queried in batches
//! (`Action::CheckLiked`, at most `LIKED_BATCH` tracks) when a track list or a radio
//! page arrives, and kept in memory for the session (`Likes`). Liking and unliking
//! update the memory without a new query. Each track is queried at most once, and a
//! failed query stops all queries for the rest of the session.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    catalog::{Album, Artist, LIKED_BATCH, SearchResults, Track},
    genres::{self, Genre},
    lists::AlbumList,
    now_playing::{self, Command as MediaCommand},
    player::{self, Item},
    radio,
    shelf::{albums_of, artists_of, sort_by_year},
};

/// A search starts once typing has paused for this long.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Ctrl+L without your own Spotify app.
const LIKES_NEED_APP: &str =
    "liking songs needs your own Spotify app: set client_id in ~/.config/deck/config.toml";

/// How long the confirmation for queueing and liking stays visible.
pub const NOTICE_TIME: Duration = Duration::from_secs(3);

/// A radio refill starts when fewer tracks than this remain after the playing one.
const RADIO_REFILL: usize = 5;
/// If a refill brings fewer new tracks than this, the station has started repeating itself
/// and the radio switches seeds. In testing a station repeated itself after about 300
/// tracks, and pages could then bring only a couple of new ones at a time for a long while.
const RADIO_FRESH_MIN: usize = 10;
/// The radio keeps at most this many played tracks before the playing one: otherwise the
/// track list would grow for the whole session, and every save would copy it.
const RADIO_HISTORY: usize = 500;

/// A key, independent of the terminal. `Ctrl` letters are lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Backspace,
}

#[derive(Debug)]
pub enum Event {
    Key(Key),
    /// A Mac media key or a Control Center button: works like Space and ← / →
    /// regardless of the view (in search too).
    Media(MediaCommand),
    /// Time passing: fires the debounced search. The loop must send this more often
    /// than `SEARCH_DEBOUNCE`.
    Tick,
    Player(player::Event),
    /// Result of `Action::Search`.
    Searched {
        query: String,
        result: Result<SearchResults, String>,
    },
    /// Result of `Action::LoadAlbumTracks`.
    AlbumTracks {
        uri: String,
        result: Result<Vec<Track>, String>,
    },
    /// Result of `Action::QueueAlbum`: the album's tracks for the queue.
    AlbumQueued {
        album: Album,
        result: Result<Vec<Track>, String>,
    },
    /// Result of `Action::LoadArtistAlbums`.
    ArtistAlbums {
        artist_id: String,
        result: Result<Vec<Album>, String>,
    },
    /// The shelf's albums after a successful add or removal.
    ShelfChanged(Vec<Album>),
    /// Result of `Action::LoadLists`: the Curated list, `None` when there is no file.
    ListsLoaded(Result<Option<AlbumList>, String>),
    /// The Curated list after a successful rejection (`Action::RejectCurated`).
    CuratedChanged(Option<AlbumList>),
    /// The previous session's playback state, read at startup.
    Restore(Box<SavedPlayback>),
    /// Result of `Action::LoadRadio`: the station's page as is.
    RadioLoaded {
        request: RadioRequest,
        result: Result<Vec<radio::Track>, String>,
    },
    /// Result of `Action::AddRadioAlbumToShelf`: the album with its year, for the shelf.
    RadioAlbum(Result<Album, String>),
    /// Favourite genres (ids) after a successful add or removal.
    FavoritesChanged(Vec<String>),
    /// Result of `Action::CheckLiked`: liked or not, in the same order.
    LikedChecked {
        uris: Vec<String>,
        result: Result<Vec<bool>, String>,
    },
    /// Result of `Action::SetLiked`.
    LikeChanged {
        uri: String,
        liked: bool,
        result: Result<(), String>,
    },
    /// An action failed (e.g. saving the shelf).
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Play(Item),
    Preload(Item),
    /// Load the track paused at `position` (restored playback state).
    Restore {
        item: Item,
        position: Duration,
    },
    TogglePause,
    Stop,
    Search(String),
    /// Fetch all albums of the artist (id) from Spotify.
    LoadArtistAlbums(String),
    /// Fetch the album's (URI) tracks for the track list.
    LoadAlbumTracks(String),
    /// Fetch the album's tracks for the queue.
    QueueAlbum(Album),
    AddToShelf(Album),
    /// Remove the album (URI) from the shelf.
    RemoveFromShelf(String),
    /// Reread the lists (`curated.json`).
    LoadLists,
    /// Reject the album (URI) from Curated: it leaves the list and is recorded in history.
    RejectCurated(String),
    /// Fetch a page from the radio station.
    LoadRadio(RadioRequest),
    /// Fetch the year and artist of a radio track's album (URI) for adding it to the shelf
    /// (also in the track list, if the album has no year).
    AddRadioAlbumToShelf(String),
    /// Add the genre (id) to favourites, i.e. to the home page.
    AddFavorite(String),
    RemoveFavorite(String),
    /// Ask whether the tracks (URIs, at most `LIKED_BATCH`) are liked.
    CheckLiked(Vec<String>),
    /// Save the track (URI) to Liked Songs or remove it from there.
    SetLiked {
        uri: String,
        liked: bool,
    },
    Quit,
}

/// A radio fetch. The result is valid only if it matches the pending request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioRequest {
    /// The genre id.
    pub genre: String,
    /// The seed URI.
    pub seed: String,
    /// Tracks (URIs) already received from the seed's station, for the refill.
    pub previous: Vec<String>,
    pub kind: RadioLoad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioLoad {
    /// A new station (Enter or Ctrl+R): replaces the context and plays the first track.
    Station,
    /// A refill for the playing radio.
    More,
}

/// Every view has a list whose scroll position (`Scroll`) is part of the view's state:
/// when you return to the view, the list is where you left it.
#[derive(Debug)]
pub enum View {
    /// The selection moves over the list rows and artists: lists first, then artists.
    Shelf {
        selected: usize,
        scroll: Scroll,
    },
    /// The Curated list. The albums are in the state (`State::curated`).
    Curated {
        selected: usize,
        scroll: Scroll,
    },
    Artist(ArtistView),
    Search(SearchView),
    Album(AlbumView),
    Queue {
        selected: usize,
        scroll: Scroll,
    },
    /// Genres: every genre in the catalog.
    Genres {
        selected: usize,
        scroll: Scroll,
    },
    Radio(RadioView),
}

/// A list's scroll position: the topmost visible row. The list only scrolls when the
/// selection would go past the visible area, so the position depends on the previous draw
/// and the height of the area. That is why the drawing (`ui`) updates it, not
/// `State::update`, and why it is a `Cell` in the `&State` the drawing gets.
#[derive(Debug, Default)]
pub struct Scroll(Cell<usize>);

impl Scroll {
    pub fn get(&self) -> usize {
        self.0.get()
    }

    pub fn set(&self, offset: usize) {
        self.0.set(offset);
    }
}

/// A genre's radio. The tracks are in the playback context (`State::radio_tracks`) if the
/// genre's radio is playing.
#[derive(Debug)]
pub struct RadioView {
    pub genre: &'static Genre,
    pub selected: usize,
    pub scroll: Scroll,
}

/// A list row above the shelf.
#[derive(Debug, PartialEq, Eq)]
pub enum ShelfRow<'a> {
    Curated(&'a AlbumList),
    Genre(&'static Genre),
    Genres,
}

#[derive(Debug)]
pub struct ArtistView {
    pub artist: Artist,
    pub source: Source,
    pub selected: usize,
    pub scroll: Scroll,
}

#[derive(Debug)]
pub enum Source {
    /// The artist's albums on the shelf by year; updated when the shelf changes.
    Shelf(Vec<Album>),
    /// All of the artist's albums from Spotify; `None` while loading.
    Spotify(Option<Vec<Album>>),
}

/// An album's tracks. Opens with Enter on an album; playback starts only with Enter
/// on a track.
#[derive(Debug)]
pub struct AlbumView {
    pub album: Album,
    /// `None` while the tracks are still being fetched.
    pub tracks: Option<Vec<Track>>,
    pub selected: usize,
    pub scroll: Scroll,
    /// Opened with n when the playing track's position is unknown (queued from a radio):
    /// the selection moves to this track (URI) when the tracks arrive.
    select_uri: Option<String>,
}

#[derive(Debug, Default)]
pub struct SearchView {
    pub query: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub results: Option<SearchResults>,
    /// Selection in the result list: artists first, then albums.
    pub selected: usize,
    pub scroll: Scroll,
    due: Option<Instant>,
    /// The last search sent; only its results are accepted.
    requested: Option<String>,
    pending: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SearchItem<'a> {
    Artist(&'a Artist),
    Album(&'a Album),
}

/// The playing track as reported by the player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    /// The track's URI; empty if unknown (old saved playback state).
    pub uri: String,
    pub artist: String,
    pub album: String,
    pub track: String,
    pub duration: Duration,
    /// Cover image URL for Now Playing.
    pub cover: Option<String>,
    pub paused: bool,
    position: Duration,
    position_at: Instant,
}

/// A track in the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedTrack {
    pub uri: String,
    pub name: String,
    pub artist: String,
    #[serde(rename = "duration_ms", with = "millis")]
    pub duration: Duration,
    /// The album (URI) and the track's position in its track list: the playing track is
    /// marked in the list even when played from the queue. A radio track's position is
    /// unknown.
    album_uri: String,
    index: Option<usize>,
    /// Album details for the n track list; `None` in old saved state.
    /// A radio track's album has no year, and the artist is the track's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album: Option<Box<Album>>,
}

/// Play order: what plays now and what plays next.
#[derive(Debug, Default)]
struct Playback {
    current: Option<Current>,
    /// The playing album; when the queue plays, the album continues after it.
    album: Option<AlbumPosition>,
    /// The playing radio instead of an album: at most one of them at a time.
    radio: Option<RadioPosition>,
    /// Tracks already played in this queue round, for the ← key. Emptied when the album
    /// continues.
    played: Vec<QueuedTrack>,
    queue: Vec<QueuedTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Current {
    /// Album track `AlbumPosition::index`.
    Album,
    /// Radio track `RadioPosition::index`.
    Radio,
    Queued(QueuedTrack),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AlbumPosition {
    uri: String,
    /// Number of tracks on the album.
    len: usize,
    /// The playing track, or the last one played while the queue plays.
    index: usize,
    /// Album details for the n track list; `None` in old saved state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album: Option<Album>,
}

/// The playing genre radio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RadioPosition {
    /// The genre's id in the catalog.
    genre: String,
    /// The current seed (URI).
    seed: String,
    /// Seeds played in this radio, including the current one.
    seeds: Vec<String>,
    /// The first track received from the current seed's station: the refill sends it and
    /// the later ones to the station.
    seed_start: usize,
    /// Tracks in the order they arrived from the station, from all seeds.
    tracks: Vec<radio::Track>,
    /// The playing track, or the last one played while the queue plays.
    index: usize,
}

/// Radio fetch state. Not saved.
#[derive(Debug, Default)]
struct RadioFetch {
    /// The pending fetch; only its result is accepted.
    pending: Option<RadioRequest>,
    /// A refill failed while this track (index) was playing: retry when the next one
    /// starts.
    failed_at: Option<usize>,
    /// Consecutive refills that brought nothing. Once every seed has been tried, refills
    /// stop and the remaining tracks play out.
    empty: usize,
}

/// Playback state that survives restarts (`playback` module): the play order, the
/// playing track's details for the status line, and the position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPlayback {
    album: Option<AlbumPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    radio: Option<RadioPosition>,
    current: Current,
    #[serde(default)]
    played: Vec<QueuedTrack>,
    #[serde(default)]
    queue: Vec<QueuedTrack>,
    /// `None` if the player had not yet reported the track's details.
    now_playing: Option<SavedTrack>,
    #[serde(rename = "position_ms", with = "millis")]
    position: Duration,
}

/// The playing track's details for the status line, even before the player has loaded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SavedTrack {
    #[serde(default)]
    uri: String,
    artist: String,
    album: String,
    track: String,
    #[serde(rename = "duration_ms", with = "millis")]
    duration: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover: Option<String>,
}

/// Duration in the file in milliseconds.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(super::millis(*duration))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

/// A brief confirmation: what was done, and the name of the track or album.
#[derive(Debug)]
struct Notice {
    kind: NoticeKind,
    name: String,
    until: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Queued,
    Liked,
    Unliked,
}

/// Liked tracks for the session. Not saved.
#[derive(Debug, Default)]
struct Likes {
    /// Likes need your own Spotify app (`Catalog::can_like`).
    enabled: bool,
    /// Known state by track URI.
    known: HashMap<String, bool>,
    /// URIs asked about, including pending and failed ones: they are not asked again.
    asked: HashSet<String>,
    /// A query failed (e.g. 429): no new queries are made during the session.
    stopped: bool,
    /// Pending saves and removals: URI → track name for the confirmation.
    changing: HashMap<String, String>,
}

impl Likes {
    /// Asks about the tracks whose state is neither known nor asked, `LIKED_BATCH`
    /// at a time.
    fn check<'a>(&mut self, uris: impl IntoIterator<Item = &'a str>) -> Vec<Action> {
        if !self.enabled || self.stopped {
            return Vec::new();
        }
        let mut new: Vec<String> = Vec::new();
        for uri in uris {
            // `insert` is false if the URI was already asked or is already in this batch.
            if uri.starts_with("spotify:track:")
                && !self.known.contains_key(uri)
                && self.asked.insert(uri.to_owned())
            {
                new.push(uri.to_owned());
            }
        }
        new.chunks(LIKED_BATCH)
            .map(|uris| Action::CheckLiked(uris.to_vec()))
            .collect()
    }
}

#[derive(Debug)]
pub struct State {
    /// The view stack, never empty: the shelf is always at the bottom.
    views: Vec<View>,
    shelf: Vec<Album>,
    /// The shelf's artists in order (`artists_of`) and album URIs: drawing asks for them on
    /// every frame, so they are only computed when the shelf changes (`set_shelf`).
    shelf_artists: Vec<Artist>,
    shelf_uris: HashSet<String>,
    /// The Curated list; `None` when there is no file or it is broken.
    curated: Option<AlbumList>,
    /// Ids of the favourite genres.
    favorites: Vec<String>,
    /// The error is from a broken list file: a successful load clears it.
    lists_broken: bool,
    pub now_playing: Option<NowPlaying>,
    playback: Playback,
    /// Confirmation for queueing or liking.
    notice: Option<Notice>,
    /// A restored track is loading: if that fails, the state is cleared. Loading is done
    /// when the player reports the position (`Paused` or `Position`).
    restoring: bool,
    /// The last saved playback state, without position (`playback_changed`).
    saved: Option<SavedPlayback>,
    /// An event that may change the playback state has been handled since the previous call
    /// to `playback_changed`: only then is the state built and compared.
    playback_dirty: bool,
    radio_fetch: RadioFetch,
    likes: Likes,
    /// State of the seed picker (xorshift). Tests can set it.
    rng: u64,
    /// Shown until the next action succeeds.
    pub error: Option<String>,
}

impl State {
    pub fn new(shelf: Vec<Album>) -> Self {
        let mut state = Self {
            views: vec![View::Shelf {
                selected: 0,
                scroll: Scroll::default(),
            }],
            shelf: Vec::new(),
            shelf_artists: Vec::new(),
            shelf_uris: HashSet::new(),
            curated: None,
            favorites: Vec::new(),
            lists_broken: false,
            now_playing: None,
            playback: Playback::default(),
            notice: None,
            restoring: false,
            saved: None,
            playback_dirty: false,
            radio_fetch: RadioFetch::default(),
            likes: Likes::default(),
            rng: genres::random() | 1,
            error: None,
        };
        state.set_shelf(shelf);
        state
    }

    /// Favourite genres read at startup.
    pub fn with_favorites(mut self, ids: Vec<String>) -> Self {
        self.set_lists(|state| state.favorites = ids);
        self
    }

    /// Enables likes: they need your own Spotify app.
    pub fn with_likes(mut self, enabled: bool) -> Self {
        self.likes.enabled = enabled;
        self
    }

    /// The Curated list read at startup. The selection starts on the top row.
    pub fn with_curated(mut self, result: Result<Option<AlbumList>, String>) -> Self {
        self.lists_loaded(result);
        if let Some(View::Shelf { selected, .. }) = self.views.first_mut() {
            *selected = 0;
        }
        self
    }

    /// The Curated list for the list view, even when empty.
    pub fn curated(&self) -> Option<&AlbumList> {
        self.curated.as_ref()
    }

    /// The album's (URI) reason, if the album is on the current Curated list.
    pub fn curated_reason(&self, album_uri: &str) -> Option<&str> {
        self.curated
            .as_ref()?
            .albums
            .iter()
            .find(|entry| entry.album.uri == album_uri)?
            .reason
            .as_deref()
    }

    /// The list rows shown above the shelf: Curated (if it has albums), favourite genres
    /// in alphabetical order, and always Genres last.
    pub fn shelf_lists(&self) -> Vec<ShelfRow<'_>> {
        self.curated
            .iter()
            .filter(|list| !list.albums.is_empty())
            .map(ShelfRow::Curated)
            .chain(self.favorite_genres().into_iter().map(ShelfRow::Genre))
            .chain([ShelfRow::Genres])
            .collect()
    }

    pub fn favorite(&self, id: &str) -> bool {
        self.favorites.iter().any(|f| f == id)
    }

    /// Favourite genres from the catalog by name. Unknown ids are skipped.
    pub fn favorite_genres(&self) -> Vec<&'static Genre> {
        genres::all()
            .iter()
            .filter(|g| self.favorite(&g.id))
            .collect()
    }

    /// The playing radio's genre (id), also when paused and while the queue plays.
    pub fn playing_genre(&self) -> Option<&str> {
        self.playback.current.as_ref()?;
        self.playback
            .radio
            .as_ref()
            .map(|radio| radio.genre.as_str())
    }

    /// The genre's radio tracks, if it is the playback context.
    pub fn radio_tracks(&self, genre: &str) -> Option<&[radio::Track]> {
        self.playback
            .radio
            .as_ref()
            .filter(|radio| radio.genre == genre)
            .map(|radio| radio.tracks.as_slice())
    }

    /// The playing radio track's position in the genre's radio (not when from the queue).
    pub fn playing_radio_track(&self, genre: &str) -> Option<usize> {
        self.now_playing.as_ref()?;
        let radio = self.playback.radio.as_ref()?;
        (self.playback.current == Some(Current::Radio) && radio.genre == genre)
            .then_some(radio.index)
    }

    /// The shelf's selectable rows: lists and artists.
    fn shelf_rows(&self) -> usize {
        self.shelf_lists().len() + self.shelf_artists.len()
    }

    /// The queue's tracks in play order (the playing track is not included).
    pub fn queue(&self) -> &[QueuedTrack] {
        &self.playback.queue
    }

    /// The latest confirmation and its name, until `NOTICE_TIME` has passed.
    pub fn notice(&self, now: Instant) -> Option<(NoticeKind, &str)> {
        self.notice
            .as_ref()
            .filter(|notice| now < notice.until)
            .map(|notice| (notice.kind, notice.name.as_str()))
    }

    /// Whether the track (URI) is liked. An unknown state is `false`.
    pub fn liked(&self, uri: &str) -> bool {
        self.likes.known.get(uri).copied().unwrap_or(false)
    }

    /// The visible view.
    pub fn view(&self) -> &View {
        self.views.last().expect("the view stack is never empty")
    }

    /// The shelf's artists in alphabetical order.
    pub fn shelf_artists(&self) -> &[Artist] {
        &self.shelf_artists
    }

    /// Whether the album (URI) is on the shelf.
    pub fn on_shelf(&self, uri: &str) -> bool {
        self.shelf_uris.contains(uri)
    }

    /// Replaces the shelf's albums and computes the data derived from them.
    fn set_shelf(&mut self, albums: Vec<Album>) {
        self.shelf_artists = artists_of(&albums);
        self.shelf_uris = albums.iter().map(|album| album.uri.clone()).collect();
        self.shelf = albums;
    }

    /// The playing track's position in the album's (URI) track list, if it plays from the
    /// album or from the queue.
    pub fn playing_track(&self, album_uri: &str) -> Option<usize> {
        self.now_playing.as_ref()?;
        let playback = &self.playback;
        match playback.current.as_ref()? {
            Current::Album => playback
                .album
                .as_ref()
                .filter(|album| album.uri == album_uri)
                .map(|album| album.index),
            Current::Radio => None,
            Current::Queued(track) => track.index.filter(|_| track.album_uri == album_uri),
        }
    }

    pub fn update(&mut self, event: Event, now: Instant) -> Vec<Action> {
        // Time passing, position and pause do not change the state to save (the position is
        // saved on quit). Tick arrives ten times a second, so after it
        // `playback_changed` does not touch the state at all.
        if !matches!(
            event,
            Event::Tick | Event::Player(player::Event::Position(_) | player::Event::Paused(_))
        ) {
            self.playback_dirty = true;
        }
        let actions = match event {
            Event::Key(key) => self.key(key, now),
            Event::Media(command) => self.media(command),
            Event::Tick => self.tick(now),
            Event::Player(event) => self.player(event, now),
            Event::Searched { query, result } => {
                self.searched(query, result);
                Vec::new()
            }
            Event::AlbumTracks { uri, result } => self.album_tracks(&uri, result),
            Event::AlbumQueued { album, result } => self.album_queued(&album, result, now),
            Event::ArtistAlbums { artist_id, result } => {
                self.artist_albums(&artist_id, result);
                Vec::new()
            }
            Event::ShelfChanged(albums) => {
                self.shelf_changed(albums);
                Vec::new()
            }
            Event::ListsLoaded(result) => {
                self.lists_loaded(result);
                Vec::new()
            }
            Event::CuratedChanged(list) => {
                self.error = None;
                self.set_curated(list);
                Vec::new()
            }
            Event::Restore(saved) => self.restore(*saved, now),
            Event::RadioLoaded { request, result } => self.radio_loaded(request, result),
            Event::RadioAlbum(Ok(album)) if !self.on_shelf(&album.uri) => {
                vec![Action::AddToShelf(album)]
            }
            Event::RadioAlbum(Ok(_)) => Vec::new(),
            Event::RadioAlbum(Err(message)) => {
                self.error = Some(message);
                Vec::new()
            }
            Event::FavoritesChanged(ids) => {
                self.error = None;
                self.set_lists(|state| state.favorites = ids);
                Vec::new()
            }
            Event::LikedChecked { uris, result } => {
                self.liked_checked(uris, result);
                Vec::new()
            }
            Event::LikeChanged { uri, liked, result } => {
                self.like_changed(uri, liked, result, now);
                Vec::new()
            }
            Event::Error(message) => {
                self.error = Some(message);
                Vec::new()
            }
        };
        let mut actions = actions;
        actions.extend(self.radio_refill());
        // The user played something else: the restored track's load no longer matters.
        if actions.iter().any(|a| matches!(a, Action::Play(_))) {
            self.restoring = false;
        }
        // Playback also shrinks the queue: the queue view's selection stays in the list.
        let len = self.playback.queue.len();
        for view in &mut self.views {
            if let View::Queue { selected, .. } = view {
                clamp(selected, len);
            }
        }
        actions
    }

    /// Playback state to save, with position; `None` when nothing is playing.
    pub fn playback(&self, now: Instant) -> Option<SavedPlayback> {
        let mut saved = self.unpositioned_playback()?;
        if let Some(playing) = &self.now_playing {
            saved.position = playing.elapsed(now);
        }
        Some(saved)
    }

    /// Whether the playback state has changed since the last call (or the restore), apart
    /// from the position: the track changed, the queue changed or playback ended. It is
    /// saved then, so that not even a crash loses the state. The event loop calls this
    /// after every event, so the state is only built if some event may have changed it
    /// (`playback_dirty`).
    pub fn playback_changed(&mut self) -> bool {
        if !std::mem::take(&mut self.playback_dirty) {
            return false;
        }
        let playback = self.unpositioned_playback();
        if playback == self.saved {
            return false;
        }
        self.saved = playback;
        true
    }

    fn unpositioned_playback(&self) -> Option<SavedPlayback> {
        let playback = &self.playback;
        Some(SavedPlayback {
            album: playback.album.clone(),
            radio: playback.radio.clone(),
            current: playback.current.clone()?,
            played: playback.played.clone(),
            queue: playback.queue.clone(),
            now_playing: self.now_playing.as_ref().map(|playing| SavedTrack {
                uri: playing.uri.clone(),
                artist: playing.artist.clone(),
                album: playing.album.clone(),
                track: playing.track.clone(),
                duration: playing.duration,
                cover: playing.cover.clone(),
            }),
            position: Duration::ZERO,
        })
    }

    /// Restores the previous session's playback state: the track shows on the status line
    /// right away, paused at the saved position, and the player loads it at that position.
    fn restore(&mut self, saved: SavedPlayback, now: Instant) -> Vec<Action> {
        let item = match &saved.current {
            Current::Album => saved
                .album
                .as_ref()
                .filter(|album| album.index < album.len)
                .map(|album| Item::Album {
                    uri: album.uri.clone(),
                    track: album.index,
                }),
            Current::Radio => saved
                .radio
                .as_ref()
                .and_then(|radio| radio.tracks.get(radio.index))
                .map(|track| Item::Track(track.uri.clone())),
            Current::Queued(track) => Some(Item::Track(track.uri.clone())),
        };
        let Some(item) = item else {
            self.error = Some("could not restore playback: the saved state is invalid".into());
            return Vec::new();
        };
        let SavedPlayback {
            album,
            radio,
            current,
            played,
            queue,
            now_playing,
            position,
        } = saved;
        self.now_playing = now_playing.map(|track| NowPlaying {
            uri: track.uri,
            artist: track.artist,
            album: track.album,
            track: track.track,
            duration: track.duration,
            cover: track.cover,
            paused: true,
            position: position.min(track.duration),
            position_at: now,
        });
        self.playback = Playback {
            current: Some(current),
            album,
            radio,
            played,
            queue,
        };
        self.restoring = true;
        self.radio_fetch = RadioFetch::default();
        self.saved = self.unpositioned_playback();
        let mut actions = vec![Action::Restore { item, position }];
        // The playing and upcoming tracks. Radio tracks already played are not asked about,
        // so that a long radio does not take many queries.
        let playing = self.now_playing.iter().map(|playing| playing.uri.as_str());
        let queue = self.playback.queue.iter().map(|track| track.uri.as_str());
        let radio = self
            .playback
            .radio
            .iter()
            .flat_map(|radio| radio.tracks.iter().skip(radio.index))
            .map(|track| track.uri.as_str());
        actions.extend(self.likes.check(playing.chain(queue).chain(radio)));
        actions
    }

    /// The playing track for Now Playing; `None` when nothing is playing.
    pub fn now_playing_info(&self, now: Instant) -> Option<now_playing::Info> {
        let playing = self.now_playing.as_ref()?;
        Some(now_playing::Info {
            title: playing.track.clone(),
            artist: playing.artist.clone(),
            album: playing.album.clone(),
            duration_ms: millis(playing.duration),
            position_ms: millis(playing.elapsed(now)),
            playing: !playing.paused,
            cover: playing.cover.clone(),
        })
    }

    fn media(&mut self, command: MediaCommand) -> Vec<Action> {
        let paused = self.now_playing.as_ref().map(|playing| playing.paused);
        match command {
            MediaCommand::Toggle => vec![Action::TogglePause],
            MediaCommand::Play if paused == Some(true) => vec![Action::TogglePause],
            MediaCommand::Pause if paused == Some(false) => vec![Action::TogglePause],
            MediaCommand::Play | MediaCommand::Pause => Vec::new(),
            MediaCommand::Next => self.next().into_iter().collect(),
            MediaCommand::Previous => self.previous().into_iter().collect(),
        }
    }

    fn key(&mut self, key: Key, now: Instant) -> Vec<Action> {
        // Outside search, Ctrl keys also work as plain letters. The queue's letter
        // is u, because q quits.
        let search = matches!(self.view(), View::Search(_));
        let key = match key {
            Key::Char(c @ ('f' | 'l' | 'e' | 'a' | 'd' | 'r' | 'n')) if !search => Key::Ctrl(c),
            Key::Char('u') if !search => Key::Ctrl('q'),
            key => key,
        };
        match key {
            Key::Ctrl('c') => return vec![Action::Quit],
            Key::Ctrl('f') => {
                self.open_search();
                return Vec::new();
            }
            Key::Ctrl('q') => {
                self.open_queue();
                return Vec::new();
            }
            Key::Ctrl('l') => return self.toggle_like(),
            Key::Ctrl('n') => return self.open_playing(),
            Key::Esc => {
                if self.views.len() > 1 {
                    self.views.pop();
                    // The shelf came into view: a new list shows up without a restart.
                    if let View::Shelf { .. } = self.view() {
                        return vec![Action::LoadLists];
                    }
                }
                return Vec::new();
            }
            _ => {}
        }
        if let View::Search(_) = self.view() {
            return self.search_key(key, now);
        }
        match key {
            Key::Char(' ') => vec![Action::TogglePause],
            Key::Left => self.previous().into_iter().collect(),
            Key::Right => self.next().into_iter().collect(),
            Key::Char('q') => vec![Action::Quit],
            _ => match self.view() {
                View::Shelf { .. } => self.shelf_key(key),
                View::Curated { .. } => self.curated_key(key),
                View::Artist(_) => self.artist_key(key),
                View::Album(_) => self.album_key(key, now),
                View::Queue { .. } => self.queue_key(key),
                View::Genres { .. } => self.genres_key(key),
                View::Radio(_) => self.radio_key(key, now),
                View::Search(_) => Vec::new(),
            },
        }
    }

    /// Plays an album (`len` tracks) from track `track`. The queue is kept and plays after
    /// this track.
    fn play_album(&mut self, album: Album, len: usize, track: usize) -> Vec<Action> {
        let uri = album.uri.clone();
        // An album replaces the radio, including a pending fetch.
        self.radio_fetch = RadioFetch::default();
        let playback = &mut self.playback;
        playback.current = Some(Current::Album);
        playback.played.clear();
        playback.radio = None;
        playback.album = Some(AlbumPosition {
            uri: uri.clone(),
            len,
            index: track,
            album: Some(album),
        });
        vec![Action::Play(Item::Album { uri, track })]
    }

    /// Plays the radio from track `index`. The queue is kept and plays after this track.
    fn play_radio(&mut self, index: usize) -> Vec<Action> {
        let playback = &mut self.playback;
        let Some(radio) = &mut playback.radio else {
            return Vec::new();
        };
        let Some(track) = radio.tracks.get(index) else {
            return Vec::new();
        };
        radio.index = index;
        let item = Item::Track(track.uri.clone());
        playback.current = Some(Current::Radio);
        playback.played.clear();
        vec![Action::Play(item)]
    }

    /// Where the next track comes from: the queue first, then the album or radio where it
    /// left off. `None` if nothing is playing or there is nothing left to play.
    fn upcoming_source(&self) -> Option<Upcoming> {
        let playback = &self.playback;
        playback.current.as_ref()?;
        if !playback.queue.is_empty() {
            return Some(Upcoming::Queue);
        }
        if let Some(album) = &playback.album {
            return (album.index + 1 < album.len).then_some(Upcoming::Album(album.index + 1));
        }
        let radio = playback.radio.as_ref()?;
        (radio.index + 1 < radio.tracks.len()).then_some(Upcoming::Radio(radio.index + 1))
    }

    fn upcoming(&self) -> Option<Item> {
        let playback = &self.playback;
        Some(match self.upcoming_source()? {
            Upcoming::Queue => Item::Track(playback.queue[0].uri.clone()),
            Upcoming::Album(track) => Item::Album {
                uri: playback.album.as_ref()?.uri.clone(),
                track,
            },
            Upcoming::Radio(index) => {
                Item::Track(playback.radio.as_ref()?.tracks[index].uri.clone())
            }
        })
    }

    /// → and the end of a track: moves to the next one (`upcoming`).
    fn next(&mut self) -> Option<Action> {
        let source = self.upcoming_source()?;
        let item = self.upcoming()?;
        let playback = &mut self.playback;
        match source {
            Upcoming::Queue => {
                let track = playback.queue.remove(0);
                if let Some(Current::Queued(previous)) = playback.current.take() {
                    playback.played.push(previous);
                }
                playback.current = Some(Current::Queued(track));
            }
            Upcoming::Album(track) => {
                if let Some(album) = &mut playback.album {
                    album.index = track;
                }
                playback.played.clear();
                playback.current = Some(Current::Album);
            }
            Upcoming::Radio(index) => {
                if let Some(radio) = &mut playback.radio {
                    radio.index = index;
                }
                playback.played.clear();
                playback.current = Some(Current::Radio);
            }
        }
        Some(Action::Play(item))
    }

    /// ←: on an album or radio, the previous track (the first one starts over). From a
    /// queue track, goes back to the previous queue track, or to the album or radio track
    /// after which the queue started, and the track goes back to the front of the queue.
    fn previous(&mut self) -> Option<Action> {
        let playback = &mut self.playback;
        // The playback state changes only once the previous track has been found.
        let item = match playback.current.as_ref()? {
            Current::Album => {
                let album = playback.album.as_mut()?;
                album.index = album.index.saturating_sub(1);
                Item::Album {
                    uri: album.uri.clone(),
                    track: album.index,
                }
            }
            Current::Radio => {
                let radio = playback.radio.as_mut()?;
                let index = radio.index.saturating_sub(1);
                let item = Item::Track(radio.tracks.get(index)?.uri.clone());
                radio.index = index;
                item
            }
            Current::Queued(_) => {
                // Every branch below sets the playing track again.
                let Some(Current::Queued(track)) = playback.current.take() else {
                    return None;
                };
                if let Some(previous) = playback.played.pop() {
                    playback.queue.insert(0, track);
                    let item = Item::Track(previous.uri.clone());
                    playback.current = Some(Current::Queued(previous));
                    item
                } else if let Some(album) = &playback.album {
                    playback.queue.insert(0, track);
                    playback.current = Some(Current::Album);
                    Item::Album {
                        uri: album.uri.clone(),
                        track: album.index,
                    }
                } else if let Some(radio_track) = playback
                    .radio
                    .as_ref()
                    .and_then(|radio| radio.tracks.get(radio.index))
                {
                    let item = Item::Track(radio_track.uri.clone());
                    playback.queue.insert(0, track);
                    playback.current = Some(Current::Radio);
                    item
                } else {
                    // The queue started from nothing: the first track starts over.
                    let item = Item::Track(track.uri.clone());
                    playback.current = Some(Current::Queued(track));
                    item
                }
            }
        };
        Some(Action::Play(item))
    }

    /// Adds the tracks to the end of the queue and shows a confirmation. If nothing is
    /// playing, the queue starts playing.
    fn enqueue(&mut self, tracks: Vec<QueuedTrack>, name: &str, now: Instant) -> Vec<Action> {
        if tracks.is_empty() {
            return Vec::new();
        }
        self.playback.queue.extend(tracks);
        self.notice = Some(Notice {
            kind: NoticeKind::Queued,
            name: name.to_owned(),
            until: now + NOTICE_TIME,
        });
        if self.playback.current.is_some() {
            return Vec::new();
        }
        let track = self.playback.queue.remove(0);
        let action = Action::Play(Item::Track(track.uri.clone()));
        self.playback.current = Some(Current::Queued(track));
        vec![action]
    }

    /// Playback ended (album and queue played, or the player stopped).
    fn stopped(&mut self) {
        let playback = &mut self.playback;
        playback.current = None;
        playback.album = None;
        playback.radio = None;
        playback.played.clear();
        self.now_playing = None;
    }

    /// Opens the album's track list without playing it. The tracks arrive as
    /// `Event::AlbumTracks`.
    fn open_album(&mut self, album: Album) -> Vec<Action> {
        let action = Action::LoadAlbumTracks(album.uri.clone());
        self.views.push(View::Album(AlbumView {
            album,
            tracks: None,
            selected: 0,
            scroll: Scroll::default(),
            select_uri: None,
        }));
        vec![action]
    }

    /// Where n leads: the playing track's album or radio. `None` when nothing is playing or
    /// the album details are missing from a restored old state.
    fn playing_target(&self) -> Option<PlayingTarget> {
        let playback = &self.playback;
        // An album from old saved state can still be found on the shelf.
        let album = |album: Option<&Album>, uri: &str| {
            album
                .or_else(|| self.shelf.iter().find(|a| a.uri == uri))
                .cloned()
        };
        match playback.current.as_ref()? {
            Current::Album => {
                let position = playback.album.as_ref()?;
                Some(PlayingTarget::Album {
                    album: album(position.album.as_ref(), &position.uri)?,
                    index: Some(position.index),
                    uri: String::new(),
                })
            }
            Current::Radio => {
                let radio = playback.radio.as_ref()?;
                Some(PlayingTarget::Radio {
                    genre: genres::find(&radio.genre)?,
                    index: radio.index,
                })
            }
            Current::Queued(track) => Some(PlayingTarget::Album {
                album: album(track.album.as_deref(), &track.album_uri)?,
                index: track.index,
                uri: track.uri.clone(),
            }),
        }
    }

    /// Whether n leads anywhere (`[n] now playing` in the help row).
    pub fn has_now_playing(&self) -> bool {
        self.playing_target().is_some()
    }

    /// n: opens the playing album's track list or the playing radio, selecting the playing
    /// track. If it is already visible, only moves the selection; Esc returns to where n
    /// was pressed.
    fn open_playing(&mut self) -> Vec<Action> {
        match self.playing_target() {
            None => Vec::new(),
            Some(PlayingTarget::Radio { genre, index }) => {
                match self.views.last_mut() {
                    Some(View::Radio(view)) if view.genre.id == genre.id => view.selected = index,
                    _ => self.views.push(View::Radio(RadioView {
                        genre,
                        selected: index,
                        scroll: Scroll::default(),
                    })),
                }
                Vec::new()
            }
            Some(PlayingTarget::Album { album, index, uri }) => {
                if let Some(View::Album(view)) = self.views.last_mut()
                    && view.album.uri == album.uri
                {
                    view.select_uri = None;
                    match (index, &view.tracks) {
                        (Some(index), Some(tracks)) => {
                            view.selected = index;
                            clamp(&mut view.selected, tracks.len());
                        }
                        (Some(index), None) => view.selected = index,
                        (None, Some(tracks)) => {
                            if let Some(index) = tracks.iter().position(|t| t.uri == uri) {
                                view.selected = index;
                            }
                        }
                        (None, None) => view.select_uri = Some(uri),
                    }
                    return Vec::new();
                }
                let action = Action::LoadAlbumTracks(album.uri.clone());
                self.views.push(View::Album(AlbumView {
                    album,
                    tracks: None,
                    selected: index.unwrap_or(0),
                    scroll: Scroll::default(),
                    select_uri: index.is_none().then_some(uri),
                }));
                vec![action]
            }
        }
    }

    /// Opens search. If search is already on the stack, returns to it (the text is kept).
    fn open_search(&mut self) {
        match self.views.iter().position(|v| matches!(v, View::Search(_))) {
            Some(index) => self.views.truncate(index + 1),
            None => self.views.push(View::Search(SearchView::default())),
        }
    }

    /// Opens the queue view. If it is already on the stack, returns to it.
    fn open_queue(&mut self) {
        match self
            .views
            .iter()
            .position(|v| matches!(v, View::Queue { .. }))
        {
            Some(index) => self.views.truncate(index + 1),
            None => self.views.push(View::Queue {
                selected: 0,
                scroll: Scroll::default(),
            }),
        }
    }

    /// Queue view: Ctrl+D removes the selected track from the queue.
    fn queue_key(&mut self, key: Key) -> Vec<Action> {
        let queue = &mut self.playback.queue;
        let Some(View::Queue { selected, .. }) = self.views.last_mut() else {
            return Vec::new();
        };
        if key == Key::Ctrl('d') && *selected < queue.len() {
            queue.remove(*selected);
            clamp(selected, queue.len());
        } else {
            step(selected, key, queue.len());
        }
        Vec::new()
    }

    /// Shelf: Enter on a list row opens the list, on a genre starts its radio, on the
    /// Genres row opens the genre list, and on an artist opens the artist's albums. Ctrl+D
    /// on a genre removes it from the home page.
    fn shelf_key(&mut self, key: Key) -> Vec<Action> {
        let lists = self.shelf_lists();
        let target = match lists.get(self.shelf_selected()) {
            Some(ShelfRow::Curated(_)) => ShelfTarget::Curated,
            Some(ShelfRow::Genre(genre)) => ShelfTarget::Genre(genre),
            Some(ShelfRow::Genres) => ShelfTarget::Genres,
            None => ShelfTarget::Artist,
        };
        let lists = lists.len();
        let artists = self.shelf_artists.len();
        let Some(View::Shelf { selected, .. }) = self.views.last_mut() else {
            return Vec::new();
        };
        if step(selected, key, lists + artists) {
            return Vec::new();
        }
        let selected = *selected;
        match (key, target) {
            (Key::Enter, ShelfTarget::Curated) => {
                self.views.push(View::Curated {
                    selected: 0,
                    scroll: Scroll::default(),
                });
                vec![Action::LoadLists]
            }
            (Key::Enter, ShelfTarget::Genre(genre)) => self.start_radio(genre),
            (Key::Ctrl('d'), ShelfTarget::Genre(genre)) => {
                vec![Action::RemoveFavorite(genre.id.clone())]
            }
            (Key::Enter, ShelfTarget::Genres) => {
                self.views.push(View::Genres {
                    selected: 0,
                    scroll: Scroll::default(),
                });
                Vec::new()
            }
            (Key::Enter, ShelfTarget::Artist) => {
                if let Some(artist) = self.shelf_artists.get(selected - lists) {
                    let albums = albums_of(&self.shelf, &artist.id);
                    self.views.push(View::Artist(ArtistView {
                        artist: artist.clone(),
                        source: Source::Shelf(albums),
                        selected: 0,
                        scroll: Scroll::default(),
                    }));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn shelf_selected(&self) -> usize {
        match self.view() {
            View::Shelf { selected, .. } => *selected,
            _ => 0,
        }
    }

    /// Genres: Enter starts the genre's radio and Ctrl+A adds the genre to the home page or
    /// removes it from there.
    fn genres_key(&mut self, key: Key) -> Vec<Action> {
        let all = genres::all();
        let Some(View::Genres { selected, .. }) = self.views.last_mut() else {
            return Vec::new();
        };
        if step(selected, key, all.len()) {
            return Vec::new();
        }
        let Some(genre) = all.get(*selected) else {
            return Vec::new();
        };
        match key {
            Key::Enter => self.start_radio(genre),
            Key::Ctrl('a') if self.favorite(&genre.id) => {
                vec![Action::RemoveFavorite(genre.id.clone())]
            }
            Key::Ctrl('a') => vec![Action::AddFavorite(genre.id.clone())],
            _ => Vec::new(),
        }
    }

    /// Radio view: Enter plays from the selected track, Ctrl+E adds the track to the queue,
    /// Ctrl+A adds the track's album to the shelf or removes it from there, and Ctrl+R
    /// starts the genre's radio from a new seed.
    fn radio_key(&mut self, key: Key, now: Instant) -> Vec<Action> {
        let View::Radio(view) = self.view() else {
            return Vec::new();
        };
        let genre = view.genre;
        let selected = view.selected;
        if key == Key::Ctrl('r') {
            let current = self
                .playback
                .radio
                .as_ref()
                .filter(|radio| radio.genre == genre.id)
                .map(|radio| radio.seed.clone());
            return self.request_station(genre, current.as_deref());
        }
        let tracks = self.radio_tracks(&genre.id).unwrap_or_default();
        let len = tracks.len();
        match (key, tracks.get(selected)) {
            (Key::Enter, Some(_)) => self.play_radio(selected),
            (Key::Ctrl('e'), Some(track)) => {
                let track = radio_queued(track);
                let name = track.name.clone();
                self.enqueue(vec![track], &name, now)
            }
            (Key::Ctrl('a'), Some(track)) if self.on_shelf(&track.album.uri) => {
                vec![Action::RemoveFromShelf(track.album.uri.clone())]
            }
            (Key::Ctrl('a'), Some(track)) => {
                vec![Action::AddRadioAlbumToShelf(track.album.uri.clone())]
            }
            _ => {
                if let Some(View::Radio(view)) = self.views.last_mut() {
                    step(&mut view.selected, key, len);
                }
                Vec::new()
            }
        }
    }

    /// Enter on a genre: starts the genre's radio from a random seed. The radio view opens
    /// once the station has been fetched. If the genre's radio is already playing, only
    /// opens the view.
    fn start_radio(&mut self, genre: &'static Genre) -> Vec<Action> {
        if self.playing_genre() == Some(genre.id.as_str()) {
            let selected = self.playback.radio.as_ref().map_or(0, |radio| radio.index);
            self.views.push(View::Radio(RadioView {
                genre,
                selected,
                scroll: Scroll::default(),
            }));
            return Vec::new();
        }
        self.request_station(genre, None)
    }

    /// Fetches the genre's station from a seed that is not `current`.
    fn request_station(&mut self, genre: &Genre, current: Option<&str>) -> Vec<Action> {
        let random = self.random();
        let seed = genre.pick_seed(current, &[], random).uri.clone();
        let request = RadioRequest {
            genre: genre.id.clone(),
            seed,
            previous: Vec::new(),
            kind: RadioLoad::Station,
        };
        self.radio_fetch.pending = Some(request.clone());
        vec![Action::LoadRadio(request)]
    }

    /// A station page arrived. A new station replaces the playback context and opens the
    /// radio view (with Ctrl+R the view is already open); a failed fetch changes nothing.
    fn radio_loaded(
        &mut self,
        request: RadioRequest,
        result: Result<Vec<radio::Track>, String>,
    ) -> Vec<Action> {
        if self.radio_fetch.pending.as_ref() != Some(&request) {
            return Vec::new();
        }
        self.radio_fetch.pending = None;
        let page = match result {
            Ok(page) => page,
            Err(message) => {
                self.error = Some(message);
                if request.kind == RadioLoad::More {
                    self.radio_fetch.failed_at = self.playback.radio.as_ref().map(|r| r.index);
                }
                return Vec::new();
            }
        };
        match request.kind {
            RadioLoad::Station => self.new_station(request, page),
            RadioLoad::More => {
                let actions = self.more_radio(page);
                self.trim_radio_history();
                actions
            }
        }
    }

    fn new_station(&mut self, request: RadioRequest, page: Vec<radio::Track>) -> Vec<Action> {
        let Some(genre) = genres::find(&request.genre) else {
            return Vec::new();
        };
        let tracks = radio::new_tracks(page, &[]);
        let Some(first) = tracks.first() else {
            self.error = Some(format!("the {} radio station is empty", genre.name));
            return Vec::new();
        };
        let item = Item::Track(first.uri.clone());
        let mut actions = self
            .likes
            .check(tracks.iter().map(|track| track.uri.as_str()));
        self.error = None;
        self.radio_fetch = RadioFetch::default();
        let playback = &mut self.playback;
        playback.album = None;
        playback.played.clear();
        playback.current = Some(Current::Radio);
        playback.radio = Some(RadioPosition {
            genre: request.genre,
            seed: request.seed.clone(),
            seeds: vec![request.seed],
            seed_start: 0,
            tracks,
            index: 0,
        });
        match self.views.last_mut() {
            Some(View::Radio(view)) => {
                view.genre = genre;
                view.selected = 0;
            }
            _ => self.views.push(View::Radio(RadioView {
                genre,
                selected: 0,
                scroll: Scroll::default(),
            })),
        }
        actions.insert(0, Action::Play(item));
        actions
    }

    /// A refill page: new tracks go to the end of the radio. If little new came in, the
    /// station is repeating itself, and the next fetch uses the genre's next unplayed seed.
    fn more_radio(&mut self, page: Vec<radio::Track>) -> Vec<Action> {
        let random = self.random();
        let Some(radio) = &mut self.playback.radio else {
            return Vec::new();
        };
        let added = radio::new_tracks(page, &radio.tracks);
        let fresh = added.len();
        let actions = self
            .likes
            .check(added.iter().map(|track| track.uri.as_str()));
        radio.tracks.extend(added);
        self.error = None;
        self.radio_fetch.failed_at = None;
        self.radio_fetch.empty = if fresh == 0 {
            self.radio_fetch.empty + 1
        } else {
            0
        };
        if fresh >= RADIO_FRESH_MIN {
            return actions;
        }
        let Some(genre) = genres::find(&radio.genre) else {
            return actions;
        };
        let seed = genre
            .pick_seed(Some(&radio.seed), &radio.seeds, random)
            .uri
            .clone();
        if !radio.seeds.contains(&seed) {
            radio.seeds.push(seed.clone());
        }
        radio.seed = seed;
        radio.seed_start = radio.tracks.len();
        actions
    }

    /// Drops played tracks from the start of the radio when more than `RADIO_HISTORY`
    /// precede the playing one. Indices into the radio (the playing track, the seed's
    /// start, and the radio view's selection and scroll position) shift along with them.
    fn trim_radio_history(&mut self) {
        let Some(radio) = &mut self.playback.radio else {
            return;
        };
        let drop = radio.index.saturating_sub(RADIO_HISTORY);
        if drop == 0 {
            return;
        }
        radio.tracks.drain(..drop);
        radio.index -= drop;
        radio.seed_start = radio.seed_start.saturating_sub(drop);
        self.radio_fetch.failed_at = self
            .radio_fetch
            .failed_at
            .map(|index| index.saturating_sub(drop));
        for view in &mut self.views {
            if let View::Radio(view) = view
                && view.genre.id == radio.genre
            {
                view.selected = view.selected.saturating_sub(drop);
                view.scroll.set(view.scroll.get().saturating_sub(drop));
            }
        }
    }

    /// A refill when fewer than `RADIO_REFILL` tracks remain after the playing radio track
    /// and no fetch is already running or has failed during this track.
    fn radio_refill(&mut self) -> Option<Action> {
        self.playback.current.as_ref()?;
        let radio = self.playback.radio.as_ref()?;
        let fetch = &self.radio_fetch;
        if fetch.pending.is_some()
            || fetch.failed_at == Some(radio.index)
            || radio.tracks.len() >= radio.index + 1 + RADIO_REFILL
        {
            return None;
        }
        let seeds = genres::find(&radio.genre).map_or(0, |genre| genre.seeds.len());
        if fetch.empty > seeds {
            return None;
        }
        let request = RadioRequest {
            genre: radio.genre.clone(),
            seed: radio.seed.clone(),
            previous: radio.tracks[radio.seed_start.min(radio.tracks.len())..]
                .iter()
                .map(|track| track.uri.clone())
                .collect(),
            kind: RadioLoad::More,
        };
        self.radio_fetch.pending = Some(request.clone());
        Some(Action::LoadRadio(request))
    }

    /// The next random number (xorshift).
    fn random(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Curated list: Enter opens the album's tracks, Ctrl+E adds the album to the queue,
    /// Ctrl+A adds it to the shelf or removes it from there, and Ctrl+D rejects it.
    fn curated_key(&mut self, key: Key) -> Vec<Action> {
        let View::Curated { selected, .. } = *self.view() else {
            return Vec::new();
        };
        let albums = self.curated.as_ref().map_or(&[][..], |list| &list.albums);
        let len = albums.len();
        let Some(album) = albums.get(selected).map(|entry| entry.album.clone()) else {
            return Vec::new();
        };
        match key {
            Key::Enter => self.open_album(album),
            Key::Ctrl('e') => vec![Action::QueueAlbum(album)],
            Key::Ctrl('a') => toggle_shelf(&self.shelf_uris, &album),
            Key::Ctrl('d') => vec![Action::RejectCurated(album.uri)],
            _ => {
                if let Some(View::Curated { selected, .. }) = self.views.last_mut() {
                    step(selected, key, len);
                }
                Vec::new()
            }
        }
    }

    fn artist_key(&mut self, key: Key) -> Vec<Action> {
        let View::Artist(view) = self.view() else {
            return Vec::new();
        };
        let albums = view.albums();
        let len = albums.len();
        let from_shelf = matches!(view.source, Source::Shelf(_));
        let Some(album) = albums.get(view.selected) else {
            return Vec::new();
        };
        match key {
            Key::Enter => self.open_album(album.clone()),
            Key::Ctrl('e') => vec![Action::QueueAlbum(album.clone())],
            Key::Ctrl('d') if from_shelf => vec![Action::RemoveFromShelf(album.uri.clone())],
            Key::Ctrl('a') => toggle_shelf(&self.shelf_uris, album),
            _ => {
                if let Some(View::Artist(view)) = self.views.last_mut() {
                    step(&mut view.selected, key, len);
                }
                Vec::new()
            }
        }
    }

    /// Track list: Enter plays the album from the selected track, Ctrl+E adds the track to
    /// the queue, and Ctrl+A adds the album to the shelf or removes it from there.
    fn album_key(&mut self, key: Key, now: Instant) -> Vec<Action> {
        let View::Album(view) = self.view() else {
            return Vec::new();
        };
        let tracks = view.tracks.as_deref().unwrap_or_default();
        let len = tracks.len();
        match key {
            Key::Enter if view.selected < len => {
                let album = view.album.clone();
                let track = view.selected;
                self.play_album(album, len, track)
            }
            Key::Ctrl('e') if view.selected < len => {
                let track = queued(&view.album, tracks, view.selected);
                let name = track.name.clone();
                self.enqueue(vec![track], &name, now)
            }
            // A radio track's album (n from the queue) has no year: when adding it to the
            // shelf, the year and the album's artist are fetched as in the radio view.
            Key::Ctrl('a') if view.album.year.is_none() && !self.on_shelf(&view.album.uri) => {
                vec![Action::AddRadioAlbumToShelf(view.album.uri.clone())]
            }
            Key::Ctrl('a') => toggle_shelf(&self.shelf_uris, &view.album),
            _ => {
                if let Some(View::Album(view)) = self.views.last_mut() {
                    step(&mut view.selected, key, len);
                }
                Vec::new()
            }
        }
    }

    fn search_key(&mut self, key: Key, now: Instant) -> Vec<Action> {
        let Some(View::Search(search)) = self.views.last_mut() else {
            return Vec::new();
        };
        match key {
            Key::Char(c) => {
                let at = byte_index(&search.query, search.cursor);
                search.query.insert(at, c);
                search.cursor += 1;
                search.edited(now);
            }
            Key::Backspace if search.cursor > 0 => {
                search.cursor -= 1;
                let at = byte_index(&search.query, search.cursor);
                search.query.remove(at);
                search.edited(now);
            }
            Key::Left => search.cursor = search.cursor.saturating_sub(1),
            Key::Right => search.cursor = (search.cursor + 1).min(search.query.chars().count()),
            Key::Up | Key::Down => {
                let len = search.items().len();
                step(&mut search.selected, key, len);
            }
            Key::Enter => match search.items().get(search.selected) {
                Some(SearchItem::Artist(artist)) => {
                    let artist = (*artist).clone();
                    let action = Action::LoadArtistAlbums(artist.id.clone());
                    self.views.push(View::Artist(ArtistView {
                        artist,
                        source: Source::Spotify(None),
                        selected: 0,
                        scroll: Scroll::default(),
                    }));
                    return vec![action];
                }
                Some(SearchItem::Album(album)) => {
                    let album = (*album).clone();
                    return self.open_album(album);
                }
                None => {}
            },
            Key::Ctrl('a') => {
                if let Some(SearchItem::Album(album)) = search.items().get(search.selected) {
                    return toggle_shelf(&self.shelf_uris, album);
                }
            }
            Key::Ctrl('e') => {
                if let Some(SearchItem::Album(album)) = search.items().get(search.selected) {
                    return vec![Action::QueueAlbum((*album).clone())];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Sends the search once typing has paused.
    fn tick(&mut self, now: Instant) -> Vec<Action> {
        let Some(search) = search_view(&mut self.views) else {
            return Vec::new();
        };
        match search.due {
            Some(due) if now >= due => {
                search.due = None;
                let query = search.query.trim().to_owned();
                if search.requested.as_ref() == Some(&query) {
                    return Vec::new();
                }
                search.requested = Some(query.clone());
                search.pending = true;
                vec![Action::Search(query)]
            }
            _ => Vec::new(),
        }
    }

    fn searched(&mut self, query: String, result: Result<SearchResults, String>) {
        let Some(search) = search_view(&mut self.views) else {
            return;
        };
        if !search.pending || search.requested.as_ref() != Some(&query) {
            return;
        }
        search.pending = false;
        match result {
            Ok(results) => {
                search.results = Some(results);
                search.selected = 0;
                self.error = None;
            }
            Err(message) => {
                search.requested = None;
                self.error = Some(message);
            }
        }
    }

    fn album_tracks(&mut self, uri: &str, result: Result<Vec<Track>, String>) -> Vec<Action> {
        let actions = match &result {
            Ok(tracks) => self
                .likes
                .check(tracks.iter().map(|track| track.uri.as_str())),
            Err(_) => Vec::new(),
        };
        for view in &mut self.views {
            if let View::Album(view) = view
                && view.album.uri == uri
            {
                let tracks = result.clone().unwrap_or_default();
                if let Some(uri) = view.select_uri.take()
                    && let Some(index) = tracks.iter().position(|track| track.uri == uri)
                {
                    view.selected = index;
                }
                clamp(&mut view.selected, tracks.len());
                view.tracks = Some(tracks);
            }
        }
        match result {
            Ok(_) => self.error = None,
            Err(message) => self.error = Some(message),
        }
        actions
    }

    /// The album's tracks arrived, to be added to the queue.
    fn album_queued(
        &mut self,
        album: &Album,
        result: Result<Vec<Track>, String>,
        now: Instant,
    ) -> Vec<Action> {
        match result {
            Ok(tracks) => {
                self.error = None;
                let mut actions = self
                    .likes
                    .check(tracks.iter().map(|track| track.uri.as_str()));
                let tracks = (0..tracks.len())
                    .map(|index| queued(album, &tracks, index))
                    .collect();
                actions.splice(0..0, self.enqueue(tracks, &album.name, now));
                actions
            }
            Err(message) => {
                self.error = Some(message);
                Vec::new()
            }
        }
    }

    fn artist_albums(&mut self, artist_id: &str, result: Result<Vec<Album>, String>) {
        for view in &mut self.views {
            if let View::Artist(ArtistView {
                artist,
                source: source @ Source::Spotify(None),
                ..
            }) = view
                && artist.id == artist_id
            {
                let albums = match &result {
                    Ok(albums) => {
                        let mut albums = albums.clone();
                        sort_by_year(&mut albums);
                        albums
                    }
                    Err(_) => Vec::new(),
                };
                *source = Source::Spotify(Some(albums));
            }
        }
        match result {
            Ok(_) => self.error = None,
            Err(message) => self.error = Some(message),
        }
    }

    /// The shelf changed: an artist view whose last album was removed is closed, and
    /// selections are kept within their lists.
    fn shelf_changed(&mut self, albums: Vec<Album>) {
        self.set_shelf(albums);
        self.error = None;
        let shelf = &self.shelf;
        for view in &mut self.views {
            if let View::Artist(ArtistView {
                artist,
                source: Source::Shelf(albums),
                ..
            }) = view
            {
                *albums = albums_of(shelf, &artist.id);
            }
        }
        self.views.retain(|view| match view {
            View::Artist(
                view @ ArtistView {
                    source: Source::Shelf(_),
                    ..
                },
            ) => !view.albums().is_empty(),
            _ => true,
        });
        let rows = self.shelf_rows();
        for view in &mut self.views {
            match view {
                View::Shelf { selected, .. } => clamp(selected, rows),
                View::Artist(view) => {
                    let len = view.albums().len();
                    clamp(&mut view.selected, len);
                }
                View::Curated { .. }
                | View::Search(_)
                | View::Album(_)
                | View::Queue { .. }
                | View::Genres { .. }
                | View::Radio(_) => {}
            }
        }
    }

    /// The lists were read: a broken file is shown as an error and its list row is hidden.
    fn lists_loaded(&mut self, result: Result<Option<AlbumList>, String>) {
        match result {
            Ok(list) => {
                if self.lists_broken {
                    self.error = None;
                    self.lists_broken = false;
                }
                self.set_curated(list);
            }
            Err(message) => {
                self.error = Some(message);
                self.lists_broken = true;
                self.set_curated(None);
            }
        }
    }

    /// Replaces the Curated list. The list view's selection stays within the list.
    fn set_curated(&mut self, list: Option<AlbumList>) {
        self.set_lists(|state| state.curated = list);
        let len = self.curated.as_ref().map_or(0, |list| list.albums.len());
        for view in &mut self.views {
            if let View::Curated { selected, .. } = view {
                clamp(selected, len);
            }
        }
    }

    /// Changes the list rows (Curated or favourite genres). The shelf's selection stays on
    /// the same artist even as list rows appear or disappear. On a list row the selection
    /// stays at the same position, so the next row takes the removed row's place.
    fn set_lists(&mut self, change: impl FnOnce(&mut Self)) {
        let before = self.shelf_lists().len();
        change(self);
        let after = self.shelf_lists().len();
        let rows = self.shelf_rows();
        for view in &mut self.views {
            if let View::Shelf { selected, .. } = view {
                if *selected >= before {
                    *selected = *selected - before + after;
                }
                clamp(selected, rows);
            }
        }
    }

    /// Ctrl+L: likes the playing track or removes the like. An unknown state counts as
    /// not liked, and while a change is pending the key does nothing.
    fn toggle_like(&mut self) -> Vec<Action> {
        let Some(playing) = self.now_playing.as_ref().filter(|p| !p.uri.is_empty()) else {
            return Vec::new();
        };
        if !self.likes.enabled {
            self.error = Some(LIKES_NEED_APP.to_owned());
            return Vec::new();
        }
        if self.likes.changing.contains_key(&playing.uri) {
            return Vec::new();
        }
        let uri = playing.uri.clone();
        let liked = !self.liked(&uri);
        self.likes
            .changing
            .insert(uri.clone(), playing.track.clone());
        vec![Action::SetLiked { uri, liked }]
    }

    fn liked_checked(&mut self, uris: Vec<String>, result: Result<Vec<bool>, String>) {
        match result {
            Ok(flags) => {
                // A like made during the query is newer information than the answer.
                for (uri, liked) in uris.into_iter().zip(flags) {
                    self.likes.known.entry(uri).or_insert(liked);
                }
            }
            Err(message) => {
                self.likes.stopped = true;
                self.error = Some(message);
            }
        }
    }

    fn like_changed(&mut self, uri: String, liked: bool, result: Result<(), String>, now: Instant) {
        let name = self.likes.changing.remove(&uri).unwrap_or_default();
        match result {
            Ok(()) => {
                self.error = None;
                self.likes.known.insert(uri, liked);
                self.notice = Some(Notice {
                    kind: if liked {
                        NoticeKind::Liked
                    } else {
                        NoticeKind::Unliked
                    },
                    name,
                    until: now + NOTICE_TIME,
                });
            }
            Err(message) => self.error = Some(message),
        }
    }

    fn player(&mut self, event: player::Event, now: Instant) -> Vec<Action> {
        match event {
            player::Event::TrackChanged {
                uri,
                artist,
                album,
                track,
                duration,
                cover,
            } => {
                // Usually the state has already been asked along with the track list or
                // radio; e.g. the tracks of a restored album have not.
                let actions = self.likes.check([uri.as_str()]);
                self.now_playing = Some(NowPlaying {
                    uri,
                    artist,
                    album,
                    track,
                    duration,
                    cover,
                    paused: false,
                    position: Duration::ZERO,
                    position_at: now,
                });
                // Loading the restored track is not a user action: a startup error
                // (e.g. a broken shelf) stays visible.
                if !self.restoring {
                    self.error = None;
                }
                return actions;
            }
            player::Event::EndOfTrack => {
                if let Some(action) = self.next() {
                    return vec![action];
                }
                self.stopped();
                return vec![Action::Stop];
            }
            player::Event::PreloadNext => {
                return self.upcoming().map(Action::Preload).into_iter().collect();
            }
            player::Event::Position(ms) => {
                self.restoring = false;
                if let Some(playing) = &mut self.now_playing {
                    playing.position = Duration::from_millis(ms.into());
                    playing.position_at = now;
                    playing.paused = false;
                }
            }
            player::Event::Paused(ms) => {
                if let Some(playing) = &mut self.now_playing {
                    playing.position = Duration::from_millis(ms.into()).min(playing.duration);
                    playing.position_at = now;
                    playing.paused = true;
                }
                if self.restoring {
                    self.restoring = false;
                } else {
                    self.error = None;
                }
            }
            player::Event::Stopped => self.stopped(),
            // The restored track could not be loaded (network, removed album): Deck
            // continues empty.
            player::Event::Error(message) if self.restoring => {
                self.restoring = false;
                self.playback = Playback::default();
                self.now_playing = None;
                self.error = Some(format!("could not restore playback: {message}"));
            }
            player::Event::Error(message) => self.error = Some(message),
        }
        Vec::new()
    }
}

/// Album track `index`, to be added to the queue.
fn queued(album: &Album, tracks: &[Track], index: usize) -> QueuedTrack {
    let track = &tracks[index];
    QueuedTrack {
        uri: track.uri.clone(),
        name: track.name.clone(),
        artist: track.artist.clone(),
        duration: track.duration,
        album_uri: album.uri.clone(),
        index: Some(index),
        album: Some(Box::new(album.clone())),
    }
}

/// A radio track to be added to the queue. The station does not report the duration, so
/// it is zero (the queue view leaves it out).
fn radio_queued(track: &radio::Track) -> QueuedTrack {
    QueuedTrack {
        uri: track.uri.clone(),
        name: track.name.clone(),
        artist: track.artist.clone(),
        duration: Duration::ZERO,
        album_uri: track.album.uri.clone(),
        index: None,
        album: Some(Box::new(Album {
            id: spotify_id(&track.album.uri),
            uri: track.album.uri.clone(),
            name: track.album.name.clone(),
            artist: track.artist.clone(),
            artist_id: spotify_id(&track.artist_uri),
            year: None,
        })),
    }
}

/// The search on the stack, if there is one.
fn search_view(views: &mut [View]) -> Option<&mut SearchView> {
    views.iter_mut().find_map(|view| match view {
        View::Search(search) => Some(search),
        _ => None,
    })
}

/// The id part of a URI: `spotify:album:abc` → `abc`.
fn spotify_id(uri: &str) -> String {
    uri.rsplit(':').next().unwrap_or_default().to_owned()
}

/// Where n leads (`State::playing_target`).
enum PlayingTarget {
    /// The album's track list. The track's position, or its URI if the position is missing.
    Album {
        album: Album,
        index: Option<usize>,
        uri: String,
    },
    Radio {
        genre: &'static Genre,
        index: usize,
    },
}

/// Where the next track comes from (`State::upcoming_source`).
enum Upcoming {
    Queue,
    /// An album track.
    Album(usize),
    /// A radio track.
    Radio(usize),
}

/// The selected shelf row, for handling a key.
enum ShelfTarget {
    Curated,
    Genre(&'static Genre),
    Genres,
    Artist,
}

impl ArtistView {
    /// The artist view's albums by year, oldest first.
    pub fn albums(&self) -> &[Album] {
        match &self.source {
            Source::Shelf(albums) => albums,
            Source::Spotify(albums) => albums.as_deref().unwrap_or_default(),
        }
    }
}

impl SearchView {
    /// The results as one list: artists first, then albums.
    pub fn items(&self) -> Vec<SearchItem<'_>> {
        let Some(results) = &self.results else {
            return Vec::new();
        };
        results
            .artists
            .iter()
            .map(SearchItem::Artist)
            .chain(results.albums.iter().map(SearchItem::Album))
            .collect()
    }

    /// The search is waiting for the delay or for results.
    pub fn searching(&self) -> bool {
        self.due.is_some() || self.pending
    }

    /// The search text changed: search after the delay; empty text clears the results.
    fn edited(&mut self, now: Instant) {
        if self.query.trim().is_empty() {
            *self = Self {
                query: std::mem::take(&mut self.query),
                cursor: self.cursor,
                ..Self::default()
            };
        } else {
            self.due = Some(now + SEARCH_DEBOUNCE);
        }
    }
}

impl NowPlaying {
    /// Elapsed time: the player's latest position plus the time passed since.
    pub fn elapsed(&self, now: Instant) -> Duration {
        if self.paused {
            return self.position;
        }
        (self.position + now.saturating_duration_since(self.position_at)).min(self.duration)
    }
}

/// Duration in milliseconds; a duration too long to fit is `u64::MAX`.
fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Ctrl+A: the album onto the shelf, or off it if already there (`shelf_uris`).
fn toggle_shelf(shelf_uris: &HashSet<String>, album: &Album) -> Vec<Action> {
    if shelf_uris.contains(&album.uri) {
        vec![Action::RemoveFromShelf(album.uri.clone())]
    } else {
        vec![Action::AddToShelf(album.clone())]
    }
}

/// Moves the selection within the list with ↑/↓. Returns `true` if the key was an arrow.
fn step(selected: &mut usize, key: Key, len: usize) -> bool {
    match key {
        Key::Up => *selected = selected.saturating_sub(1),
        Key::Down => *selected = (*selected + 1).min(len.saturating_sub(1)),
        _ => return false,
    }
    true
}

fn clamp(selected: &mut usize, len: usize) {
    *selected = (*selected).min(len.saturating_sub(1));
}

/// The byte offset of a character index in a string.
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lists::{Curated, ListAlbum, TEST_NOW};
    use Key::*;

    fn album(id: &str, name: &str, artist: &str, year: Option<u16>) -> Album {
        Album {
            id: id.to_owned(),
            uri: format!("spotify:album:{id}"),
            name: name.to_owned(),
            artist: artist.to_owned(),
            artist_id: artist_id(artist),
            year,
        }
    }

    fn artist(name: &str) -> Artist {
        Artist {
            id: artist_id(name),
            name: name.to_owned(),
        }
    }

    fn artist_id(name: &str) -> String {
        format!("id-{}", name.to_lowercase().replace(' ', "-"))
    }

    fn bleach() -> Album {
        album("bleach", "Bleach", "Nirvana", Some(1989))
    }
    fn nevermind() -> Album {
        album("nevermind", "Nevermind", "Nirvana", Some(1991))
    }
    fn in_utero() -> Album {
        album("inutero", "In Utero", "Nirvana", Some(1993))
    }
    fn ok_computer() -> Album {
        album("okc", "OK Computer", "Radiohead", Some(1997))
    }
    fn colour() -> Album {
        album(
            "colour",
            "The Colour And The Shape",
            "Foo Fighters",
            Some(1997),
        )
    }

    /// The state and the test's clock.
    struct App {
        state: State,
        now: Instant,
    }

    impl App {
        /// Shelf with the selection on the first artist: the list group always has at least
        /// the Genres row, where the selection starts.
        fn new(shelf: Vec<Album>) -> Self {
            let mut app = Self::at_top(shelf);
            app.key(Down);
            app
        }

        /// Shelf with the selection on the top list row, as at startup.
        fn at_top(shelf: Vec<Album>) -> Self {
            Self {
                state: State::new(shelf),
                now: Instant::now(),
            }
        }

        fn send(&mut self, event: Event) -> Vec<Action> {
            self.state.update(event, self.now)
        }

        fn key(&mut self, key: Key) -> Vec<Action> {
            self.send(Event::Key(key))
        }

        fn keys(&mut self, keys: &[Key]) -> Vec<Action> {
            keys.iter().flat_map(|&k| self.key(k)).collect()
        }

        fn type_text(&mut self, text: &str) -> Vec<Action> {
            text.chars().flat_map(|c| self.key(Char(c))).collect()
        }

        /// Lets time pass and sends a tick.
        fn wait(&mut self, ms: u64) -> Vec<Action> {
            self.now += Duration::from_millis(ms);
            self.send(Event::Tick)
        }

        /// Types a search, waits for the delay and returns the results.
        fn search(&mut self, text: &str, artists: Vec<Artist>, albums: Vec<Album>) {
            self.type_text(text);
            let actions = self.wait(300);
            assert_eq!(actions, [Action::Search(text.to_owned())]);
            self.send(Event::Searched {
                query: text.to_owned(),
                result: Ok(SearchResults { artists, albums }),
            });
        }

        fn shelf_selected(&self) -> usize {
            match self.state.view() {
                View::Shelf { selected, .. } => *selected,
                other => panic!("expected the shelf, got {other:?}"),
            }
        }

        fn artist_view(&self) -> &ArtistView {
            match self.state.view() {
                View::Artist(view) => view,
                other => panic!("expected an artist view, got {other:?}"),
            }
        }

        fn artist_albums(&self) -> Vec<String> {
            self.artist_view()
                .albums()
                .iter()
                .map(|a| a.name.clone())
                .collect()
        }

        fn album_view(&self) -> &AlbumView {
            match self.state.view() {
                View::Album(view) => view,
                other => panic!("expected a track list, got {other:?}"),
            }
        }

        fn search_view(&self) -> &SearchView {
            match self.state.view() {
                View::Search(view) => view,
                other => panic!("expected search, got {other:?}"),
            }
        }

        fn is_shelf(&self) -> bool {
            matches!(self.state.view(), View::Shelf { .. })
        }
    }

    fn shelf_app() -> App {
        App::new(vec![
            in_utero(),
            ok_computer(),
            nevermind(),
            colour(),
            bleach(),
        ])
    }

    fn play(album_id: &str, track: usize) -> Action {
        Action::Play(Item::Album {
            uri: format!("spotify:album:{album_id}"),
            track,
        })
    }

    fn load_tracks(album_id: &str) -> Action {
        Action::LoadAlbumTracks(format!("spotify:album:{album_id}"))
    }

    fn names(artists: &[Artist]) -> Vec<&str> {
        artists.iter().map(|a| a.name.as_str()).collect()
    }

    // --- Album shelf ---

    #[test]
    fn starts_on_shelf_with_artists_alphabetical() {
        let app = App::at_top(shelf_app().state.shelf);
        assert!(app.is_shelf());
        assert_eq!(app.shelf_selected(), 0);
        assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
        assert_eq!(
            names(app.state.shelf_artists()),
            ["Foo Fighters", "Nirvana", "Radiohead"]
        );
        assert_eq!(app.state.now_playing, None);
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn shelf_up_down_stay_in_bounds() {
        let mut app = App::at_top(shelf_app().state.shelf);
        assert!(app.key(Up).is_empty());
        assert_eq!(app.shelf_selected(), 0);
        // Genres and three artists.
        app.keys(&[Down, Down, Down, Down]);
        assert_eq!(app.shelf_selected(), 3);
        app.key(Up);
        assert_eq!(app.shelf_selected(), 2);
    }

    #[test]
    fn empty_shelf_keys_do_nothing_and_search_opens() {
        let mut app = App::new(vec![]);
        assert!(app.state.shelf_artists().is_empty());
        assert!(app.keys(&[Up, Down, Ctrl('d'), Ctrl('a'), Esc]).is_empty());
        assert!(app.is_shelf());
        assert_eq!(app.shelf_selected(), 0);

        app.key(Ctrl('f'));
        app.search_view();

        // The only row is Genres.
        app.key(Esc);
        assert!(app.key(Enter).is_empty());
        assert!(matches!(app.state.view(), View::Genres { selected: 0, .. }));
    }

    #[test]
    fn enter_opens_artist_shelf_albums_oldest_first() {
        let mut app = shelf_app();
        app.key(Down);
        assert!(app.key(Enter).is_empty());
        let view = app.artist_view();
        assert_eq!(view.artist, artist("Nirvana"));
        assert!(matches!(view.source, Source::Shelf(_)));
        assert_eq!(view.selected, 0);
        assert_eq!(app.artist_albums(), ["Bleach", "Nevermind", "In Utero"]);
    }

    #[test]
    fn esc_returns_to_shelf_keeping_selection() {
        let mut app = shelf_app();
        app.keys(&[Down, Down, Enter]);
        // The shelf comes into view: the lists are reread.
        assert_eq!(app.key(Esc), [Action::LoadLists]);
        assert_eq!(app.shelf_selected(), 3);
        // The shelf is at the bottom: Esc goes nowhere.
        app.key(Esc);
        assert_eq!(app.shelf_selected(), 3);
    }

    // --- Lists on the shelf and the Curated list ---

    fn no_other() -> Album {
        album("noother", "No Other", "Gene Clark", Some(1974))
    }
    fn number_one() -> Album {
        album("number1", "#1 Record", "Big Star", Some(1972))
    }
    fn transatlanticism() -> Album {
        album(
            "transat",
            "Transatlanticism",
            "Death Cab for Cutie",
            Some(2003),
        )
    }

    /// A Curated list for the view, round 2026-W41.
    fn curated(albums: &[Album]) -> AlbumList {
        Curated {
            round: "2026-W41".to_owned(),
            created: "2026-10-05T07:00:00+03:00".to_owned(),
            albums: albums
                .iter()
                .map(|album| ListAlbum {
                    album: album.clone(),
                    reason: Some(format!("Because of {}.", album.artist)),
                })
                .collect(),
        }
        .to_list(TEST_NOW)
    }

    fn three_curated() -> AlbumList {
        curated(&[no_other(), number_one(), transatlanticism()])
    }

    /// Shelf and a Curated list (three albums).
    fn curated_app() -> App {
        let mut app = shelf_app();
        app.state = State::new(app.state.shelf.clone()).with_curated(Ok(Some(three_curated())));
        app
    }

    fn curated_selected(app: &App) -> usize {
        match app.state.view() {
            View::Curated { selected, .. } => *selected,
            other => panic!("expected the Curated list, got {other:?}"),
        }
    }

    fn curated_names(app: &App) -> Vec<String> {
        app.state
            .curated()
            .map(|list| list.albums.iter().map(|a| a.album.name.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn list_row_comes_before_artists_and_starts_selected() {
        let mut app = curated_app();
        let lists = app.state.shelf_lists();
        assert_eq!(lists.len(), 2);
        assert!(
            matches!(&lists[0], ShelfRow::Curated(list) if list.subtitle == "3 albums · week 41")
        );
        assert_eq!(lists[1], ShelfRow::Genres);
        assert_eq!(app.shelf_selected(), 0);
        // List rows and three artists as one list.
        app.keys(&[Down, Down, Down, Down, Down, Down]);
        assert_eq!(app.shelf_selected(), 4);
        app.keys(&[Up, Up]);
        assert_eq!(app.shelf_selected(), 2);
        // Enter on an artist opens the artist's albums.
        assert!(app.key(Enter).is_empty());
        assert_eq!(app.artist_view().artist, artist("Foo Fighters"));
    }

    #[test]
    fn enter_on_list_row_opens_list_and_reloads_it() {
        let mut app = curated_app();
        assert_eq!(app.key(Enter), [Action::LoadLists]);
        assert_eq!(curated_selected(&app), 0);
        assert_eq!(
            curated_names(&app),
            ["No Other", "#1 Record", "Transatlanticism"]
        );
        assert_eq!(app.key(Esc), [Action::LoadLists]);
        assert_eq!(app.shelf_selected(), 0);
    }

    #[test]
    fn without_list_shelf_is_as_before() {
        for list in [None, Some(curated(&[]))] {
            let mut app = shelf_app();
            app.state = State::new(app.state.shelf.clone()).with_curated(Ok(list));
            assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
            app.keys(&[Down, Down, Down]);
            assert_eq!(app.shelf_selected(), 3);
            app.key(Enter);
            assert_eq!(app.artist_view().artist, artist("Radiohead"));
        }
    }

    #[test]
    fn list_appearing_or_disappearing_keeps_selected_artist() {
        let mut app = shelf_app();
        app.keys(&[Down, Down]);
        app.send(Event::ListsLoaded(Ok(Some(three_curated()))));
        assert_eq!(app.shelf_selected(), 4);
        app.send(Event::ListsLoaded(Ok(Some(three_curated()))));
        assert_eq!(app.shelf_selected(), 4);
        app.send(Event::ListsLoaded(Ok(None)));
        assert_eq!(app.shelf_selected(), 3);

        // Selection on the Curated row: when it goes, the next row, Genres, is selected.
        let mut app = curated_app();
        app.send(Event::ListsLoaded(Ok(Some(curated(&[])))));
        assert_eq!(app.shelf_selected(), 0);
        assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
    }

    #[test]
    fn broken_list_file_shows_error_and_hides_list_row() {
        let mut app = curated_app();
        app.send(Event::ListsLoaded(
            Err("curated list file is broken".into()),
        ));
        assert_eq!(
            app.state.error.as_deref(),
            Some("curated list file is broken")
        );
        assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
        assert_eq!(app.state.curated(), None);

        // Fixed file: the error goes away and the row comes back.
        app.send(Event::ListsLoaded(Ok(Some(three_curated()))));
        assert_eq!(app.state.error, None);
        assert_eq!(app.state.shelf_lists().len(), 2);

        // A successful load does not clear an unrelated error.
        app.send(Event::Error("the shelf is read-only".into()));
        app.send(Event::ListsLoaded(Ok(Some(three_curated()))));
        assert_eq!(app.state.error.as_deref(), Some("the shelf is read-only"));
    }

    #[test]
    fn curated_keys() {
        let mut app = curated_app();
        app.key(Enter);
        assert!(app.key(Up).is_empty());
        assert_eq!(curated_selected(&app), 0);
        app.keys(&[Down, Down, Down]);
        assert_eq!(curated_selected(&app), 2);
        app.key(Up);

        assert_eq!(app.key(Ctrl('e')), [Action::QueueAlbum(number_one())]);
        assert_eq!(app.key(Ctrl('a')), [Action::AddToShelf(number_one())]);
        app.send(Event::ShelfChanged(vec![number_one(), colour()]));
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf(number_one().uri)]
        );
        assert_eq!(
            app.key(Ctrl('d')),
            [Action::RejectCurated(number_one().uri)]
        );
        // The state changes only once the rejection has been saved.
        assert_eq!(curated_names(&app).len(), 3);

        // Enter opens the track list without playing; Esc goes back without rereading.
        assert_eq!(app.key(Enter), [load_tracks("number1")]);
        assert_eq!(app.album_view().album, number_one());
        assert!(app.key(Esc).is_empty());
        assert_eq!(curated_selected(&app), 1);
    }

    #[test]
    fn rejecting_keeps_selection_in_list_until_nothing_is_left() {
        let mut app = curated_app();
        app.keys(&[Enter, Down, Down]);
        app.send(Event::Error("could not save".into()));
        app.send(Event::CuratedChanged(Some(curated(&[
            no_other(),
            number_one(),
        ]))));
        assert_eq!(app.state.error, None);
        assert_eq!(curated_selected(&app), 1);
        assert_eq!(curated_names(&app), ["No Other", "#1 Record"]);

        app.send(Event::CuratedChanged(Some(curated(&[]))));
        assert_eq!(curated_selected(&app), 0);
        assert!(
            app.keys(&[Up, Down, Enter, Ctrl('e'), Ctrl('a'), Ctrl('d')])
                .is_empty()
        );

        // The shelf no longer has a Curated row: the selection is on the Genres row.
        assert_eq!(app.key(Esc), [Action::LoadLists]);
        assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
        assert_eq!(app.shelf_selected(), 0);
    }

    #[test]
    fn failed_reject_keeps_list_and_shows_error() {
        let mut app = curated_app();
        app.keys(&[Enter, Ctrl('d')]);
        app.send(Event::Error("curated history file is broken".into()));
        assert_eq!(curated_names(&app).len(), 3);
        assert_eq!(
            app.state.error.as_deref(),
            Some("curated history file is broken")
        );
    }

    #[test]
    fn player_keys_in_curated_view() {
        let mut app = curated_app();
        app.key(Enter);
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        assert_eq!(app.key(Char('q')), [Action::Quit]);
        app.key(Ctrl('f'));
        app.search_view();
    }

    // --- Artist's albums from the shelf ---

    #[test]
    fn artist_up_down_stay_in_bounds() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Up]);
        assert_eq!(app.artist_view().selected, 0);
        app.keys(&[Down, Down, Down, Down]);
        assert_eq!(app.artist_view().selected, 2);
    }

    #[test]
    fn enter_opens_selected_albums_tracks_without_playing() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down]);
        assert_eq!(app.key(Enter), [load_tracks("nevermind")]);
        let view = app.album_view();
        assert_eq!(view.album, nevermind());
        assert_eq!(view.tracks, None);
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn ctrl_d_removes_selected_album_from_shelf() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down]);
        assert_eq!(
            app.key(Ctrl('d')),
            [Action::RemoveFromShelf("spotify:album:nevermind".into())]
        );
        // The state changes only once the removal has been saved.
        assert_eq!(app.artist_albums(), ["Bleach", "Nevermind", "In Utero"]);

        app.send(Event::ShelfChanged(vec![
            in_utero(),
            ok_computer(),
            colour(),
            bleach(),
        ]));
        assert_eq!(app.artist_albums(), ["Bleach", "In Utero"]);
        assert_eq!(app.artist_view().selected, 1);
    }

    #[test]
    fn removing_last_album_clamps_selection() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down, Down]);
        app.key(Ctrl('d'));
        app.send(Event::ShelfChanged(vec![
            ok_computer(),
            nevermind(),
            colour(),
            bleach(),
        ]));
        assert_eq!(app.artist_albums(), ["Bleach", "Nevermind"]);
        assert_eq!(app.artist_view().selected, 1);
    }

    #[test]
    fn removing_artists_last_album_returns_to_shelf() {
        let mut app = shelf_app();
        app.keys(&[Down, Down, Enter]);
        assert_eq!(
            app.key(Ctrl('d')),
            [Action::RemoveFromShelf("spotify:album:okc".into())]
        );
        app.send(Event::ShelfChanged(vec![
            in_utero(),
            nevermind(),
            colour(),
            bleach(),
        ]));
        assert!(app.is_shelf());
        assert_eq!(
            names(app.state.shelf_artists()),
            ["Foo Fighters", "Nirvana"]
        );
        assert_eq!(app.shelf_selected(), 2);
    }

    #[test]
    fn removing_only_album_leaves_empty_shelf() {
        let mut app = App::new(vec![colour()]);
        app.key(Enter);
        app.key(Ctrl('d'));
        app.send(Event::ShelfChanged(vec![]));
        assert!(app.is_shelf());
        assert_eq!(app.shelf_selected(), 0);
        assert!(app.state.shelf_artists().is_empty());
    }

    #[test]
    fn failed_removal_keeps_view_and_shows_error() {
        let mut app = shelf_app();
        app.keys(&[Down, Down, Enter, Ctrl('d')]);
        app.send(Event::Error("the shelf is read-only".into()));
        assert_eq!(app.artist_albums(), ["OK Computer"]);
        assert_eq!(app.state.error.as_deref(), Some("the shelf is read-only"));
    }

    #[test]
    fn ctrl_a_removes_from_shelf_artist_view() {
        let mut app = shelf_app();
        app.keys(&[Down, Down, Enter]);
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf("spotify:album:okc".into())]
        );
    }

    // --- Search ---

    #[test]
    fn ctrl_f_opens_search_from_shelf_and_esc_returns() {
        let mut app = shelf_app();
        app.key(Down);
        assert!(app.key(Ctrl('f')).is_empty());
        let search = app.search_view();
        assert_eq!(search.query, "");
        assert_eq!(search.results, None);
        app.key(Esc);
        assert_eq!(app.shelf_selected(), 2);
    }

    #[test]
    fn ctrl_f_opens_search_from_artist_and_esc_returns() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down, Ctrl('f')]);
        app.search_view();
        app.key(Esc);
        assert_eq!(app.artist_view().selected, 1);
        assert_eq!(app.artist_view().artist, artist("Nirvana"));
    }

    #[test]
    fn ctrl_f_in_search_keeps_search() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("foo");
        app.key(Ctrl('f'));
        assert_eq!(app.search_view().query, "foo");
        app.key(Esc);
        assert!(app.is_shelf());
    }

    #[test]
    fn typing_edits_query_at_cursor() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        // Space, q and the arrows edit the search and do not control the player.
        assert!(app.type_text("the qeen").is_empty());
        assert!(app.keys(&[Left, Left, Left]).is_empty());
        app.key(Char('u'));
        assert_eq!(app.search_view().query, "the queen");
        assert_eq!(app.search_view().cursor, 6);
        app.keys(&[Right, Right, Right, Right, Right]);
        assert_eq!(app.search_view().cursor, 9);
        app.key(Backspace);
        assert_eq!(app.search_view().query, "the quee");
        app.keys(&[Left; 20]);
        assert_eq!(app.search_view().cursor, 0);
        app.key(Backspace);
        assert_eq!(app.search_view().query, "the quee");
    }

    #[test]
    fn cursor_counts_characters_not_bytes() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("björk");
        app.keys(&[Left, Left, Left, Backspace]);
        assert_eq!(app.search_view().query, "börk");
        app.key(Char('j'));
        assert_eq!(app.search_view().query, "björk");
    }

    #[test]
    fn search_starts_after_typing_pause() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.key(Char('f'));
        assert!(app.wait(100).is_empty());
        app.key(Char('o'));
        assert!(app.wait(100).is_empty());
        app.key(Char('o'));
        assert!(app.wait(299).is_empty());
        assert!(app.search_view().searching());
        assert_eq!(app.wait(1), [Action::Search("foo".into())]);
        assert!(app.search_view().searching());
        assert!(app.wait(300).is_empty());
    }

    #[test]
    fn each_keystroke_restarts_debounce() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("fo");
        assert!(app.wait(200).is_empty());
        app.key(Char('o'));
        assert!(app.wait(200).is_empty());
        assert_eq!(app.wait(100), [Action::Search("foo".into())]);
    }

    #[test]
    fn search_query_is_trimmed() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text(" foo ");
        assert_eq!(app.wait(300), [Action::Search("foo".into())]);
    }

    #[test]
    fn results_are_shown_and_stale_results_ignored() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("nir");
        assert_eq!(app.wait(300), [Action::Search("nir".into())]);
        app.type_text("vana");
        assert_eq!(app.wait(300), [Action::Search("nirvana".into())]);

        app.send(Event::Searched {
            query: "nir".into(),
            result: Ok(SearchResults {
                artists: vec![artist("Nirvana")],
                albums: vec![],
            }),
        });
        assert_eq!(app.search_view().results, None);
        assert!(app.search_view().searching());

        app.send(Event::Searched {
            query: "nirvana".into(),
            result: Ok(SearchResults {
                artists: vec![artist("Nirvana")],
                albums: vec![nevermind()],
            }),
        });
        let search = app.search_view();
        assert!(!search.searching());
        assert_eq!(
            search.items(),
            [
                SearchItem::Artist(&artist("Nirvana")),
                SearchItem::Album(&nevermind())
            ]
        );
    }

    #[test]
    fn new_results_reset_selection() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("nirvana", vec![artist("Nirvana")], vec![nevermind()]);
        app.key(Down);
        assert_eq!(app.search_view().selected, 1);
        app.key(Char('s'));
        assert_eq!(app.wait(300), [Action::Search("nirvanas".into())]);
        assert_eq!(app.search_view().selected, 1);
        app.send(Event::Searched {
            query: "nirvanas".into(),
            result: Ok(SearchResults {
                artists: vec![artist("Nirvanas")],
                albums: vec![nevermind()],
            }),
        });
        assert_eq!(app.search_view().selected, 0);
    }

    #[test]
    fn same_query_is_not_searched_again() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("foo", vec![], vec![colour()]);
        app.key(Backspace);
        app.key(Char('o'));
        assert!(app.wait(300).is_empty());
        assert_eq!(app.search_view().results.as_ref().unwrap().albums.len(), 1);
    }

    #[test]
    fn clearing_query_clears_results_without_search() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("ab", vec![artist("ABBA")], vec![]);
        app.keys(&[Backspace, Backspace]);
        assert_eq!(app.search_view().results, None);
        assert!(!app.search_view().searching());
        assert!(app.wait(300).is_empty());
        // The same search starts again after clearing.
        app.type_text("ab");
        assert_eq!(app.wait(300), [Action::Search("ab".into())]);
    }

    #[test]
    fn whitespace_only_query_is_not_searched() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("  ");
        assert!(app.wait(300).is_empty());
        assert!(!app.search_view().searching());
    }

    #[test]
    fn failed_search_shows_error_and_can_retry() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.type_text("foo");
        app.wait(300);
        app.send(Event::Searched {
            query: "foo".into(),
            result: Err("Spotify is not responding".into()),
        });
        assert_eq!(
            app.state.error.as_deref(),
            Some("Spotify is not responding")
        );
        assert!(!app.search_view().searching());
        app.key(Backspace);
        app.key(Char('o'));
        assert_eq!(app.wait(300), [Action::Search("foo".into())]);
    }

    #[test]
    fn search_up_down_moves_over_artists_then_albums() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search(
            "n",
            vec![artist("Nirvana"), artist("Nick Cave")],
            vec![nevermind()],
        );
        app.key(Up);
        assert_eq!(app.search_view().selected, 0);
        app.keys(&[Down, Down, Down, Down]);
        assert_eq!(app.search_view().selected, 2);
        app.key(Up);
        assert_eq!(app.search_view().selected, 1);
    }

    #[test]
    fn search_keys_without_results_do_nothing() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        assert!(app.keys(&[Up, Down, Enter, Ctrl('a')]).is_empty());
        assert_eq!(app.search_view().selected, 0);
    }

    #[test]
    fn enter_on_search_album_opens_its_tracks_without_playing() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("n", vec![artist("Nirvana")], vec![nevermind(), in_utero()]);
        app.keys(&[Down, Down]);
        assert_eq!(app.key(Enter), [load_tracks("inutero")]);
        assert_eq!(app.album_view().album, in_utero());
        // Esc returns to search, with the text and selection intact.
        app.key(Esc);
        assert_eq!(app.search_view().query, "n");
        assert_eq!(app.search_view().selected, 2);
    }

    #[test]
    fn ctrl_a_on_search_album_adds_to_shelf() {
        let mut app = App::new(vec![]);
        app.key(Ctrl('f'));
        app.search("n", vec![artist("Nirvana")], vec![nevermind()]);
        // On an artist Ctrl+A does nothing.
        assert!(app.key(Ctrl('a')).is_empty());
        app.key(Down);
        assert_eq!(app.key(Ctrl('a')), [Action::AddToShelf(nevermind())]);
        assert!(!app.state.on_shelf(&nevermind().uri));

        app.send(Event::ShelfChanged(vec![nevermind()]));
        assert!(app.state.on_shelf(&nevermind().uri));
        // Ctrl+A on an album already on the shelf removes it.
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf(nevermind().uri)]
        );
        app.search_view();
    }

    #[test]
    fn search_marks_albums_on_shelf() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search(
            "n",
            vec![],
            vec![nevermind(), album("x", "Nevermind (Live)", "Nirvana", None)],
        );
        let marked: Vec<bool> = app
            .search_view()
            .items()
            .iter()
            .map(|item| match item {
                SearchItem::Album(a) => app.state.on_shelf(&a.uri),
                SearchItem::Artist(_) => false,
            })
            .collect();
        assert_eq!(marked, [true, false]);
    }

    #[test]
    fn search_ctrl_d_does_nothing() {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("n", vec![], vec![nevermind()]);
        app.key(Down);
        assert!(app.key(Ctrl('d')).is_empty());
    }

    // --- Artist's albums from Spotify ---

    fn spotify_artist_app() -> App {
        let mut app = shelf_app();
        app.key(Ctrl('f'));
        app.search("nirvana", vec![artist("Nirvana")], vec![]);
        assert_eq!(
            app.key(Enter),
            [Action::LoadArtistAlbums("id-nirvana".into())]
        );
        app
    }

    fn unplugged() -> Album {
        album(
            "unplugged",
            "MTV Unplugged In New York",
            "Nirvana",
            Some(1994),
        )
    }

    #[test]
    fn enter_on_search_artist_loads_spotify_albums() {
        let mut app = spotify_artist_app();
        let view = app.artist_view();
        assert_eq!(view.artist, artist("Nirvana"));
        assert!(matches!(view.source, Source::Spotify(None)));
        assert!(app.artist_albums().is_empty());

        app.send(Event::ArtistAlbums {
            artist_id: "id-nirvana".into(),
            result: Ok(vec![unplugged(), in_utero(), bleach(), nevermind()]),
        });
        assert_eq!(
            app.artist_albums(),
            [
                "Bleach",
                "Nevermind",
                "In Utero",
                "MTV Unplugged In New York"
            ]
        );
    }

    #[test]
    fn albums_of_other_artist_are_ignored() {
        let mut app = spotify_artist_app();
        app.send(Event::ArtistAlbums {
            artist_id: "id-radiohead".into(),
            result: Ok(vec![ok_computer()]),
        });
        assert!(matches!(app.artist_view().source, Source::Spotify(None)));
    }

    #[test]
    fn failed_artist_albums_shows_error() {
        let mut app = spotify_artist_app();
        app.send(Event::ArtistAlbums {
            artist_id: "id-nirvana".into(),
            result: Err("Spotify is not responding".into()),
        });
        assert!(matches!(
            app.artist_view().source,
            Source::Spotify(Some(ref albums)) if albums.is_empty()
        ));
        assert_eq!(
            app.state.error.as_deref(),
            Some("Spotify is not responding")
        );
    }

    #[test]
    fn spotify_artist_keys() {
        let mut app = spotify_artist_app();
        app.send(Event::ArtistAlbums {
            artist_id: "id-nirvana".into(),
            result: Ok(vec![unplugged(), nevermind()]),
        });
        // Nevermind is on the shelf: Ctrl+A removes it, Ctrl+D does nothing.
        assert!(app.state.on_shelf(&nevermind().uri));
        assert!(!app.state.on_shelf(&unplugged().uri));
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf(nevermind().uri)]
        );
        assert!(app.key(Ctrl('d')).is_empty());

        app.key(Down);
        assert_eq!(app.key(Ctrl('a')), [Action::AddToShelf(unplugged())]);
        assert_eq!(app.key(Enter), [load_tracks("unplugged")]);
        assert_eq!(app.album_view().album, unplugged());
        app.key(Esc);

        app.send(Event::ShelfChanged(vec![
            in_utero(),
            ok_computer(),
            nevermind(),
            colour(),
            bleach(),
            unplugged(),
        ]));
        assert!(app.state.on_shelf(&unplugged().uri));
        assert_eq!(app.artist_view().selected, 1);
    }

    #[test]
    fn esc_from_spotify_artist_returns_to_search() {
        let mut app = spotify_artist_app();
        app.key(Esc);
        assert_eq!(app.search_view().query, "nirvana");
        app.key(Esc);
        assert!(app.is_shelf());
    }

    #[test]
    fn ctrl_f_from_spotify_artist_returns_to_search() {
        let mut app = spotify_artist_app();
        app.key(Ctrl('f'));
        assert_eq!(app.search_view().query, "nirvana");
        app.key(Esc);
        assert!(app.is_shelf());
    }

    // --- Keys everywhere ---

    /// When nothing is playing, ←/→ do nothing (play order: see Queue).
    #[test]
    fn player_keys_outside_search() {
        let mut app = shelf_app();
        for setup in [&[][..], &[Enter][..]] {
            app.keys(setup);
            assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
            assert!(app.keys(&[Left, Right]).is_empty());
            assert_eq!(app.key(Char('q')), [Action::Quit]);
        }
        app.artist_view();
    }

    #[test]
    fn player_keys_in_spotify_artist_view() {
        let mut app = spotify_artist_app();
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        assert!(app.keys(&[Left, Right]).is_empty());
        assert_eq!(app.key(Char('q')), [Action::Quit]);
    }

    #[test]
    fn ctrl_c_quits_everywhere() {
        let mut app = shelf_app();
        assert_eq!(app.key(Ctrl('c')), [Action::Quit]);
        app.key(Enter);
        assert_eq!(app.key(Ctrl('c')), [Action::Quit]);
        app.key(Ctrl('f'));
        assert_eq!(app.key(Ctrl('c')), [Action::Quit]);
    }

    #[test]
    fn other_keys_do_nothing() {
        let mut app = shelf_app();
        assert!(app.keys(&[Char('x'), Ctrl('x'), Backspace]).is_empty());
        app.key(Enter);
        assert!(app.keys(&[Char('x'), Ctrl('x'), Backspace]).is_empty());
        app.artist_view();
    }

    // --- Track list ---

    fn track(name: &str, secs: u64) -> Track {
        Track {
            uri: track_uri(name),
            name: name.to_owned(),
            artist: "Nirvana".to_owned(),
            duration: Duration::from_secs(secs),
        }
    }

    fn track_uri(name: &str) -> String {
        format!("spotify:track:{}", name.to_lowercase().replace(' ', "-"))
    }

    fn nevermind_tracks() -> Vec<Track> {
        vec![
            track("Smells Like Teen Spirit", 301),
            track("In Bloom", 254),
            track("Come As You Are", 218),
            track("Breed", 183),
        ]
    }

    fn album_tracks(album: &Album, result: Result<Vec<Track>, String>) -> Event {
        Event::AlbumTracks {
            uri: album.uri.clone(),
            result,
        }
    }

    /// The player started track `name`.
    fn started(name: &str) -> Event {
        Event::Player(player::Event::TrackChanged {
            uri: track_uri(name),
            artist: "Nirvana".into(),
            album: "Nevermind".into(),
            track: name.into(),
            duration: Duration::from_secs(200),
            cover: None,
        })
    }

    fn end_of_track() -> Event {
        Event::Player(player::Event::EndOfTrack)
    }

    /// Shelf › Nirvana › Nevermind open, tracks arrived. Nothing is playing.
    fn nevermind_app() -> App {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down]);
        assert_eq!(app.key(Enter), [load_tracks("nevermind")]);
        app.send(album_tracks(&nevermind(), Ok(nevermind_tracks())));
        app
    }

    #[test]
    fn album_tracks_arrive_without_playing() {
        let app = nevermind_app();
        let view = app.album_view();
        assert_eq!(view.tracks, Some(nevermind_tracks()));
        assert_eq!(view.selected, 0);
        assert_eq!(app.state.error, None);
        assert_eq!(app.state.now_playing, None);
        assert_eq!(app.state.playing_track(&nevermind().uri), None);
    }

    #[test]
    fn opening_the_playing_album_marks_its_playing_track() {
        let mut app = nevermind_app();
        app.keys(&[Down, Down, Enter]);
        app.send(started("Come As You Are"));
        app.keys(&[Esc, Esc]);
        // Opening does not restart playback.
        assert_eq!(app.keys(&[Enter, Down, Enter]), [load_tracks("nevermind")]);
        app.send(album_tracks(&nevermind(), Ok(nevermind_tracks())));
        assert_eq!(app.state.playing_track(&nevermind().uri), Some(2));
    }

    #[test]
    fn failed_playback_keeps_track_list() {
        let mut app = nevermind_app();
        app.key(Down);
        assert_eq!(app.key(Enter), [play("nevermind", 1)]);
        app.send(Event::Player(player::Event::Error(
            "Spotify is not responding".into(),
        )));
        assert_eq!(app.album_view().tracks, Some(nevermind_tracks()));
        assert_eq!(app.album_view().selected, 1);
        assert_eq!(
            app.state.error.as_deref(),
            Some("Spotify is not responding")
        );
    }

    #[test]
    fn tracks_of_other_album_are_ignored() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Down, Enter]);
        app.send(album_tracks(&bleach(), Ok(vec![track("Blew", 175)])));
        assert_eq!(app.album_view().tracks, None);
    }

    #[test]
    fn failed_album_shows_error_and_empty_list() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Enter]);
        app.send(album_tracks(
            &bleach(),
            Err("Spotify is not responding".into()),
        ));
        assert_eq!(app.album_view().tracks, Some(vec![]));
        assert_eq!(
            app.state.error.as_deref(),
            Some("Spotify is not responding")
        );
        // In an empty list Enter plays nothing.
        assert!(app.keys(&[Down, Enter]).is_empty());
    }

    #[test]
    fn enter_before_tracks_arrive_does_nothing() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Enter]);
        assert!(app.keys(&[Down, Enter]).is_empty());
        assert_eq!(app.album_view().selected, 0);
    }

    #[test]
    fn album_up_down_stay_in_bounds() {
        let mut app = nevermind_app();
        app.key(Up);
        assert_eq!(app.album_view().selected, 0);
        app.keys(&[Down; 6]);
        assert_eq!(app.album_view().selected, 3);
        app.key(Up);
        assert_eq!(app.album_view().selected, 2);
    }

    #[test]
    fn enter_on_track_plays_album_from_it() {
        let mut app = nevermind_app();
        app.keys(&[Down, Down]);
        assert_eq!(app.key(Enter), [play("nevermind", 2)]);
        // The list stays open and the selection in place.
        assert_eq!(app.album_view().selected, 2);
        app.keys(&[Up, Up]);
        assert_eq!(app.key(Enter), [play("nevermind", 0)]);
    }

    #[test]
    fn playing_mark_follows_track_changes() {
        let mut app = nevermind_app();
        let uri = nevermind().uri;
        assert_eq!(app.state.playing_track(&uri), None);

        app.key(Enter);
        // The mark shows only once the player has started the track.
        assert_eq!(app.state.playing_track(&uri), None);
        app.send(started("Smells Like Teen Spirit"));
        assert_eq!(app.state.playing_track(&uri), Some(0));
        assert_eq!(app.send(end_of_track()), [play("nevermind", 1)]);
        assert_eq!(app.state.playing_track(&uri), Some(1));
        assert_eq!(app.key(Right), [play("nevermind", 2)]);
        assert_eq!(app.state.playing_track(&uri), Some(2));
        // Another album is playing: this list has no playing track.
        app.keys(&[Esc, Up, Enter]);
        app.send(album_tracks(&bleach(), Ok(vec![track("Blew", 175)])));
        assert_eq!(app.key(Enter), [play("bleach", 0)]);
        assert_eq!(app.state.playing_track(&uri), None);
        assert_eq!(app.state.playing_track(&bleach().uri), Some(0));

        app.send(Event::Player(player::Event::Stopped));
        assert_eq!(app.state.playing_track(&bleach().uri), None);
    }

    #[test]
    fn esc_from_album_returns_to_artist_albums() {
        let mut app = nevermind_app();
        app.key(Down);
        app.key(Esc);
        let view = app.artist_view();
        assert_eq!(view.artist, artist("Nirvana"));
        assert_eq!(view.selected, 1);
        app.key(Esc);
        assert_eq!(app.shelf_selected(), 2);
    }

    #[test]
    fn esc_from_spotify_album_returns_to_spotify_artist() {
        let mut app = spotify_artist_app();
        app.send(Event::ArtistAlbums {
            artist_id: "id-nirvana".into(),
            result: Ok(vec![unplugged(), nevermind()]),
        });
        app.key(Down);
        assert_eq!(app.key(Enter), [load_tracks("unplugged")]);
        app.key(Esc);
        assert!(matches!(app.artist_view().source, Source::Spotify(Some(_))));
        assert_eq!(app.artist_view().selected, 1);
        app.key(Esc);
        assert_eq!(app.search_view().query, "nirvana");
    }

    #[test]
    fn player_keys_in_album_view() {
        let mut app = nevermind_app();
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        app.key(Enter);
        assert_eq!(app.key(Right), [play("nevermind", 1)]);
        assert_eq!(app.key(Left), [play("nevermind", 0)]);
        assert_eq!(app.key(Char('q')), [Action::Quit]);
        assert_eq!(app.key(Ctrl('c')), [Action::Quit]);
        assert!(app.keys(&[Char('x'), Backspace]).is_empty());
        app.album_view();
    }

    #[test]
    fn media_keys_control_playback_in_any_view() {
        let mut app = nevermind_app();
        app.key(Enter);
        app.send(started("Smells Like Teen Spirit"));
        app.send(Event::Player(player::Event::Position(0)));
        // In search Space types, but media keys control playback.
        app.key(Ctrl('f'));
        let media = |app: &mut App, command| app.send(Event::Media(command));
        assert_eq!(media(&mut app, MediaCommand::Toggle), [Action::TogglePause]);
        assert!(media(&mut app, MediaCommand::Play).is_empty());
        assert_eq!(media(&mut app, MediaCommand::Pause), [Action::TogglePause]);
        app.send(Event::Player(player::Event::Paused(1_000)));
        assert!(media(&mut app, MediaCommand::Pause).is_empty());
        assert_eq!(media(&mut app, MediaCommand::Play), [Action::TogglePause]);
        assert_eq!(media(&mut app, MediaCommand::Next), [play("nevermind", 1)]);
        assert_eq!(
            media(&mut app, MediaCommand::Previous),
            [play("nevermind", 0)]
        );
        assert_eq!(app.search_view().query, "");
    }

    #[test]
    fn ctrl_a_in_album_view_toggles_album_on_shelf() {
        let mut app = App::new(vec![]);
        app.key(Ctrl('f'));
        app.search("n", vec![], vec![nevermind()]);
        app.key(Enter);
        app.send(album_tracks(&nevermind(), Ok(nevermind_tracks())));
        app.key(Down);
        assert_eq!(app.key(Ctrl('a')), [Action::AddToShelf(nevermind())]);
        app.send(Event::ShelfChanged(vec![nevermind()]));
        // Ctrl+A on an album on the shelf removes it; Ctrl+D does nothing in the list.
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf(nevermind().uri)]
        );
        assert!(app.key(Ctrl('d')).is_empty());
        assert_eq!(app.album_view().selected, 1);
    }

    #[test]
    fn ctrl_f_from_album_view_opens_search() {
        let mut app = nevermind_app();
        app.key(Ctrl('f'));
        app.search_view();
        app.key(Esc);
        assert_eq!(app.album_view().album, nevermind());
    }

    // --- Queue ---

    fn play_track(name: &str) -> Action {
        Action::Play(Item::Track(track_uri(name)))
    }

    fn bleach_tracks() -> Vec<Track> {
        vec![track("Blew", 175), track("Floyd The Barber", 138)]
    }

    fn queue_names(app: &App) -> Vec<&str> {
        app.state.queue().iter().map(|t| t.name.as_str()).collect()
    }

    /// Nevermind playing from track `track`.
    fn nevermind_playing(track: usize) -> App {
        let mut app = nevermind_app();
        app.keys(&vec![Down; track]);
        assert_eq!(app.key(Enter), [play("nevermind", track)]);
        app.send(started("track"));
        app
    }

    /// Adds the selected track to the queue in the track list.
    fn queue_track(app: &mut App, index: usize) -> Vec<Action> {
        while app.album_view().selected > index {
            app.key(Up);
        }
        while app.album_view().selected < index {
            app.key(Down);
        }
        app.key(Ctrl('e'))
    }

    fn queue_bleach(app: &mut App) -> Vec<Action> {
        app.send(Event::AlbumQueued {
            album: bleach(),
            result: Ok(bleach_tracks()),
        })
    }

    fn queue_view_selected(app: &App) -> usize {
        match app.state.view() {
            View::Queue { selected, .. } => *selected,
            other => panic!("expected the queue, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_e_queues_selected_track_with_short_notice() {
        let mut app = nevermind_playing(0);
        assert!(queue_track(&mut app, 1).is_empty());
        assert_eq!(queue_names(&app), ["In Bloom"]);
        let queued = &app.state.queue()[0];
        assert_eq!(queued.artist, "Nirvana");
        assert_eq!(queued.duration, Duration::from_secs(254));
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Queued, "In Bloom"))
        );
        app.wait(2_900);
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Queued, "In Bloom"))
        );
        app.wait(100);
        assert_eq!(app.state.notice(app.now), None);
        // The list stays open and the selection in place.
        assert_eq!(app.album_view().selected, 1);
    }

    #[test]
    fn ctrl_e_before_tracks_arrive_does_nothing() {
        let mut app = shelf_app();
        app.keys(&[Down, Enter, Enter]);
        assert!(app.key(Ctrl('e')).is_empty());
        assert!(app.state.queue().is_empty());
    }

    #[test]
    fn ctrl_e_on_album_lists_queues_whole_album() {
        let mut app = shelf_app();
        // On the shelf (artists) Ctrl+E does nothing.
        assert!(app.key(Ctrl('e')).is_empty());
        app.keys(&[Down, Enter]);
        assert_eq!(app.key(Ctrl('e')), [Action::QueueAlbum(bleach())]);

        let mut app = spotify_artist_app();
        app.send(Event::ArtistAlbums {
            artist_id: "id-nirvana".into(),
            result: Ok(vec![unplugged(), nevermind()]),
        });
        app.key(Down);
        assert_eq!(app.key(Ctrl('e')), [Action::QueueAlbum(unplugged())]);

        let mut app = App::new(vec![]);
        app.key(Ctrl('f'));
        app.search("n", vec![artist("Nirvana")], vec![nevermind()]);
        // An artist cannot be queued, an album can.
        assert!(app.key(Ctrl('e')).is_empty());
        app.key(Down);
        assert_eq!(app.key(Ctrl('e')), [Action::QueueAlbum(nevermind())]);
        assert_eq!(app.search_view().query, "n");
    }

    #[test]
    fn queued_album_tracks_are_added_in_order() {
        let mut app = nevermind_playing(0);
        assert!(queue_bleach(&mut app).is_empty());
        assert_eq!(queue_names(&app), ["Blew", "Floyd The Barber"]);
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Queued, "Bleach"))
        );
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn failed_album_queueing_shows_error() {
        let mut app = nevermind_playing(0);
        let actions = app.send(Event::AlbumQueued {
            album: bleach(),
            result: Err("could not queue the album: x".into()),
        });
        assert!(actions.is_empty());
        assert!(app.state.queue().is_empty());
        assert_eq!(app.state.notice(app.now), None);
        assert_eq!(
            app.state.error.as_deref(),
            Some("could not queue the album: x")
        );
    }

    #[test]
    fn queue_plays_after_current_track_then_album_continues() {
        let mut app = nevermind_playing(0);
        queue_track(&mut app, 3);
        queue_bleach(&mut app);
        assert_eq!(app.send(end_of_track()), [play_track("Breed")]);
        assert_eq!(queue_names(&app), ["Blew", "Floyd The Barber"]);
        assert_eq!(app.send(end_of_track()), [play_track("Blew")]);
        assert_eq!(app.send(end_of_track()), [play_track("Floyd The Barber")]);
        assert!(app.state.queue().is_empty());
        // The album continues where it left off.
        assert_eq!(app.send(end_of_track()), [play("nevermind", 1)]);
        assert_eq!(app.send(end_of_track()), [play("nevermind", 2)]);
        assert_eq!(app.send(end_of_track()), [play("nevermind", 3)]);
        // When the album ends, playback stops.
        assert_eq!(app.send(end_of_track()), [Action::Stop]);
        assert_eq!(app.state.now_playing, None);
        assert!(app.keys(&[Left, Right]).is_empty());
    }

    #[test]
    fn tracks_queued_while_queue_plays_go_to_its_end() {
        let mut app = nevermind_playing(0);
        queue_track(&mut app, 3);
        app.send(end_of_track());
        queue_track(&mut app, 2);
        assert_eq!(app.send(end_of_track()), [play_track("Come As You Are")]);
        assert_eq!(app.send(end_of_track()), [play("nevermind", 1)]);
    }

    #[test]
    fn right_plays_queue_first() {
        let mut app = nevermind_playing(1);
        queue_track(&mut app, 3);
        assert_eq!(app.key(Right), [play_track("Breed")]);
        assert_eq!(app.key(Right), [play("nevermind", 2)]);
        assert_eq!(app.key(Right), [play("nevermind", 3)]);
        // On the last track → does nothing.
        assert!(app.key(Right).is_empty());
    }

    #[test]
    fn left_walks_back_through_queue_to_album() {
        let mut app = nevermind_playing(1);
        queue_track(&mut app, 3);
        queue_bleach(&mut app);
        app.keys(&[Right, Right]);
        assert_eq!(queue_names(&app), ["Floyd The Barber"]);
        // The previous queue track, and the played one goes back to the front of the queue.
        assert_eq!(app.key(Left), [play_track("Breed")]);
        assert_eq!(queue_names(&app), ["Blew", "Floyd The Barber"]);
        // The album track after which the queue started.
        assert_eq!(app.key(Left), [play("nevermind", 1)]);
        assert_eq!(queue_names(&app), ["Breed", "Blew", "Floyd The Barber"]);
        assert_eq!(app.key(Left), [play("nevermind", 0)]);
        assert_eq!(app.key(Left), [play("nevermind", 0)]);
        // Going forward, the queue plays first again.
        assert_eq!(app.key(Right), [play_track("Breed")]);
    }

    #[test]
    fn ctrl_e_when_nothing_plays_starts_the_queue() {
        let mut app = nevermind_app();
        assert_eq!(queue_track(&mut app, 1), [play_track("In Bloom")]);
        assert!(app.state.queue().is_empty());
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Queued, "In Bloom"))
        );
        app.send(started("In Bloom"));
        // The playing track is marked in its album's list.
        assert_eq!(app.state.playing_track(&nevermind().uri), Some(1));
        assert!(queue_track(&mut app, 2).is_empty());
        // On the first track ← starts it over.
        assert_eq!(app.key(Left), [play_track("In Bloom")]);
        assert_eq!(queue_names(&app), ["Come As You Are"]);
        assert_eq!(app.send(end_of_track()), [play_track("Come As You Are")]);
        // After the queue there is no album to continue.
        assert_eq!(app.send(end_of_track()), [Action::Stop]);
        assert_eq!(app.state.now_playing, None);
    }

    #[test]
    fn queued_album_starts_when_nothing_plays() {
        let mut app = shelf_app();
        assert_eq!(queue_bleach(&mut app), [play_track("Blew")]);
        assert_eq!(queue_names(&app), ["Floyd The Barber"]);
    }

    #[test]
    fn paused_counts_as_playing() {
        let mut app = nevermind_playing(0);
        app.send(Event::Player(player::Event::Paused(0)));
        assert!(queue_track(&mut app, 2).is_empty());
        assert_eq!(queue_names(&app), ["Come As You Are"]);
    }

    #[test]
    fn playing_another_album_keeps_the_queue() {
        let mut app = nevermind_playing(0);
        queue_bleach(&mut app);
        app.keys(&[Down, Down]);
        assert_eq!(app.key(Enter), [play("nevermind", 2)]);
        assert_eq!(app.send(end_of_track()), [play_track("Blew")]);
        assert_eq!(app.send(end_of_track()), [play_track("Floyd The Barber")]);
        assert_eq!(app.send(end_of_track()), [play("nevermind", 3)]);
    }

    #[test]
    fn player_stopping_keeps_the_queue() {
        let mut app = nevermind_playing(0);
        queue_track(&mut app, 2);
        app.send(Event::Player(player::Event::Stopped));
        assert_eq!(app.state.now_playing, None);
        assert_eq!(app.state.playing_track(&nevermind().uri), None);
        assert!(app.keys(&[Left, Right]).is_empty());
        assert_eq!(queue_names(&app), ["Come As You Are"]);
    }

    #[test]
    fn preload_asks_for_the_upcoming_track() {
        let preload = || Event::Player(player::Event::PreloadNext);
        let mut app = nevermind_app();
        assert!(app.send(preload()).is_empty());

        let mut app = nevermind_playing(2);
        assert_eq!(
            app.send(preload()),
            [Action::Preload(Item::Album {
                uri: nevermind().uri,
                track: 3
            })]
        );
        queue_track(&mut app, 0);
        assert_eq!(
            app.send(preload()),
            [Action::Preload(Item::Track(track_uri(
                "Smells Like Teen Spirit"
            )))]
        );
        // Preloading does not change the order.
        assert_eq!(queue_names(&app), ["Smells Like Teen Spirit"]);
        app.send(end_of_track());
        app.send(end_of_track());
        assert!(app.send(preload()).is_empty());
    }

    #[test]
    fn ctrl_q_opens_queue_view_and_esc_returns() {
        let mut app = nevermind_playing(0);
        app.key(Ctrl('q'));
        assert_eq!(queue_view_selected(&app), 0);
        // A second Ctrl+Q does not stack the view.
        app.key(Ctrl('q'));
        app.key(Esc);
        assert_eq!(app.album_view().album, nevermind());

        app.key(Ctrl('f'));
        app.key(Ctrl('q'));
        queue_view_selected(&app);
        app.key(Esc);
        app.search_view();
    }

    #[test]
    fn queue_view_selects_and_removes() {
        let mut app = nevermind_playing(0);
        for index in [1, 2, 3] {
            queue_track(&mut app, index);
        }
        app.key(Ctrl('q'));
        app.key(Up);
        assert_eq!(queue_view_selected(&app), 0);
        app.keys(&[Down; 5]);
        assert_eq!(queue_view_selected(&app), 2);
        app.key(Up);
        assert!(app.key(Ctrl('d')).is_empty());
        assert_eq!(queue_names(&app), ["In Bloom", "Breed"]);
        assert_eq!(queue_view_selected(&app), 1);
        // Removing the last one moves the selection to the previous one.
        app.key(Ctrl('d'));
        assert_eq!(queue_names(&app), ["In Bloom"]);
        assert_eq!(queue_view_selected(&app), 0);
        app.key(Ctrl('d'));
        assert!(app.state.queue().is_empty());
        assert!(app.keys(&[Ctrl('d'), Down, Enter, Ctrl('e')]).is_empty());
        // After the queue empties, the album continues.
        assert_eq!(app.send(end_of_track()), [play("nevermind", 1)]);
    }

    #[test]
    fn queue_view_selection_follows_shrinking_queue() {
        let mut app = nevermind_playing(0);
        queue_track(&mut app, 1);
        queue_track(&mut app, 2);
        app.key(Ctrl('q'));
        app.key(Down);
        assert_eq!(queue_view_selected(&app), 1);
        app.send(end_of_track());
        assert_eq!(queue_names(&app), ["Come As You Are"]);
        assert_eq!(queue_view_selected(&app), 0);
    }

    #[test]
    fn player_keys_in_queue_view() {
        let mut app = nevermind_playing(0);
        queue_track(&mut app, 2);
        app.key(Ctrl('q'));
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        assert_eq!(app.key(Right), [play_track("Come As You Are")]);
        assert_eq!(app.key(Char('q')), [Action::Quit]);
    }

    // --- Now playing and errors ---

    fn track_changed() -> Event {
        Event::Player(player::Event::TrackChanged {
            uri: "spotify:track:everlong".into(),
            artist: "Foo Fighters".into(),
            album: "The Colour And The Shape".into(),
            track: "Everlong".into(),
            duration: Duration::from_secs(250),
            cover: Some("https://i.scdn.co/image/everlong".into()),
        })
    }

    #[test]
    fn player_events_update_now_playing() {
        let mut app = shelf_app();
        app.send(track_changed());
        let now = app.state.now_playing.clone().unwrap();
        assert_eq!(now.track, "Everlong");
        assert_eq!(now.duration, Duration::from_secs(250));
        assert!(!now.paused);
        assert_eq!(now.elapsed(app.now), Duration::ZERO);

        app.send(Event::Player(player::Event::Position(83_000)));
        assert_eq!(
            app.state.now_playing.as_ref().unwrap().elapsed(app.now),
            Duration::from_secs(83)
        );

        app.send(Event::Player(player::Event::Paused(83_000)));
        assert!(app.state.now_playing.as_ref().unwrap().paused);

        app.send(Event::Player(player::Event::Position(83_000)));
        assert!(!app.state.now_playing.as_ref().unwrap().paused);

        app.send(Event::Player(player::Event::Stopped));
        assert_eq!(app.state.now_playing, None);
    }

    #[test]
    fn elapsed_runs_while_playing_and_stops_when_paused() {
        let mut app = shelf_app();
        app.send(track_changed());
        app.send(Event::Player(player::Event::Position(10_000)));
        app.wait(400);
        let now = app.state.now_playing.clone().unwrap();
        assert_eq!(now.elapsed(app.now), Duration::from_millis(10_400));

        // When paused, the time is the position the player reported.
        app.send(Event::Player(player::Event::Paused(10_400)));
        app.wait(5_000);
        let now = app.state.now_playing.clone().unwrap();
        assert_eq!(now.elapsed(app.now), Duration::from_millis(10_400));

        // Never beyond the track's duration.
        app.send(Event::Player(player::Event::Position(249_900)));
        app.wait(1_000);
        let now = app.state.now_playing.clone().unwrap();
        assert_eq!(now.elapsed(app.now), Duration::from_secs(250));
    }

    #[test]
    fn now_playing_info_follows_track_and_pause() {
        let mut app = shelf_app();
        assert_eq!(app.state.now_playing_info(app.now), None);
        app.send(track_changed());
        app.send(Event::Player(player::Event::Position(83_000)));
        app.wait(2_000);
        let expected = now_playing::Info {
            title: "Everlong".into(),
            artist: "Foo Fighters".into(),
            album: "The Colour And The Shape".into(),
            duration_ms: 250_000,
            position_ms: 85_000,
            playing: true,
            cover: Some("https://i.scdn.co/image/everlong".into()),
        };
        assert_eq!(app.state.now_playing_info(app.now), Some(expected.clone()));

        app.send(Event::Player(player::Event::Paused(85_000)));
        app.wait(5_000);
        assert_eq!(
            app.state.now_playing_info(app.now),
            Some(now_playing::Info {
                playing: false,
                ..expected
            })
        );

        app.send(Event::Player(player::Event::Stopped));
        assert_eq!(app.state.now_playing_info(app.now), None);
    }

    #[test]
    fn error_stays_until_next_success() {
        let mut app = shelf_app();
        app.send(Event::Player(player::Event::Error(
            "playback failed".into(),
        )));
        assert_eq!(app.state.error.as_deref(), Some("playback failed"));
        // Position updates are not actions: the error stays visible.
        app.send(Event::Player(player::Event::Position(1_000)));
        app.wait(300);
        assert!(app.state.error.is_some());

        app.send(track_changed());
        assert_eq!(app.state.error, None);

        app.send(Event::Error("cannot save the shelf".into()));
        app.send(Event::ShelfChanged(vec![colour()]));
        assert_eq!(app.state.error, None);

        app.send(Event::Error("x".into()));
        app.send(Event::Player(player::Event::Paused(0)));
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn successful_search_clears_error() {
        let mut app = shelf_app();
        app.send(Event::Error("x".into()));
        app.key(Ctrl('f'));
        app.search("foo", vec![], vec![]);
        assert_eq!(app.state.error, None);
    }

    // --- Saving and restoring playback ---

    /// Nevermind playing from In Bloom at 1:23, with Come As You Are and
    /// Breed in the queue.
    fn in_bloom_with_queue() -> App {
        let mut app = nevermind_playing(1);
        app.send(started("In Bloom"));
        queue_track(&mut app, 2);
        queue_track(&mut app, 3);
        app.send(Event::Player(player::Event::Position(83_000)));
        app
    }

    /// Quitting with `q`: the event loop saves `playback` before quitting.
    fn quit(app: &mut App) -> SavedPlayback {
        assert_eq!(app.key(Char('q')), [Action::Quit]);
        app.state.playback(app.now).expect("something is playing")
    }

    /// A new Deck that restores the saved playback state.
    fn restored(saved: SavedPlayback) -> (App, Vec<Action>) {
        let mut app = shelf_app();
        let actions = app.send(Event::Restore(Box::new(saved)));
        (app, actions)
    }

    #[test]
    fn saved_playback_has_album_track_position_and_queue() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let json = serde_json::to_value(&saved).unwrap();
        let nevermind = serde_json::to_value(nevermind()).unwrap();
        let queued = |name: &str, secs: u64, index: usize| {
            serde_json::json!({
                "uri": track_uri(name),
                "name": name,
                "artist": "Nirvana",
                "duration_ms": secs * 1000,
                "album_uri": "spotify:album:nevermind",
                "index": index,
                "album": nevermind,
            })
        };
        assert_eq!(
            json,
            serde_json::json!({
                "album": {
                    "uri": "spotify:album:nevermind",
                    "len": 4,
                    "index": 1,
                    "album": nevermind,
                },
                "current": "album",
                "played": [],
                "queue": [queued("Come As You Are", 218, 2), queued("Breed", 183, 3)],
                "now_playing": {
                    "uri": track_uri("In Bloom"),
                    "artist": "Nirvana",
                    "album": "Nevermind",
                    "track": "In Bloom",
                    "duration_ms": 200_000,
                },
                "position_ms": 83_000,
            })
        );
        // The same state when read from the file.
        assert_eq!(
            serde_json::from_value::<SavedPlayback>(json).unwrap(),
            saved
        );
    }

    #[test]
    fn restore_shows_track_stopped_at_saved_position_with_queue() {
        let mut app = in_bloom_with_queue();
        let (app, actions) = restored(quit(&mut app));
        assert_eq!(
            actions,
            [Action::Restore {
                item: Item::Album {
                    uri: nevermind().uri,
                    track: 1,
                },
                position: Duration::from_secs(83),
            }]
        );
        // The status line shows right away, before the player has loaded the track.
        let playing = app.state.now_playing.clone().unwrap();
        assert_eq!(playing.track, "In Bloom");
        assert!(playing.paused);
        assert_eq!(playing.elapsed(app.now), Duration::from_secs(83));
        assert_eq!(queue_names(&app), ["Come As You Are", "Breed"]);
        assert_eq!(app.state.playing_track(&nevermind().uri), Some(1));
        assert!(app.is_shelf());
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn restored_track_continues_with_queue_then_album() {
        let mut app = in_bloom_with_queue();
        let (mut app, _) = restored(quit(&mut app));
        // The player loaded the track paused at the same position.
        app.send(started("In Bloom"));
        app.send(Event::Player(player::Event::Paused(83_000)));
        let playing = app.state.now_playing.clone().unwrap();
        assert!(playing.paused);
        assert_eq!(playing.elapsed(app.now), Duration::from_secs(83));

        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        app.send(Event::Player(player::Event::Position(83_000)));
        assert_eq!(app.send(end_of_track()), [play_track("Come As You Are")]);
        assert_eq!(app.send(end_of_track()), [play_track("Breed")]);
        assert_eq!(app.send(end_of_track()), [play("nevermind", 2)]);
    }

    #[test]
    fn arrows_work_on_restored_track_as_when_paused() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let (mut app, _) = restored(saved.clone());
        assert_eq!(app.key(Right), [play_track("Come As You Are")]);
        let (mut app, _) = restored(saved);
        assert_eq!(app.key(Left), [play("nevermind", 0)]);
    }

    #[test]
    fn restored_queue_track_plays_album_after_queue() {
        let mut app = in_bloom_with_queue();
        app.send(end_of_track());
        app.send(started("Come As You Are"));
        let saved = quit(&mut app);
        let (mut app, actions) = restored(saved);
        assert_eq!(
            actions,
            [Action::Restore {
                item: Item::Track(track_uri("Come As You Are")),
                position: Duration::ZERO,
            }]
        );
        assert_eq!(queue_names(&app), ["Breed"]);
        assert_eq!(app.state.playing_track(&nevermind().uri), Some(2));
        // ← goes back to the album track after which the queue started.
        assert_eq!(app.key(Left), [play("nevermind", 1)]);
    }

    #[test]
    fn nothing_playing_saves_nothing() {
        let mut app = shelf_app();
        assert_eq!(app.key(Char('q')), [Action::Quit]);
        assert_eq!(app.state.playback(app.now), None);
        assert!(!app.state.playback_changed());
    }

    #[test]
    fn playback_is_saved_when_track_or_queue_changes() {
        let mut app = nevermind_app();
        assert!(!app.state.playback_changed());

        app.key(Enter);
        assert!(app.state.playback_changed());
        app.send(started("Smells Like Teen Spirit"));
        assert!(app.state.playback_changed());
        assert!(!app.state.playback_changed());

        // Position and pause are not changes: the position is saved on quit.
        app.send(Event::Player(player::Event::Position(30_000)));
        app.send(Event::Player(player::Event::Paused(30_000)));
        assert!(!app.state.playback_changed());

        queue_track(&mut app, 3);
        assert!(app.state.playback_changed());
        app.key(Ctrl('q'));
        app.key(Ctrl('d'));
        assert!(app.state.playback_changed());

        // The album ended: the state is cleared.
        app.key(Esc);
        for _ in 0..4 {
            app.send(end_of_track());
        }
        assert_eq!(app.state.now_playing, None);
        assert!(app.state.playback_changed());
        assert_eq!(app.state.playback(app.now), None);
    }

    #[test]
    fn tick_and_position_do_not_build_playback_but_next_track_is_a_change() {
        let mut app = nevermind_app();
        app.key(Enter);
        app.send(started("Smells Like Teen Spirit"));
        assert!(app.state.playback_changed());

        // Tick, position and pause do not even mark the state for building.
        app.wait(100);
        app.send(Event::Player(player::Event::Position(1_000)));
        app.send(Event::Player(player::Event::Paused(1_000)));
        assert!(!app.state.playback_dirty);
        assert!(!app.state.playback_changed());

        app.key(Right);
        assert!(app.state.playback_changed());
        app.wait(100);
        assert!(!app.state.playback_changed());
    }

    #[test]
    fn restore_is_not_a_change_to_save() {
        let mut app = in_bloom_with_queue();
        let (mut app, _) = restored(quit(&mut app));
        assert!(!app.state.playback_changed());
    }

    #[test]
    fn quitting_before_restored_track_loads_keeps_position() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let (mut app, _) = restored(saved.clone());
        app.wait(5_000);
        assert_eq!(quit(&mut app), saved);
    }

    #[test]
    fn failed_restore_starts_empty_with_error() {
        let mut app = in_bloom_with_queue();
        let (mut app, _) = restored(quit(&mut app));
        app.send(Event::Player(player::Event::Error(
            "could not load the album's tracks: 404".into(),
        )));
        assert_eq!(
            app.state.error.as_deref(),
            Some("could not restore playback: could not load the album's tracks: 404")
        );
        assert_eq!(app.state.now_playing, None);
        assert!(app.state.queue().is_empty());
        assert!(app.state.playback_changed());
        assert_eq!(app.state.playback(app.now), None);
        assert!(app.key(Right).is_empty());
        assert!(app.key(Left).is_empty());
    }

    #[test]
    fn startup_error_stays_while_restored_track_loads() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let mut app = shelf_app();
        app.send(Event::Error("shelf file is broken".into()));
        app.send(Event::Restore(Box::new(saved)));
        app.send(started("In Bloom"));
        app.send(Event::Player(player::Event::Paused(83_000)));
        assert_eq!(app.state.error.as_deref(), Some("shelf file is broken"));
        assert!(app.state.now_playing.as_ref().unwrap().paused);
    }

    #[test]
    fn error_after_restored_track_loads_keeps_playback() {
        let mut app = in_bloom_with_queue();
        let (mut app, _) = restored(quit(&mut app));
        app.send(started("In Bloom"));
        app.send(Event::Player(player::Event::Paused(83_000)));
        app.send(Event::Player(player::Event::Error("x".into())));
        assert_eq!(app.state.error.as_deref(), Some("x"));
        assert_eq!(queue_names(&app), ["Come As You Are", "Breed"]);
        assert!(app.state.now_playing.is_some());

        // Playing another track while loading also ends the restore.
        let mut app = in_bloom_with_queue();
        let (mut app, _) = restored(quit(&mut app));
        app.key(Right);
        app.send(Event::Player(player::Event::Error("x".into())));
        assert_eq!(queue_names(&app), ["Breed"]);
    }

    #[test]
    fn invalid_saved_playback_starts_empty_with_error() {
        let saved: SavedPlayback = serde_json::from_value(serde_json::json!({
            "album": null,
            "current": "album",
            "now_playing": null,
            "position_ms": 0,
        }))
        .unwrap();
        let (app, actions) = restored(saved);
        assert!(actions.is_empty());
        assert_eq!(
            app.state.error.as_deref(),
            Some("could not restore playback: the saved state is invalid")
        );
        assert_eq!(app.state.now_playing, None);
        assert_eq!(app.state.playback(app.now), None);
    }

    // --- Genres and radio ---

    fn lofi() -> &'static Genre {
        genres::find("lofi").unwrap()
    }

    fn synthwave() -> &'static Genre {
        genres::find("synthwave").unwrap()
    }

    /// Shelf and favourite genres; the selection on the top list row.
    fn genre_app(favorites: &[&str]) -> App {
        let mut app = App::at_top(shelf_app().state.shelf);
        app.state = State::new(app.state.shelf.clone())
            .with_favorites(favorites.iter().map(|id| id.to_string()).collect());
        app
    }

    fn radio_track(n: usize) -> radio::Track {
        radio::Track {
            uri: format!("spotify:track:r{n}"),
            name: format!("Song {n}"),
            artist: format!("Artist {n}"),
            artist_uri: format!("spotify:artist:a{n}"),
            album: radio::Album {
                uri: format!("spotify:album:record{n}"),
                name: format!("Record {n}"),
            },
        }
    }

    fn station(range: std::ops::Range<usize>) -> Vec<radio::Track> {
        range.map(radio_track).collect()
    }

    fn radio_play(n: usize) -> Action {
        Action::Play(Item::Track(format!("spotify:track:r{n}")))
    }

    /// The only radio fetch among the actions.
    fn radio_request(actions: &[Action]) -> RadioRequest {
        let requests: Vec<&RadioRequest> = actions
            .iter()
            .filter_map(|a| match a {
                Action::LoadRadio(request) => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1, "{actions:?}");
        requests[0].clone()
    }

    fn radio_loaded(request: &RadioRequest, tracks: Vec<radio::Track>) -> Event {
        Event::RadioLoaded {
            request: request.clone(),
            result: Ok(tracks),
        }
    }

    /// Enter on the selected genre and station `tracks`: the radio plays the first track.
    fn start_radio(app: &mut App, tracks: Vec<radio::Track>) -> RadioRequest {
        let request = radio_request(&app.key(Enter));
        let first = tracks[0].uri.clone();
        let actions = app.send(radio_loaded(&request, tracks));
        assert_eq!(actions, [Action::Play(Item::Track(first))]);
        app.send(started("Song"));
        request
    }

    fn radio_of(app: &App) -> &RadioPosition {
        app.state.playback.radio.as_ref().expect("radio playing")
    }

    fn radio_view(app: &App) -> &RadioView {
        match app.state.view() {
            View::Radio(view) => view,
            other => panic!("expected the radio, got {other:?}"),
        }
    }

    /// Plays the radio forward until track `n` plays. Returns the actions.
    fn play_until(app: &mut App, n: usize) -> Vec<Action> {
        let mut actions = Vec::new();
        while radio_of(app).index < n {
            actions = app.send(end_of_track());
            app.send(started("Song"));
        }
        actions
    }

    #[test]
    fn favorite_genres_come_after_curated_and_before_genres_row() {
        let app = genre_app(&["synthwave", "polka-noir", "lofi"]);
        assert_eq!(
            app.state.shelf_lists(),
            [
                ShelfRow::Genre(lofi()),
                ShelfRow::Genre(synthwave()),
                ShelfRow::Genres
            ]
        );

        let state = State::new(Vec::new())
            .with_favorites(vec!["lofi".into()])
            .with_curated(Ok(Some(three_curated())));
        assert!(matches!(state.shelf_lists()[0], ShelfRow::Curated(_)));
        assert_eq!(state.shelf_lists()[1], ShelfRow::Genre(lofi()));
        assert!(matches!(state.view(), View::Shelf { selected: 0, .. }));
    }

    #[test]
    fn enter_on_genre_starts_radio_and_opens_it_when_the_station_arrives() {
        let mut app = genre_app(&["lofi", "synthwave"]);
        let request = radio_request(&app.key(Enter));
        assert_eq!(request.genre, "lofi");
        assert_eq!(request.kind, RadioLoad::Station);
        assert!(request.previous.is_empty());
        assert!(lofi().seeds.iter().any(|s| s.uri == request.seed));
        // The view changes only once the station has been fetched.
        assert!(app.is_shelf());

        let actions = app.send(radio_loaded(&request, station(0..50)));
        assert_eq!(actions, [radio_play(0)]);
        assert_eq!(radio_view(&app).genre, lofi());
        assert_eq!(radio_view(&app).selected, 0);
        assert_eq!(app.state.playing_genre(), Some("lofi"));
        assert_eq!(app.state.radio_tracks("lofi").unwrap().len(), 50);
        assert_eq!(app.state.radio_tracks("synthwave"), None);
        assert_eq!(radio_of(&app).seeds, [request.seed]);

        // Esc returns to the shelf.
        assert_eq!(app.key(Esc), [Action::LoadLists]);
        assert!(app.is_shelf());
    }

    #[test]
    fn enter_on_playing_genre_only_opens_the_radio() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        play_until(&mut app, 2);
        app.key(Esc);
        // When opened, the selection is on the playing track.
        assert!(app.key(Enter).is_empty());
        assert_eq!(radio_view(&app).selected, 2);
        assert_eq!(app.state.views.len(), 2);
    }

    #[test]
    fn failed_station_changes_nothing() {
        let mut app = nevermind_playing(0);
        app.state.views.truncate(1);
        app.state.favorites = vec!["lofi".into()];
        if let Some(View::Shelf { selected, .. }) = app.state.views.last_mut() {
            *selected = 0;
        }
        let request = radio_request(&app.key(Enter));
        let actions = app.send(Event::RadioLoaded {
            request,
            result: Err("could not load the radio: offline".into()),
        });
        assert!(actions.is_empty());
        assert_eq!(
            app.state.error.as_deref(),
            Some("could not load the radio: offline")
        );
        assert!(app.is_shelf());
        assert_eq!(app.state.playing_genre(), None);
        assert_eq!(app.state.playing_track("spotify:album:nevermind"), Some(0));

        // A stale result is ignored: only the latest request counts. Fixed random state so
        // that the requests get different seeds.
        app.state.rng = 1;
        let first = radio_request(&app.key(Enter));
        let second = radio_request(&app.key(Enter));
        assert_ne!(first, second);
        assert!(app.send(radio_loaded(&first, station(0..50))).is_empty());
        assert_eq!(
            app.send(radio_loaded(&second, station(0..50))),
            [radio_play(0)]
        );
        // A new radio replaces the album.
        assert_eq!(app.state.playing_track("spotify:album:nevermind"), None);
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn ctrl_d_on_genre_row_removes_it_from_home() {
        let mut app = genre_app(&["lofi", "synthwave"]);
        assert_eq!(app.key(Ctrl('d')), [Action::RemoveFavorite("lofi".into())]);
        app.send(Event::FavoritesChanged(vec!["synthwave".into()]));
        // The selection stays at the same position: the next row.
        assert_eq!(app.shelf_selected(), 0);
        assert_eq!(app.state.shelf_lists()[0], ShelfRow::Genre(synthwave()));
        // Ctrl+D on the Genres row or on an artist does not remove a genre.
        app.key(Down);
        assert!(app.key(Ctrl('d')).is_empty());

        // A selection on an artist stays on the artist when more genre rows appear.
        app.keys(&[Down, Down]);
        assert_eq!(app.shelf_selected(), 3);
        app.send(Event::FavoritesChanged(vec![
            "lofi".into(),
            "synthwave".into(),
        ]));
        assert_eq!(app.shelf_selected(), 4);
        app.key(Enter);
        assert_eq!(app.artist_view().artist, artist("Nirvana"));
    }

    #[test]
    fn more_genres_lists_catalog_and_ctrl_a_adds_synthwave_to_home() {
        let mut app = genre_app(&[]);
        assert_eq!(app.state.shelf_lists(), [ShelfRow::Genres]);
        assert!(app.key(Enter).is_empty());
        assert!(matches!(app.state.view(), View::Genres { selected: 0, .. }));

        let index = genres::all()
            .iter()
            .position(|g| g.id == "synthwave")
            .unwrap();
        app.keys(&vec![Down; index]);
        assert!(matches!(app.state.view(), View::Genres { selected, .. } if *selected == index));
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::AddFavorite("synthwave".into())]
        );
        app.send(Event::FavoritesChanged(vec!["synthwave".into()]));
        assert!(app.state.favorite("synthwave"));
        // A toggle: Ctrl+A again removes it.
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFavorite("synthwave".into())]
        );

        // Esc returns to the home page, where synthwave now is.
        assert_eq!(app.key(Esc), [Action::LoadLists]);
        assert_eq!(
            app.state.shelf_lists(),
            [ShelfRow::Genre(synthwave()), ShelfRow::Genres]
        );
    }

    #[test]
    fn enter_in_more_genres_starts_radio_and_esc_returns_there() {
        let mut app = genre_app(&[]);
        app.key(Enter);
        let request = radio_request(&app.key(Enter));
        assert_eq!(request.genre, genres::all()[0].id);
        app.send(radio_loaded(&request, station(0..50)));
        assert_eq!(radio_view(&app).genre, &genres::all()[0]);
        assert!(app.key(Esc).is_empty());
        assert!(matches!(app.state.view(), View::Genres { selected: 0, .. }));
    }

    #[test]
    fn radio_view_keys() {
        let mut app = genre_app(&["synthwave"]);
        let first = start_radio(&mut app, station(0..50));

        // Enter plays onward from the selected track.
        app.keys(&[Down, Down, Up]);
        assert_eq!(app.key(Enter), [radio_play(1)]);
        app.send(started("Song 1"));
        assert_eq!(app.state.playing_radio_track("synthwave"), Some(1));
        assert_eq!(app.send(end_of_track()), [radio_play(2)]);

        // Ctrl+E adds the track to the queue.
        assert!(app.key(Ctrl('e')).is_empty());
        assert_eq!(queue_names(&app), ["Song 1"]);
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Queued, "Song 1"))
        );

        // Ctrl+A fetches the album's year and adds it to the shelf, or removes it if there.
        let record = album("record1", "Record 1", "Various Artists", Some(2019));
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::AddRadioAlbumToShelf(record.uri.clone())]
        );
        assert_eq!(
            app.send(Event::RadioAlbum(Ok(record.clone()))),
            [Action::AddToShelf(record.clone())]
        );
        app.send(Event::ShelfChanged(vec![record.clone()]));
        assert!(app.state.on_shelf(&record.uri));
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::RemoveFromShelf(record.uri.clone())]
        );
        // The album reached the shelf before the result: it is not added twice.
        assert!(app.send(Event::RadioAlbum(Ok(record))).is_empty());
        // Fetching the album failed.
        app.send(Event::RadioAlbum(Err("could not add the album".into())));
        assert_eq!(app.state.error.as_deref(), Some("could not add the album"));

        // Ctrl+R: a new seed, and the radio starts over in the same view.
        let request = radio_request(&app.key(Ctrl('r')));
        assert_eq!(request.kind, RadioLoad::Station);
        assert_eq!(request.genre, "synthwave");
        assert_ne!(request.seed, first.seed);
        assert_eq!(
            app.send(radio_loaded(&request, station(100..150))),
            [radio_play(100)]
        );
        assert_eq!(app.state.views.len(), 2);
        assert_eq!(radio_view(&app).selected, 0);
        assert_eq!(radio_of(&app).seeds, [request.seed]);
        assert_eq!(app.state.error, None);
        // The queue is kept.
        assert_eq!(queue_names(&app), ["Song 1"]);
    }

    #[test]
    fn player_keys_in_radio_and_genre_views() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        assert_eq!(app.key(Right), [radio_play(1)]);
        assert_eq!(app.key(Left), [radio_play(0)]);
        // The first track starts over.
        assert_eq!(app.key(Left), [radio_play(0)]);
        app.key(Esc);
        app.keys(&[Down, Enter]);
        assert!(matches!(app.state.view(), View::Genres { .. }));
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        assert_eq!(app.key(Char('q')), [Action::Quit]);
    }

    #[test]
    fn radio_continues_by_itself_past_the_first_page() {
        let mut app = genre_app(&["lofi"]);
        let start = start_radio(&mut app, station(0..50));
        // No refill while at least five tracks remain after the playing one.
        assert!(
            play_until(&mut app, 44)
                .iter()
                .all(|a| !matches!(a, Action::LoadRadio(_)))
        );

        let actions = app.send(end_of_track());
        assert_eq!(actions[0], radio_play(45));
        let more = radio_request(&actions);
        assert_eq!(more.kind, RadioLoad::More);
        assert_eq!(more.seed, start.seed);
        assert_eq!(more.previous.len(), 50);
        assert_eq!(more.previous[49], "spotify:track:r49");
        // A fetch is already running: no second one.
        app.send(started("Song 45"));
        assert!(
            app.send(end_of_track())
                .iter()
                .all(|a| !matches!(a, Action::LoadRadio(_)))
        );

        // Refill: a known track is skipped, new ones go to the end.
        let mut page = station(46..96);
        page.insert(0, radio_track(3));
        assert!(app.send(radio_loaded(&more, page)).is_empty());
        assert_eq!(radio_of(&app).tracks.len(), 96);
        assert_eq!(radio_of(&app).seed, start.seed);

        // The radio continues by itself.
        play_until(&mut app, 60);
        assert_eq!(radio_of(&app).index, 60);
        assert_eq!(app.state.playing_genre(), Some("lofi"));
    }

    #[test]
    fn radio_keeps_at_most_radio_history_played_tracks() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..600));
        let more = radio_request(&play_until(&mut app, 595));
        // n moves the radio view's selection to the playing track.
        app.key(Char('n'));
        assert_eq!(radio_view(&app).selected, 595);

        app.send(radio_loaded(&more, station(600..650)));
        let radio = radio_of(&app);
        assert_eq!(radio.index, RADIO_HISTORY);
        assert_eq!(radio.tracks.len(), 650 - 95);
        assert_eq!(radio.tracks[radio.index].uri, "spotify:track:r595");
        assert_eq!(radio.seed_start, 0);
        assert_eq!(radio_view(&app).selected, RADIO_HISTORY);
        assert_eq!(app.state.playing_radio_track("lofi"), Some(RADIO_HISTORY));

        // Playback continues and ← goes back to the previous track as before trimming.
        assert_eq!(app.send(end_of_track())[0], radio_play(596));
        app.send(started("Song 596"));
        assert_eq!(app.key(Left), [radio_play(595)]);
    }

    #[test]
    fn previous_without_a_track_to_go_back_to_keeps_playback() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..20));
        app.state.playback.radio.as_mut().unwrap().tracks.clear();
        // An empty radio fetches more, but nothing is played.
        let actions = app.key(Left);
        assert!(
            !actions.iter().any(|a| matches!(a, Action::Play(_))),
            "{actions:?}"
        );
        assert_eq!(app.state.playback.current, Some(Current::Radio));
    }

    #[test]
    fn exhausted_station_switches_to_an_unplayed_seed() {
        let mut app = genre_app(&["lofi"]);
        let start = start_radio(&mut app, station(0..8));
        let more = radio_request(&play_until(&mut app, 3));
        play_until(&mut app, 5);

        // Only one new: the station repeats itself, so the next fetch uses a new seed.
        let actions = app.send(radio_loaded(&more, station(0..9)));
        let radio = radio_of(&app);
        assert_eq!(radio.tracks.len(), 9);
        assert_ne!(radio.seed, start.seed);
        assert_eq!(radio.seeds, [start.seed.clone(), radio.seed.clone()]);
        assert_eq!(radio.seed_start, 9);
        // Still fewer than five after the playing one: the fetch continues right away with
        // a new seed, without the previous station's tracks.
        let next = radio_request(&actions);
        assert_eq!(next.seed, radio.seed);
        assert!(next.previous.is_empty());

        // The new seed's tracks are appended seamlessly.
        assert!(app.send(radio_loaded(&next, station(9..59))).is_empty());
        assert_eq!(radio_of(&app).tracks.len(), 59);
        assert_eq!(radio_of(&app).seed, next.seed);
        play_until(&mut app, 12);
        assert_eq!(radio_of(&app).index, 12);
    }

    #[test]
    fn continuation_stops_when_no_seed_brings_anything_new() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..6));
        let mut actions = play_until(&mut app, 1);
        let mut fetches = 0;
        while let Some(Action::LoadRadio(request)) = actions.last().cloned() {
            fetches += 1;
            actions = app.send(radio_loaded(&request, station(0..6)));
        }
        // Every seed was tried once, then fetching stopped.
        assert_eq!(fetches, lofi().seeds.len() + 1);
        assert_eq!(radio_of(&app).seeds.len(), lofi().seeds.len());
        // The remaining tracks play, and playback stops as at the end of an album.
        play_until(&mut app, 5);
        assert_eq!(app.send(end_of_track()), [Action::Stop]);
        assert_eq!(app.state.playing_genre(), None);
    }

    #[test]
    fn failed_continuation_shows_error_and_retries_when_next_track_starts() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..6));
        let more = radio_request(&play_until(&mut app, 1));
        let actions = app.send(Event::RadioLoaded {
            request: more,
            result: Err("could not load the radio: timeout".into()),
        });
        assert!(actions.is_empty());
        assert_eq!(
            app.state.error.as_deref(),
            Some("could not load the radio: timeout")
        );
        // No new attempt during the same track.
        assert!(app.wait(1000).is_empty());

        let actions = app.send(end_of_track());
        assert_eq!(actions[0], radio_play(2));
        let retry = radio_request(&actions);
        app.send(radio_loaded(&retry, station(6..56)));
        assert_eq!(app.state.error, None);
        assert_eq!(radio_of(&app).tracks.len(), 56);
    }

    #[test]
    fn queue_plays_before_the_next_radio_track() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        app.keys(&[Down, Down, Down, Ctrl('e')]);
        assert_eq!(queue_names(&app), ["Song 3"]);

        assert_eq!(app.send(end_of_track()), [radio_play(3)]);
        app.send(started("Song 3"));
        // A track playing from the queue is not the radio's ♪, but the genre still plays.
        assert_eq!(app.state.playing_radio_track("lofi"), None);
        assert_eq!(app.state.playing_genre(), Some("lofi"));
        // ← goes back to the radio track, and the queue track goes back to the queue.
        assert_eq!(app.key(Left), [radio_play(0)]);
        assert_eq!(queue_names(&app), ["Song 3"]);
        assert_eq!(app.key(Right), [radio_play(3)]);
        // After the queue the radio continues where it left off.
        assert_eq!(app.send(end_of_track()), [radio_play(1)]);
        assert_eq!(app.state.playing_radio_track("lofi"), Some(1));
    }

    #[test]
    fn queued_radio_track_has_no_album_position() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        app.keys(&[Down, Ctrl('e')]);
        app.send(end_of_track());
        app.send(started("Song 1"));
        assert_eq!(app.state.playing_track("spotify:album:record1"), None);
        assert_eq!(app.state.queue().len(), 0);
    }

    #[test]
    fn playing_an_album_replaces_the_radio() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        app.keys(&[Down, Ctrl('e')]);
        app.state.views.truncate(1);
        app.key(Ctrl('f'));
        app.search("nevermind", vec![], vec![nevermind()]);
        app.key(Enter);
        app.send(album_tracks(&nevermind(), Ok(nevermind_tracks())));
        assert_eq!(app.key(Enter), [play("nevermind", 0)]);
        assert_eq!(app.state.playing_genre(), None);
        assert_eq!(app.state.radio_tracks("lofi"), None);
        assert_eq!(queue_names(&app), ["Song 1"]);
        // Radio view without a radio: Ctrl+R starts a new one, the others do nothing.
        app.state.views.push(View::Radio(RadioView {
            genre: lofi(),
            selected: 0,
            scroll: Scroll::default(),
        }));
        assert!(app.keys(&[Down, Enter, Ctrl('e'), Ctrl('a')]).is_empty());
        assert_eq!(radio_request(&app.key(Ctrl('r'))).genre, "lofi");
    }

    #[test]
    fn radio_is_saved_and_restored() {
        let mut app = genre_app(&["lofi"]);
        let start = start_radio(&mut app, station(0..50));
        play_until(&mut app, 2);
        app.keys(&[Down, Ctrl('e')]);
        let saved = quit(&mut app);
        let json = serde_json::to_string(&saved).unwrap();
        assert!(json.contains(r#""current":"radio""#), "{json}");
        assert_eq!(serde_json::from_str::<SavedPlayback>(&json).unwrap(), saved);

        let (mut app, actions) = restored(saved);
        assert_eq!(
            actions,
            [Action::Restore {
                item: Item::Track("spotify:track:r2".into()),
                position: Duration::ZERO,
            }]
        );
        assert_eq!(app.state.playing_genre(), Some("lofi"));
        assert_eq!(app.state.radio_tracks("lofi").unwrap().len(), 50);
        assert_eq!(queue_names(&app), ["Song 1"]);
        // Space resumes, and the radio continues after the queue.
        assert_eq!(app.key(Char(' ')), [Action::TogglePause]);
        app.send(Event::Player(player::Event::Position(0)));
        assert_eq!(
            app.send(end_of_track()),
            [Action::Play(Item::Track("spotify:track:r1".into()))]
        );
        assert_eq!(app.send(end_of_track()), [radio_play(3)]);

        // Refills work on a restored radio from the same seed.
        let more = radio_request(&play_until(&mut app, 45));
        assert_eq!(more.seed, start.seed);
        assert_eq!(more.previous.len(), 50);
    }

    #[test]
    fn restored_radio_near_its_end_loads_more_at_once() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..8));
        let more = radio_request(&play_until(&mut app, 3));
        let saved = quit(&mut app);
        let (_, actions) = restored(saved);
        assert!(matches!(actions[0], Action::Restore { .. }));
        assert_eq!(radio_request(&actions), more);
    }

    #[test]
    fn invalid_saved_radio_starts_empty_with_error() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        let mut saved = quit(&mut app);
        saved.radio.as_mut().unwrap().index = 50;
        let (app, actions) = restored(saved);
        assert!(actions.is_empty());
        assert!(app.state.error.is_some());
        assert_eq!(app.state.playing_genre(), None);
    }

    // --- Likes (Liked Songs) ---

    /// Shelf with likes enabled.
    fn likes_app() -> App {
        let mut app = shelf_app();
        app.state.likes.enabled = true;
        app
    }

    fn check(names: &[&str]) -> Action {
        Action::CheckLiked(names.iter().map(|name| track_uri(name)).collect())
    }

    fn set_liked(name: &str, liked: bool) -> Action {
        Action::SetLiked {
            uri: track_uri(name),
            liked,
        }
    }

    fn like_changed(name: &str, liked: bool, result: Result<(), String>) -> Event {
        Event::LikeChanged {
            uri: track_uri(name),
            liked,
            result,
        }
    }

    fn liked_checked(names: &[&str], result: Result<Vec<bool>, String>) -> Event {
        Event::LikedChecked {
            uris: names.iter().map(|name| track_uri(name)).collect(),
            result,
        }
    }

    /// In Bloom is playing, and its like has not been asked yet.
    fn in_bloom_playing() -> App {
        let mut app = likes_app();
        assert_eq!(app.send(started("In Bloom")), [check(&["In Bloom"])]);
        app
    }

    #[test]
    fn ctrl_l_likes_playing_track_and_again_unlikes_it() {
        let mut app = in_bloom_playing();
        app.send(liked_checked(&["In Bloom"], Ok(vec![false])));

        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
        // The state changes only once Spotify has answered.
        assert!(!app.state.liked(&track_uri("In Bloom")));
        app.send(like_changed("In Bloom", true, Ok(())));
        assert!(app.state.liked(&track_uri("In Bloom")));
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Liked, "In Bloom"))
        );

        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", false)]);
        app.send(like_changed("In Bloom", false, Ok(())));
        assert!(!app.state.liked(&track_uri("In Bloom")));
        assert_eq!(
            app.state.notice(app.now),
            Some((NoticeKind::Unliked, "In Bloom"))
        );
        app.now += NOTICE_TIME;
        assert_eq!(app.state.notice(app.now), None);
    }

    #[test]
    fn ctrl_l_unlikes_track_known_to_be_liked() {
        let mut app = in_bloom_playing();
        app.send(liked_checked(&["In Bloom"], Ok(vec![true])));
        assert!(app.state.liked(&track_uri("In Bloom")));
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", false)]);
    }

    #[test]
    fn ctrl_l_before_answer_likes() {
        let mut app = in_bloom_playing();
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
        // A late answer does not undo the like.
        app.send(like_changed("In Bloom", true, Ok(())));
        app.send(liked_checked(&["In Bloom"], Ok(vec![false])));
        assert!(app.state.liked(&track_uri("In Bloom")));
    }

    #[test]
    fn ctrl_l_works_in_every_view_also_in_search() {
        let mut app = in_bloom_playing();
        app.key(Ctrl('f'));
        app.type_text("nirv");
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
        assert_eq!(app.search_view().query, "nirv");
        app.send(like_changed("In Bloom", true, Ok(())));
        app.key(Ctrl('q'));
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", false)]);
    }

    #[test]
    fn ctrl_l_waits_for_previous_change() {
        let mut app = in_bloom_playing();
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
        assert_eq!(app.key(Ctrl('l')), []);
        app.send(like_changed("In Bloom", true, Ok(())));
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", false)]);
    }

    #[test]
    fn ctrl_l_without_playing_track_does_nothing() {
        let mut app = likes_app();
        assert_eq!(app.key(Ctrl('l')), []);
        assert_eq!(app.state.error, None);
    }

    #[test]
    fn ctrl_l_without_own_spotify_app_explains() {
        let mut app = shelf_app();
        assert_eq!(app.send(started("In Bloom")), []);
        assert_eq!(app.key(Ctrl('l')), []);
        assert_eq!(app.state.error.as_deref(), Some(LIKES_NEED_APP));
    }

    #[test]
    fn failed_like_shows_error_and_keeps_state() {
        let mut app = in_bloom_playing();
        app.send(liked_checked(&["In Bloom"], Ok(vec![false])));
        app.key(Ctrl('l'));
        app.send(like_changed(
            "In Bloom",
            true,
            Err("Spotify is rate limiting".into()),
        ));
        assert_eq!(app.state.error.as_deref(), Some("Spotify is rate limiting"));
        assert!(!app.state.liked(&track_uri("In Bloom")));
        assert_eq!(app.state.notice(app.now), None);
        // A new attempt is the user's own: Ctrl+L tries again.
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
    }

    #[test]
    fn album_tracks_are_checked_once_in_one_batch() {
        let mut app = likes_app();
        app.keys(&[Down, Enter, Down]);
        app.key(Enter);
        let names = [
            "Smells Like Teen Spirit",
            "In Bloom",
            "Come As You Are",
            "Breed",
        ];
        assert_eq!(
            app.send(album_tracks(&nevermind(), Ok(nevermind_tracks()))),
            [check(&names)]
        );
        app.send(liked_checked(&names, Ok(vec![false, true, false, false])));
        assert!(app.state.liked(&track_uri("In Bloom")));
        assert!(!app.state.liked(&track_uri("Breed")));

        // The track list again and playing an album track: no new query.
        app.key(Esc);
        app.key(Enter);
        assert_eq!(
            app.send(album_tracks(&nevermind(), Ok(nevermind_tracks()))),
            []
        );
        assert_eq!(app.send(started("In Bloom")), []);
    }

    #[test]
    fn unanswered_check_is_not_repeated() {
        let mut app = in_bloom_playing();
        assert_eq!(app.send(started("In Bloom")), []);
        assert_eq!(app.send(started("Breed")), [check(&["Breed"])]);
    }

    #[test]
    fn queued_album_is_checked() {
        let mut app = in_bloom_playing();
        let actions = app.send(Event::AlbumQueued {
            album: nevermind(),
            result: Ok(nevermind_tracks()),
        });
        // Nothing was playing in the play order, so the queue starts playing.
        assert_eq!(actions[0], play_track("Smells Like Teen Spirit"));
        assert_eq!(
            actions[1..],
            [check(&[
                "Smells Like Teen Spirit",
                "Come As You Are",
                "Breed"
            ])]
        );
    }

    #[test]
    fn failed_check_stops_checks_without_retry() {
        let mut app = in_bloom_playing();
        app.send(liked_checked(
            &["In Bloom"],
            Err("Spotify is rate limiting requests, try again in 30 s".into()),
        ));
        assert_eq!(
            app.state.error.as_deref(),
            Some("Spotify is rate limiting requests, try again in 30 s")
        );
        assert_eq!(app.send(started("Breed")), []);
        app.keys(&[Down, Enter, Down]);
        app.key(Enter);
        assert_eq!(
            app.send(album_tracks(&nevermind(), Ok(nevermind_tracks()))),
            []
        );
        // Ctrl+L still works: an unknown state counts as not liked.
        assert_eq!(app.key(Ctrl('l')), [set_liked("Breed", true)]);
    }

    #[test]
    fn radio_pages_are_checked_at_most_40_at_a_time() {
        let mut app = genre_app(&["lofi"]);
        app.state.likes.enabled = true;
        let request = radio_request(&app.key(Enter));
        let actions = app.send(radio_loaded(&request, station(0..50)));
        let uris = |range: std::ops::Range<usize>| -> Vec<String> {
            range.map(|n| format!("spotify:track:r{n}")).collect()
        };
        assert_eq!(
            actions,
            [
                radio_play(0),
                Action::CheckLiked(uris(0..40)),
                Action::CheckLiked(uris(40..50)),
            ]
        );
        app.send(Event::LikedChecked {
            uris: uris(0..40),
            result: Ok((0..40).map(|n| n == 3).collect()),
        });
        assert!(app.state.liked("spotify:track:r3"));
        assert!(!app.state.liked("spotify:track:r4"));

        // A refill asks only about the new tracks.
        let more = radio_request(&play_until(&mut app, 45));
        let actions = app.send(radio_loaded(&more, station(45..60)));
        assert_eq!(actions, [Action::CheckLiked(uris(50..60))]);
    }

    #[test]
    fn restore_checks_playing_queue_and_upcoming_radio() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let mut app = likes_app();
        let actions = app.send(Event::Restore(Box::new(saved)));
        assert!(matches!(actions[0], Action::Restore { .. }));
        assert_eq!(
            actions[1..],
            [check(&["In Bloom", "Come As You Are", "Breed"])]
        );

        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        play_until(&mut app, 30);
        let saved = quit(&mut app);
        let mut app = likes_app();
        let actions = app.send(Event::Restore(Box::new(saved)));
        let upcoming: Vec<String> = (30..50).map(|n| format!("spotify:track:r{n}")).collect();
        let mut expected = vec![track_uri("Song")];
        expected.extend(upcoming);
        assert_eq!(actions[1..], [Action::CheckLiked(expected)]);
    }

    // --- Letters alongside Ctrl keys ---

    /// Outside search the letters f, l, e, a, d and r do the same as their Ctrl
    /// counterparts, and u the same as Ctrl+Q: the same actions, view, queue and
    /// error. Likes are enabled and something is playing, so that l does something too.
    fn assert_letters_match_ctrl(setup: impl Fn() -> App) {
        let prepared = || {
            let mut app = setup();
            app.state.likes.enabled = true;
            if app.state.now_playing.is_none() {
                app.send(started("In Bloom"));
            }
            app
        };
        let pairs = [
            ('f', 'f'),
            ('l', 'l'),
            ('e', 'e'),
            ('a', 'a'),
            ('d', 'd'),
            ('r', 'r'),
            ('u', 'q'),
        ];
        for (c, ctrl_key) in pairs {
            let mut ctrl = prepared();
            let mut letter = prepared();
            let view = format!("{:?}", ctrl.state.view());
            assert_eq!(
                same_seed(letter.key(Char(c))),
                same_seed(ctrl.key(Ctrl(ctrl_key))),
                "{c} in {view}"
            );
            assert_eq!(
                format!("{:?}", letter.state.view()),
                format!("{:?}", ctrl.state.view()),
                "{c} in {view}"
            );
            assert_eq!(queue_names(&letter), queue_names(&ctrl), "{c} in {view}");
            assert_eq!(letter.state.error, ctrl.state.error, "{c} in {view}");
        }
    }

    /// The new station's seed is random, so it is not compared.
    fn same_seed(actions: Vec<Action>) -> Vec<Action> {
        actions
            .into_iter()
            .map(|action| match action {
                Action::LoadRadio(request) => Action::LoadRadio(RadioRequest {
                    seed: String::new(),
                    ..request
                }),
                action => action,
            })
            .collect()
    }

    #[test]
    fn letters_work_like_ctrl_keys_in_every_view_but_search() {
        // Shelf: an artist and a genre on the home page.
        assert_letters_match_ctrl(shelf_app);
        assert_letters_match_ctrl(|| genre_app(&["lofi"]));
        assert_letters_match_ctrl(|| {
            let mut app = curated_app();
            app.key(Enter);
            app
        });
        // An artist from the shelf and from Spotify.
        assert_letters_match_ctrl(|| {
            let mut app = shelf_app();
            app.key(Enter);
            app
        });
        assert_letters_match_ctrl(|| {
            let mut app = spotify_artist_app();
            app.send(Event::ArtistAlbums {
                artist_id: "id-nirvana".into(),
                result: Ok(vec![unplugged(), nevermind()]),
            });
            app
        });
        assert_letters_match_ctrl(nevermind_app);
        assert_letters_match_ctrl(|| {
            let mut app = nevermind_app();
            queue_track(&mut app, 1);
            app.key(Ctrl('q'));
            app
        });
        assert_letters_match_ctrl(|| {
            let mut app = genre_app(&[]);
            app.key(Enter);
            app
        });
        assert_letters_match_ctrl(|| {
            let mut app = genre_app(&["synthwave"]);
            start_radio(&mut app, station(0..50));
            app
        });
    }

    #[test]
    fn letters_do_their_view_specific_thing() {
        let mut app = curated_app();
        app.key(Enter);
        assert_eq!(app.key(Char('e')), [Action::QueueAlbum(no_other())]);
        assert_eq!(app.key(Char('a')), [Action::AddToShelf(no_other())]);
        assert_eq!(app.key(Char('d')), [Action::RejectCurated(no_other().uri)]);

        let mut app = in_bloom_playing();
        assert_eq!(app.key(Char('l')), [set_liked("In Bloom", true)]);
        assert!(app.key(Char('f')).is_empty());
        assert!(matches!(app.state.view(), View::Search(_)));

        let mut app = genre_app(&["synthwave"]);
        start_radio(&mut app, station(0..50));
        assert_eq!(radio_request(&app.key(Char('r'))).genre, "synthwave");
    }

    #[test]
    fn u_opens_queue_from_every_view_but_search() {
        let views: [fn() -> App; 6] = [
            shelf_app,
            || {
                let mut app = shelf_app();
                app.key(Enter);
                app
            },
            || {
                let mut app = curated_app();
                app.key(Enter);
                app
            },
            nevermind_app,
            || {
                let mut app = genre_app(&[]);
                app.key(Enter);
                app
            },
            || {
                let mut app = genre_app(&["synthwave"]);
                start_radio(&mut app, station(0..50));
                app
            },
        ];
        for setup in views {
            let mut app = setup();
            let view = format!("{:?}", app.state.view());
            assert!(app.key(Char('u')).is_empty(), "{view}");
            assert!(matches!(app.state.view(), View::Queue { .. }), "{view}");
            // In the queue u does not stack the view; Esc returns to where you came from.
            app.key(Char('u'));
            app.key(Esc);
            assert_eq!(format!("{:?}", app.state.view()), view);
        }
    }

    #[test]
    fn letters_type_into_search_where_ctrl_keys_still_work() {
        let mut app = in_bloom_playing();
        app.key(Ctrl('f'));
        assert!(app.type_text("fleadru").is_empty());
        assert_eq!(app.search_view().query, "fleadru");
        assert_eq!(app.key(Ctrl('l')), [set_liked("In Bloom", true)]);
        assert!(matches!(app.state.view(), View::Search(_)));
        assert!(app.key(Ctrl('q')).is_empty());
        assert!(matches!(app.state.view(), View::Queue { .. }));
    }

    #[test]
    fn capitals_and_q_keep_their_meaning() {
        let mut app = in_bloom_playing();
        assert!(
            app.keys(&[
                Char('F'),
                Char('L'),
                Char('E'),
                Char('A'),
                Char('D'),
                Char('R'),
                Char('U')
            ])
            .is_empty()
        );
        assert!(app.is_shelf());
        // q quits; the queue opens with u and Ctrl+Q.
        assert_eq!(app.key(Char('q')), [Action::Quit]);
        app.key(Ctrl('q'));
        assert!(matches!(app.state.view(), View::Queue { .. }));
    }

    fn radio_album_tracks(n: usize, tracks: &[usize]) -> Event {
        Event::AlbumTracks {
            uri: format!("spotify:album:record{n}"),
            result: Ok(tracks
                .iter()
                .map(|&t| Track {
                    uri: format!("spotify:track:r{t}"),
                    name: format!("Song {t}"),
                    artist: format!("Artist {t}"),
                    duration: Duration::from_secs(200),
                })
                .collect()),
        }
    }

    #[test]
    fn n_does_nothing_when_nothing_plays() {
        let mut app = shelf_app();
        assert!(app.keys(&[Char('n'), Ctrl('n')]).is_empty());
        assert!(app.is_shelf());
        assert!(!app.state.has_now_playing());
        // The player reported a track, but there is no playback context.
        app.send(started("In Bloom"));
        assert!(app.key(Char('n')).is_empty());
        assert!(app.is_shelf());
    }

    #[test]
    fn n_opens_playing_album_at_playing_track_and_esc_returns() {
        let mut app = nevermind_playing(2);
        app.keys(&[Esc, Esc]);
        assert!(app.is_shelf());
        assert!(app.state.has_now_playing());
        assert_eq!(app.key(Char('n')), [load_tracks("nevermind")]);
        assert_eq!(app.album_view().album, nevermind());
        assert_eq!(app.album_view().selected, 2);
        app.send(album_tracks(&nevermind(), Ok(nevermind_tracks())));
        assert_eq!(app.album_view().selected, 2);
        app.key(Esc);
        assert!(app.is_shelf());

        // From another view too: Esc returns there.
        app.key(Char('u'));
        assert_eq!(app.key(Ctrl('n')), [load_tracks("nevermind")]);
        app.key(Esc);
        assert!(matches!(app.state.view(), View::Queue { .. }));
    }

    #[test]
    fn n_in_open_playing_album_only_moves_selection() {
        let mut app = nevermind_playing(2);
        app.keys(&[Up, Up]);
        assert_eq!(app.album_view().selected, 0);
        assert!(app.key(Char('n')).is_empty());
        assert_eq!(app.album_view().selected, 2);
        // No new view was opened: Esc returns to the artist's albums.
        app.key(Esc);
        assert_eq!(app.artist_view().artist.name, "Nirvana");
    }

    #[test]
    fn n_types_in_search_and_ctrl_n_opens_playing_album() {
        let mut app = nevermind_playing(1);
        app.key(Ctrl('f'));
        assert!(app.key(Char('n')).is_empty());
        assert_eq!(app.search_view().query, "n");
        assert_eq!(app.key(Ctrl('n')), [load_tracks("nevermind")]);
        assert_eq!(app.album_view().selected, 1);
        app.key(Esc);
        assert_eq!(app.search_view().query, "n");
    }

    #[test]
    fn n_opens_album_of_track_queued_from_another_album() {
        let mut app = nevermind_playing(3);
        queue_bleach(&mut app);
        app.keys(&[Right, Right]);
        assert_eq!(queue_names(&app), Vec::<&str>::new());
        app.keys(&[Esc, Esc]);
        assert_eq!(app.key(Char('n')), [load_tracks("bleach")]);
        assert_eq!(app.album_view().album, bleach());
        assert_eq!(app.album_view().selected, 1);
    }

    #[test]
    fn n_opens_playing_radio_at_playing_track() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        play_until(&mut app, 3);
        app.key(Esc);
        assert!(app.is_shelf());
        assert!(app.key(Char('n')).is_empty());
        assert_eq!(radio_view(&app).genre, lofi());
        assert_eq!(radio_view(&app).selected, 3);
        // The radio is already visible: only the selection moves.
        app.keys(&[Up, Up]);
        app.key(Char('n'));
        assert_eq!(radio_view(&app).selected, 3);
        app.key(Esc);
        assert!(app.is_shelf());
    }

    #[test]
    fn n_on_queued_radio_track_opens_its_album_at_the_track() {
        let mut app = genre_app(&["lofi"]);
        start_radio(&mut app, station(0..50));
        app.keys(&[Down, Down, Down, Down, Down, Ctrl('e'), Right]);
        app.send(started("Song 5"));
        app.key(Esc);
        assert_eq!(
            app.key(Char('n')),
            [Action::LoadAlbumTracks("spotify:album:record5".into())]
        );
        let album = &app.album_view().album;
        assert_eq!(album.name, "Record 5");
        assert_eq!(album.artist, "Artist 5");
        assert_eq!(album.artist_id, "a5");
        assert_eq!(album.year, None);
        // The track's position becomes known when the tracks arrive.
        app.send(radio_album_tracks(5, &[4, 5, 6]));
        assert_eq!(app.album_view().selected, 1);
        // The album has no year: adding it to the shelf fetches details as in the radio.
        assert_eq!(
            app.key(Ctrl('a')),
            [Action::AddRadioAlbumToShelf("spotify:album:record5".into())]
        );
    }

    #[test]
    fn n_works_after_restore_and_with_old_saved_state_from_shelf() {
        let mut app = in_bloom_with_queue();
        let saved = quit(&mut app);
        let (mut app, _) = restored(saved.clone());
        assert_eq!(app.key(Char('n')), [load_tracks("nevermind")]);
        assert_eq!(app.album_view().selected, 1);

        // An old file without album details: the album is found on the shelf.
        let mut json = serde_json::to_value(&saved).unwrap();
        json["album"].as_object_mut().unwrap().remove("album");
        let old: SavedPlayback = serde_json::from_value(json).unwrap();
        let (mut app, _) = restored(old.clone());
        assert_eq!(app.key(Char('n')), [load_tracks("nevermind")]);
        // Nor on the shelf: n does nothing.
        let mut app = App::new(Vec::new());
        app.send(Event::Restore(Box::new(old)));
        assert!(app.key(Char('n')).is_empty());
        assert!(app.is_shelf());
    }
}
