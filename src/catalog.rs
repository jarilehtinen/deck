//! Spotify Web API: search, artist albums, album tracks and Liked Songs.
//!
//! The token comes from an OAuth sign-in to the user's own Spotify app when
//! `config.toml` gives a `client_id`. Otherwise the librespot session token (login5)
//! is used, which Spotify rate limits because of the shared client ID (429).
//! Subcommands (`deck curate submit`) sign in without a session, using only the saved token
//! ([`Catalog::connect_stored`]).
//!
//! Likes go through `/me/library` (the old `/me/tracks` has been shut down), and they
//! need an own-app token with the library scopes (`LIBRARY_SCOPES`). A token saved
//! before those scopes is not accepted: startup asks the user to sign in again.
//!
//! After too many requests Spotify blocks all of the app's requests for hours (429),
//! so requests are saved: album track lists, artist albums and the latest searches are
//! kept in memory (shared by clones), and after a 429 no new requests are sent until
//! `retry-after` has passed. The block is shared with the other Deck processes through
//! [`RateLimit`].

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use librespot_oauth::{OAuthClient, OAuthError, OAuthToken};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell};

use crate::{
    config::{self, Config},
    player::SharedSession,
    rate_limit::RateLimit,
    store::unix_now,
};

const API: &str = "https://api.spotify.com/v1";

/// Scopes for the user's own app: Liked Songs. Search and albums need no scopes.
const LIBRARY_SCOPES: &[&str] = &["user-library-read", "user-library-modify"];

/// How many URIs `/me/library/contains` takes at once.
pub const LIKED_BATCH: usize = 40;

/// The response when the token was saved before the library scopes.
const MISSING_SCOPE: &str = "Insufficient client scope";

/// Timeout for a Web API request, from opening the connection to the end of the response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// How long (seconds) no requests are sent after a 429 if Spotify gives no
/// `retry-after`.
const DEFAULT_BLOCK: u64 = 60;

/// Maximum number of responses kept in memory.
const SEARCHES_KEPT: usize = 50;
const ARTISTS_KEPT: usize = 100;
const ALBUMS_KEPT: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    pub year: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub uri: String,
    pub name: String,
    /// The track's artists, comma separated (on a compilation, not the album's).
    pub artist: String,
    pub duration: Duration,
}

/// An album search hit: the album, all its artists and Spotify's album type
/// (`album`, `single` or `compilation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundAlbum {
    pub album: Album,
    pub artists: Vec<String>,
    pub album_type: String,
}

/// A hit from searching for a genre seed (`deck genre add`): an artist or a track. For
/// an artist, `name` is the artist's name and `artists` contains only that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundSeed {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchResults {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
}

#[derive(Clone)]
pub struct Catalog {
    auth: Auth,
    http: reqwest::Client,
    shared: Arc<Shared>,
}

/// Memory shared by clones: responses and the 429 block.
struct Shared {
    searches: Memo<SearchResults>,
    artist_albums: Memo<Vec<Album>>,
    album_tracks: Memo<Vec<Track>>,
    rate_limit: RateLimit,
}

#[derive(Clone)]
enum Auth {
    /// The librespot session token (login5), from the session shared with the player.
    Session(SharedSession),
    App(Arc<AppAuth>),
}

impl Catalog {
    /// Prepares Web API authentication. With the user's own app, the first start
    /// opens the browser.
    pub async fn connect(session: SharedSession, config: &Config) -> Result<Self> {
        let auth = match &config.client_id {
            Some(client_id) => Auth::App(Arc::new(AppAuth::connect(client_id).await?)),
            None => Auth::Session(session),
        };
        Self::new(auth)
    }

    /// Signs in without a librespot session or browser, using the saved Web API
    /// token (`~/.cache/deck/webapi.json`). Without it, returns an error telling the
    /// user to start `deck` once.
    pub async fn connect_stored(config: &Config) -> Result<Self> {
        let Some(client_id) = &config.client_id else {
            bail!("client_id is missing from ~/.config/deck/config.toml");
        };
        Self::new(Auth::App(Arc::new(AppAuth::stored(client_id).await?)))
    }

    fn new(auth: Auth) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("cannot create the HTTP client")?;
        Ok(Self {
            auth,
            http,
            shared: Arc::new(Shared {
                searches: Memo::new(SEARCHES_KEPT),
                artist_albums: Memo::new(ARTISTS_KEPT),
                album_tracks: Memo::new(ALBUMS_KEPT),
                rate_limit: RateLimit::at(&RateLimit::default_path()?),
            }),
        })
    }

    /// Searches for artists and albums. The result for the same query comes from
    /// memory, so that e.g. backspace does not search again.
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        self.shared
            .searches
            .get_or_fetch(query, || self.fetch_search(query))
            .await
    }

    async fn fetch_search(&self, query: &str) -> Result<SearchResults> {
        let page: SearchPage = self
            .get(
                "/search",
                &[("q", query), ("type", "artist,album"), ("limit", "10")],
            )
            .await?;
        Ok(SearchResults {
            artists: page
                .artists
                .items
                .into_iter()
                .map(|a| Artist {
                    id: a.id,
                    name: a.name,
                })
                .collect(),
            albums: page.albums.items.into_iter().map(Album::from).collect(),
        })
    }

    /// Searches for an album by name and artist (an `album:` + `artist:` search). Hits
    /// come in Spotify's order and are not filtered.
    pub async fn search_albums(&self, artist: &str, album: &str) -> Result<Vec<FoundAlbum>> {
        let query = format!("album:{album} artist:{artist}");
        let page: AlbumSearchPage = self
            .get(
                "/search",
                &[("q", query.as_str()), ("type", "album"), ("limit", "10")],
            )
            .await?;
        Ok(page
            .albums
            .items
            .into_iter()
            .map(|a| FoundAlbum {
                artists: a.artists.iter().map(|ar| ar.name.clone()).collect(),
                album_type: a.album_type.clone().unwrap_or_default(),
                album: Album::from(a),
            })
            .collect())
    }

    /// Searches for an artist by name. Hits come in Spotify's order.
    pub async fn search_artists(&self, artist: &str) -> Result<Vec<FoundSeed>> {
        let page: ArtistSearchPage = self
            .get(
                "/search",
                &[("q", artist), ("type", "artist"), ("limit", "10")],
            )
            .await?;
        Ok(page
            .artists
            .items
            .into_iter()
            .map(|a| FoundSeed {
                uri: format!("spotify:artist:{}", a.id),
                artists: vec![a.name.clone()],
                name: a.name,
            })
            .collect())
    }

    /// Searches for a track by name and artist (a `track:` + `artist:` search). Hits
    /// come in Spotify's order.
    pub async fn search_tracks(&self, artist: &str, track: &str) -> Result<Vec<FoundSeed>> {
        let query = format!("track:{track} artist:{artist}");
        let page: TrackSearchPage = self
            .get(
                "/search",
                &[("q", query.as_str()), ("type", "track"), ("limit", "10")],
            )
            .await?;
        Ok(page
            .tracks
            .items
            .into_iter()
            .map(|t| FoundSeed {
                uri: t.uri,
                name: t.name,
                artists: t.artists.into_iter().map(|a| a.name).collect(),
            })
            .collect())
    }

    /// The artist's albums. Kept in memory for the whole run.
    pub async fn artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        self.shared
            .artist_albums
            .get_or_fetch(artist_id, || self.fetch_artist_albums(artist_id))
            .await
    }

    async fn fetch_artist_albums(&self, artist_id: &str) -> Result<Vec<Album>> {
        let items: Vec<ApiAlbum> = self
            .get_all(
                &format!("/artists/{artist_id}/albums"),
                &[("include_groups", "album"), ("limit", "10")],
            )
            .await?;
        Ok(items.into_iter().map(Album::from).collect())
    }

    /// The album's tracks. The list does not change, so it is kept in memory: opening
    /// an album fetches it for both the view and the player with one request.
    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        self.shared
            .album_tracks
            .get_or_fetch(album_id, || self.fetch_album_tracks(album_id))
            .await
    }

    async fn fetch_album_tracks(&self, album_id: &str) -> Result<Vec<Track>> {
        let items: Vec<ApiTrack> = self
            .get_all(&format!("/albums/{album_id}/tracks"), &[("limit", "50")])
            .await?;
        Ok(items
            .into_iter()
            .map(|t| Track {
                uri: t.uri,
                name: t.name,
                artist: t
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                duration: Duration::from_millis(t.duration_ms),
            })
            .collect())
    }

    /// Whether likes work: they need the user's own app (`client_id`).
    pub fn can_like(&self) -> bool {
        matches!(self.auth, Auth::App(_))
    }

    /// Whether each track (URI) is liked, in the same order as `uris`. At most
    /// [`LIKED_BATCH`] at once.
    pub async fn liked(&self, uris: &[String]) -> Result<Vec<bool>> {
        let joined = uris.join(",");
        let flags = self
            .get("/me/library/contains", &[("uris", joined.as_str())])
            .await?;
        liked_flags(uris.len(), flags)
    }

    /// Saves the track (URI) to Liked Songs or removes it from there.
    pub async fn set_liked(&self, uri: &str, liked: bool) -> Result<()> {
        let url = format!("{API}/me/library");
        let request = if liked {
            self.http.put(url)
        } else {
            self.http.delete(url)
        };
        self.send(
            request
                .query(&[("uris", uri)])
                .header(reqwest::header::CONTENT_LENGTH, 0),
        )
        .await?;
        Ok(())
    }

    async fn token(&self) -> Result<String> {
        match &self.auth {
            Auth::Session(session) => Ok(session
                .get()
                .await?
                .login5()
                .auth_token()
                .await
                .context("could not get an access token from the session")?
                .access_token),
            Auth::App(app) => app.token().await,
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        self.fetch(self.http.get(format!("{API}{path}")).query(query))
            .await
    }

    /// Fetches all pages (following the `next` link) and returns the items as one list.
    async fn get_all<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>> {
        let mut page: Page<T> = self.get(path, query).await?;
        let mut items = std::mem::take(&mut page.items);
        while let Some(next) = page.next.take() {
            page = self.fetch(self.http.get(next)).await?;
            items.append(&mut page.items);
        }
        Ok(items)
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T> {
        self.send(request)
            .await?
            .json()
            .await
            .context("could not read the Spotify response")
    }

    /// Sends the request with the token. An error response is returned as an error, and
    /// nothing is retried. After a 429, from any Deck process, the request is not sent
    /// at all until the time given by Spotify has passed.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let now = unix_now();
        if let Some(until) = self.shared.rate_limit.blocked_until(now) {
            return Err(RateLimited {
                retry_after: Some(until - now),
            }
            .into());
        }
        let response = request
            .bearer_auth(self.token().await?)
            .send()
            .await
            .context("Spotify is not responding")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok());
            let wait = retry_after.unwrap_or(DEFAULT_BLOCK);
            self.shared.rate_limit.block(unix_now() + wait);
            return Err(RateLimited { retry_after }.into());
        }
        let body = response.text().await.unwrap_or_default();
        bail!(api_error(status, &body))
    }
}

/// Locks the memory. A poisoned lock is fine: the contents are always intact.
fn lock<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Responses kept in memory by key. Concurrent callers asking for the same key wait
/// for a single fetch. A failed fetch is not remembered, so it is retried the next
/// time. When full, the oldest key is forgotten.
struct Memo<T> {
    capacity: usize,
    entries: StdMutex<MemoEntries<T>>,
}

struct MemoEntries<T> {
    cells: HashMap<String, Arc<OnceCell<T>>>,
    /// Keys in insertion order, oldest first.
    order: VecDeque<String>,
}

impl<T: Clone> Memo<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: StdMutex::new(MemoEntries {
                cells: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    async fn get_or_fetch<F, Fut>(&self, key: &str, fetch: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.cell(key).get_or_try_init(fetch).await.cloned()
    }

    fn cell(&self, key: &str) -> Arc<OnceCell<T>> {
        let mut entries = lock(&self.entries);
        if let Some(cell) = entries.cells.get(key) {
            return cell.clone();
        }
        while entries.order.len() >= self.capacity {
            let Some(oldest) = entries.order.pop_front() else {
                break;
            };
            entries.cells.remove(&oldest);
        }
        let cell = Arc::new(OnceCell::new());
        entries.cells.insert(key.to_owned(), cell.clone());
        entries.order.push_back(key.to_owned());
        cell
    }
}

/// The Web API answered 429: Spotify blocks all of the app's requests for `retry-after`
/// seconds. A separate type so that `deck curate submit` can recognise it (`downcast_ref`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    /// The `retry-after` header in seconds, if Spotify gave one.
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wait = self.retry_after.map_or("?".to_owned(), |s| s.to_string());
        write!(
            f,
            "Spotify is rate limiting requests, try again in {wait} s"
        )
    }
}

impl std::error::Error for RateLimited {}

/// Turns a Web API error response into a message (other than 429, see [`RateLimited`]).
fn api_error(status: StatusCode, body: &str) -> String {
    if status == StatusCode::FORBIDDEN && body.contains(MISSING_SCOPE) {
        return "Deck has no access to your Liked Songs – restart Deck to sign in again".into();
    }
    format!("Spotify Web API {status}: {body}")
}

/// The `/me/library/contains` response: one boolean for each URI asked about.
fn liked_flags(asked: usize, flags: Vec<bool>) -> Result<Vec<bool>> {
    if flags.len() != asked {
        bail!("Spotify answered {} of {asked} liked songs", flags.len());
    }
    Ok(flags)
}

/// Whether the token covers the library scopes.
fn has_library_scopes(scopes: &[String]) -> bool {
    LIBRARY_SCOPES
        .iter()
        .all(|needed| scopes.iter().any(|scope| scope == needed))
}

/// The token of the user's own app. The refresh token is kept in the cache, so the
/// browser is needed only the first time.
struct AppAuth {
    client: OAuthClient,
    token_path: PathBuf,
    token: Mutex<OAuthToken>,
}

#[derive(Serialize, Deserialize)]
struct StoredToken {
    refresh_token: String,
}

impl AppAuth {
    /// Refreshes the saved token. The browser is opened only when there is no token,
    /// Spotify rejected it or it lacks the library scopes. A network error is an error:
    /// otherwise every lost connection would needlessly open the browser.
    async fn connect(client_id: &str) -> Result<Self> {
        let client = config::oauth_client(client_id, LIBRARY_SCOPES)?;
        let token_path = token_path()?;

        let refreshed = match refresh_stored(&client, &token_path).await? {
            Stored::Token(token) if has_library_scopes(&token.scopes) => Some(token),
            // The token was saved before likes existed: the library scopes can only be
            // had by signing in again.
            Stored::Token(token) => {
                log::warn!(
                    "the Web API token lacks the library scopes: {:?}",
                    token.scopes
                );
                eprintln!("Deck needs access to your Liked Songs.");
                None
            }
            Stored::SignInNeeded(reason) => {
                log::warn!("{reason}");
                None
            }
        };
        let token = match refreshed {
            Some(token) => token,
            None => {
                eprintln!("Sign in to the Deck Spotify app in the browser that opens…");
                client
                    .get_access_token_async()
                    .await
                    .context("Web API sign-in failed")?
            }
        };
        Self::with_token(client, token_path, token).await
    }

    /// Like `connect`, but the browser is never opened: without a working saved
    /// token, the result is an error.
    async fn stored(client_id: &str) -> Result<Self> {
        let client = config::oauth_client(client_id, LIBRARY_SCOPES)?;
        let token_path = token_path()?;
        let token = match refresh_stored(&client, &token_path).await? {
            Stored::Token(token) => token,
            Stored::SignInNeeded(reason) => {
                bail!("{reason}: no usable Spotify sign-in, start `deck` once to sign in")
            }
        };
        Self::with_token(client, token_path, token).await
    }

    async fn with_token(
        client: OAuthClient,
        token_path: PathBuf,
        token: OAuthToken,
    ) -> Result<Self> {
        let auth = Self {
            client,
            token_path,
            token: Mutex::new(token),
        };
        auth.save(&*auth.token.lock().await)?;
        Ok(auth)
    }

    async fn token(&self) -> Result<String> {
        let mut token = self.token.lock().await;
        if token.expires_at <= Instant::now() + Duration::from_secs(60) {
            let fresh = refresh(&self.client, token.refresh_token.clone()).await?;
            self.save(&fresh)?;
            *token = fresh;
        }
        Ok(token.access_token.clone())
    }

    fn save(&self, token: &OAuthToken) -> Result<()> {
        let stored = StoredToken {
            refresh_token: token.refresh_token.clone(),
        };
        write_private(&self.token_path, serde_json::to_string(&stored)?.as_bytes())
            .with_context(|| format!("cannot save {}", self.token_path.display()))
    }
}

/// The refresh token of the user's own app (`~/.cache/deck/webapi.json`).
fn token_path() -> Result<PathBuf> {
    Ok(config::private_cache_dir()?.join("webapi.json"))
}

/// Writes the file atomically (`.tmp` and rename), readable by the owner only
/// (0600), so that the token is never readable by others, not even for a moment.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A crash leftover may have other permissions, and `mode` only applies to new files.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
}

/// The result of refreshing the saved token when the network worked.
enum Stored {
    Token(OAuthToken),
    /// A new sign-in is needed: there is no token, the file is broken or Spotify
    /// rejected the token. Carries the reason for the log.
    SignInNeeded(String),
}

/// Reads the saved refresh token and uses it to renew the access token. An error means
/// something other than [`Stored::SignInNeeded`], e.g. a network problem.
async fn refresh_stored(client: &OAuthClient, token_path: &Path) -> Result<Stored> {
    let path = token_path.display();
    let text = match fs::read_to_string(token_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Stored::SignInNeeded(format!("{path} is missing")));
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {path}")),
    };
    // The file is Deck's own cache, not a user file, so a broken one may be replaced
    // by signing in again.
    let stored: StoredToken = match serde_json::from_str(&text) {
        Ok(stored) => stored,
        Err(e) => return Ok(Stored::SignInNeeded(format!("{path} is broken: {e}"))),
    };
    match refresh(client, stored.refresh_token).await {
        Ok(token) => Ok(Stored::Token(token)),
        Err(e) if rejected(&e) => Ok(Stored::SignInNeeded(format!(
            "Spotify rejected the saved Web API token: {e:#}"
        ))),
        Err(e) => Err(e.context("cannot reach Spotify to renew the sign-in")),
    }
}

/// Whether Spotify rejected the refresh token (e.g. `invalid_grant`), or the request
/// failed otherwise (network, server error). librespot passes on only the text of the
/// oauth2 error, so the check is based on it: a server error response starts with
/// "Server returned error response", a network error is "Request failed" and an
/// unparseable response "Failed to parse server response". An unclear case does not
/// open the browser.
fn rejected(error: &anyhow::Error) -> bool {
    match error.downcast_ref::<OAuthError>() {
        Some(OAuthError::ExchangeCode { e }) => {
            e.starts_with("Server returned error response")
                && !e.contains("server_error")
                && !e.contains("temporarily_unavailable")
        }
        _ => false,
    }
}

/// Renews the access token. Spotify usually returns no new refresh token, in which
/// case the old one is still valid and is kept.
async fn refresh(client: &OAuthClient, refresh_token: String) -> Result<OAuthToken> {
    let mut token = client
        .refresh_token_async(&refresh_token)
        .await
        .context("could not refresh the Web API token")?;
    if token.refresh_token.is_empty() {
        token.refresh_token = refresh_token;
    }
    Ok(token)
}

#[derive(Deserialize)]
struct SearchPage {
    artists: Page<ApiArtist>,
    albums: Page<ApiAlbum>,
}

#[derive(Deserialize)]
struct ArtistSearchPage {
    artists: Page<ApiArtist>,
}

#[derive(Deserialize)]
struct TrackSearchPage {
    tracks: Page<ApiTrack>,
}

#[derive(Deserialize)]
struct AlbumSearchPage {
    albums: Page<ApiAlbum>,
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct ApiArtist {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct ApiAlbum {
    id: String,
    uri: String,
    name: String,
    artists: Vec<ApiArtist>,
    release_date: Option<String>,
    album_type: Option<String>,
}

#[derive(Deserialize)]
struct ApiTrack {
    uri: String,
    name: String,
    artists: Vec<ApiArtist>,
    duration_ms: u64,
}

impl From<ApiAlbum> for Album {
    fn from(a: ApiAlbum) -> Self {
        let (artist, artist_id) = a
            .artists
            .into_iter()
            .next()
            .map(|ar| (ar.name, ar.id))
            .unwrap_or_default();
        Self {
            year: a
                .release_date
                .as_deref()
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse().ok()),
            id: a.id,
            uri: a.uri,
            name: a.name,
            artist,
            artist_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(names: &[&str]) -> Vec<String> {
        names.iter().map(|&name| name.to_owned()).collect()
    }

    #[test]
    fn library_scopes_are_needed_both() {
        assert!(has_library_scopes(&scopes(&[
            "user-library-modify",
            "user-library-read",
        ])));
        assert!(has_library_scopes(&scopes(&[
            "playlist-read-private",
            "user-library-read",
            "user-library-modify",
        ])));
        assert!(!has_library_scopes(&scopes(&["user-library-read"])));
        assert!(!has_library_scopes(&[]));
    }

    #[test]
    fn missing_scope_asks_to_sign_in_again() {
        let body = r#"{"error":{"status":403,"message":"Insufficient client scope"}}"#;
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, body),
            "Deck has no access to your Liked Songs – restart Deck to sign in again"
        );
    }

    #[test]
    fn other_errors_keep_status_and_body() {
        let limited = |retry_after| anyhow::Error::from(RateLimited { retry_after }).to_string();
        assert_eq!(
            limited(Some(34560)),
            "Spotify is rate limiting requests, try again in 34560 s"
        );
        assert_eq!(
            limited(None),
            "Spotify is rate limiting requests, try again in ? s"
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "Forbidden"),
            "Spotify Web API 403 Forbidden: Forbidden"
        );
    }

    #[test]
    fn contains_answer_has_a_flag_per_uri() {
        let flags = serde_json::from_str("[false,true]").unwrap();
        assert_eq!(liked_flags(2, flags).unwrap(), [false, true]);
        let short = serde_json::from_str("[true]").unwrap();
        assert_eq!(
            liked_flags(2, short).unwrap_err().to_string(),
            "Spotify answered 1 of 2 liked songs"
        );
    }

    #[tokio::test]
    async fn memo_fetches_a_key_once_even_concurrently() {
        let memo = Memo::<u32>::new(10);
        let calls = std::sync::atomic::AtomicU32::new(0);
        let fetch = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(7)
        };
        let (a, b) = tokio::join!(memo.get_or_fetch("x", fetch), memo.get_or_fetch("x", fetch));
        assert_eq!((a.unwrap(), b.unwrap()), (7, 7));
        assert_eq!(memo.get_or_fetch("x", fetch).await.unwrap(), 7);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn memo_does_not_keep_errors() {
        let memo = Memo::<u32>::new(10);
        let failed = memo
            .get_or_fetch("x", || async { bail!("Spotify is not responding") })
            .await;
        assert!(failed.is_err());
        assert_eq!(memo.get_or_fetch("x", || async { Ok(1) }).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn memo_forgets_the_oldest_when_full() {
        let memo = Memo::<u32>::new(2);
        for (key, value) in [("a", 1), ("b", 2), ("c", 3)] {
            memo.get_or_fetch(key, || async move { Ok(value) })
                .await
                .unwrap();
        }
        // "a" was forgotten, "b" and "c" are in memory.
        assert_eq!(
            memo.get_or_fetch("a", || async { Ok(10) }).await.unwrap(),
            10
        );
        assert_eq!(
            memo.get_or_fetch("c", || async { Ok(30) }).await.unwrap(),
            3
        );
    }

    #[test]
    fn only_a_rejection_by_spotify_needs_a_new_sign_in() {
        let failure = |message: &str| {
            anyhow::Error::from(OAuthError::ExchangeCode {
                e: message.to_owned(),
            })
            .context("could not refresh the Web API token")
        };
        assert!(rejected(&failure(
            "Server returned error response: invalid_grant: Refresh token revoked"
        )));
        assert!(rejected(&failure(
            "Server returned error response: invalid_client: Invalid client"
        )));
        assert!(!rejected(&failure("Request failed")));
        assert!(!rejected(&failure("Failed to parse server response")));
        assert!(!rejected(&failure(
            "Server returned error response: server_error"
        )));
        assert!(!rejected(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn token_file_is_written_atomically_for_the_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("webapi.json");
        let tmp = dir.path().join("webapi.json.tmp");
        // A leftover from a crash, readable by everyone.
        fs::write(&tmp, "old").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"{\"refresh_token\":\"r\"}").unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"refresh_token\":\"r\"}"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!tmp.exists());

        write_private(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }
}
