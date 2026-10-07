//! Last.fm API (`ws.audioscrobbler.com/2.0`): the user's most played artists and
//! albums per period, plus a cache of listened albums
//! (`~/.cache/deck/lastfm-albums.json`, valid for a day).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::store::{read_json, unix_now, write_atomic};

const API: &str = "https://ws.audioscrobbler.com/2.0/";
/// Age after which the album cache is fetched again.
const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Attempts per call on a temporary error, and the delay (grows each time).
const ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Last.fm's temporary error codes: 8 operation failed, 11 service offline,
/// 16 temporarily unavailable, 29 rate limit exceeded.
const TEMPORARY_ERRORS: [u32; 4] = [8, 11, 16, 29];
/// Page size for `user.getTopAlbums` when fetching listened albums.
const PAGE_SIZE: u32 = 1000;

/// A Last.fm time period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Overall,
    TwelveMonths,
    ThreeMonths,
}

impl Period {
    fn as_str(self) -> &'static str {
        match self {
            Self::Overall => "overall",
            Self::TwelveMonths => "12month",
            Self::ThreeMonths => "3month",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopArtist {
    pub name: String,
    pub plays: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopAlbum {
    pub artist: String,
    pub album: String,
    pub plays: u32,
}

pub struct LastFm {
    key: String,
    user: String,
    http: reqwest::Client,
}

impl LastFm {
    pub fn new(key: &str, user: &str) -> Result<Self> {
        Ok(Self {
            key: key.to_owned(),
            user: user.to_owned(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    pub async fn top_artists(&self, period: Period, limit: u32) -> Result<Vec<TopArtist>> {
        let response: TopArtistsResponse = self.get("user.gettopartists", period, limit, 1).await?;
        response
            .topartists
            .artist
            .into_iter()
            .map(|a| {
                Ok(TopArtist {
                    plays: plays(&a.playcount)?,
                    name: a.name,
                })
            })
            .collect()
    }

    pub async fn top_albums(&self, period: Period, limit: u32) -> Result<Vec<TopAlbum>> {
        Ok(self.top_albums_page(period, limit, 1).await?.0)
    }

    /// All albums with at least `min_plays` plays of all time. The result is read
    /// from the cache if it is less than a day old and was fetched for the same
    /// user with the same threshold.
    pub async fn listened_albums(&self, cache: &Path, min_plays: u32) -> Result<Vec<TopAlbum>> {
        let now = unix_now();
        if let Some(cached) = read_cache(cache)
            && cached.user == self.user
            && cached.min_plays == min_plays
            && now.saturating_sub(cached.fetched) < CACHE_MAX_AGE.as_secs()
        {
            return Ok(cached.albums);
        }

        let mut albums = Vec::new();
        let mut page = 1;
        loop {
            let (mut items, total_pages) = self
                .top_albums_page(Period::Overall, PAGE_SIZE, page)
                .await?;
            // The list is sorted by play count, highest first.
            let below = items.iter().position(|a| a.plays < min_plays);
            if let Some(index) = below {
                items.truncate(index);
            }
            albums.append(&mut items);
            if below.is_some() || page >= total_pages {
                break;
            }
            page += 1;
        }

        let cached = AlbumCache {
            user: self.user.clone(),
            min_plays,
            fetched: now,
            albums,
        };
        if let Err(e) = write_cache(cache, &cached) {
            log::warn!("cannot save the Last.fm cache: {e:#}");
        }
        Ok(cached.albums)
    }

    async fn top_albums_page(
        &self,
        period: Period,
        limit: u32,
        page: u32,
    ) -> Result<(Vec<TopAlbum>, u32)> {
        let response: TopAlbumsResponse =
            self.get("user.gettopalbums", period, limit, page).await?;
        let total_pages = response
            .topalbums
            .attr
            .as_ref()
            .and_then(|a| a.total_pages.parse().ok())
            .unwrap_or(1);
        let albums = response
            .topalbums
            .album
            .into_iter()
            .map(|a| {
                Ok(TopAlbum {
                    plays: plays(&a.playcount)?,
                    artist: a.artist.name,
                    album: a.name,
                })
            })
            .collect::<Result<_>>()?;
        Ok((albums, total_pages))
    }

    /// Last.fm sometimes answers with a temporary error, so the call is tried
    /// at most [`ATTEMPTS`] times.
    async fn get<T: DeserializeOwned>(
        &self,
        method: &str,
        period: Period,
        limit: u32,
        page: u32,
    ) -> Result<T> {
        let mut attempt = 1;
        loop {
            match self.get_once(method, period, limit, page).await {
                Err(Failure::Temporary(e)) if attempt < ATTEMPTS => {
                    log::warn!("Last.fm call {method} failed, retrying: {e:#}");
                    tokio::time::sleep(RETRY_DELAY * attempt).await;
                    attempt += 1;
                }
                Err(Failure::Temporary(e) | Failure::Permanent(e)) => return Err(e),
                Ok(value) => return Ok(value),
            }
        }
    }

    async fn get_once<T: DeserializeOwned>(
        &self,
        method: &str,
        period: Period,
        limit: u32,
        page: u32,
    ) -> Result<T, Failure> {
        let limit = limit.to_string();
        let page = page.to_string();
        let response = self
            .http
            .get(API)
            .query(&[
                ("method", method),
                ("user", &self.user),
                ("period", period.as_str()),
                ("limit", &limit),
                ("page", &page),
                ("api_key", &self.key),
                ("format", "json"),
            ])
            .send()
            .await
            .context("Last.fm is not responding")
            .map_err(Failure::Temporary)?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("could not read the Last.fm response")
            .map_err(Failure::Temporary)?;
        if let Ok(error) = serde_json::from_str::<ApiError>(&body) {
            let e = anyhow!("Last.fm error {}: {}", error.error, error.message);
            return Err(if TEMPORARY_ERRORS.contains(&error.error) {
                Failure::Temporary(e)
            } else {
                Failure::Permanent(e)
            });
        }
        if !status.is_success() {
            let e = anyhow!("Last.fm {status}: {body}");
            return Err(if status.is_server_error() {
                Failure::Temporary(e)
            } else {
                Failure::Permanent(e)
            });
        }
        serde_json::from_str(&body)
            .context("could not read the Last.fm response")
            .map_err(Failure::Permanent)
    }
}

/// Default path of the cache.
pub fn albums_cache_path() -> Result<PathBuf> {
    Ok(crate::config::cache_dir()?.join("lastfm-albums.json"))
}

fn plays(playcount: &str) -> Result<u32> {
    playcount
        .parse()
        .with_context(|| format!("Last.fm returned an invalid play count {playcount:?}"))
}

enum Failure {
    Temporary(anyhow::Error),
    Permanent(anyhow::Error),
}

#[derive(Serialize, Deserialize)]
struct AlbumCache {
    user: String,
    min_plays: u32,
    /// Fetch time in Unix seconds.
    fetched: u64,
    albums: Vec<TopAlbum>,
}

/// A missing or broken cache is the same as no cache: it is fetched again and
/// overwritten. A broken one logs a warning.
fn read_cache(path: &Path) -> Option<AlbumCache> {
    read_json(path, "Last.fm cache").unwrap_or_else(|e| {
        log::warn!("{e:#}, fetching it again");
        None
    })
}

/// Saves the cache atomically as compact JSON (the file can be large).
fn write_cache(path: &Path, cache: &AlbumCache) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(cache)?)
        .with_context(|| format!("cannot save {}", path.display()))
}

#[derive(Deserialize)]
struct ApiError {
    error: u32,
    message: String,
}

#[derive(Deserialize)]
struct TopArtistsResponse {
    topartists: TopArtists,
}

#[derive(Deserialize)]
struct TopArtists {
    artist: Vec<ApiArtist>,
}

#[derive(Deserialize)]
struct ApiArtist {
    name: String,
    playcount: String,
}

#[derive(Deserialize)]
struct TopAlbumsResponse {
    topalbums: TopAlbums,
}

#[derive(Deserialize)]
struct TopAlbums {
    album: Vec<ApiAlbum>,
    #[serde(rename = "@attr")]
    attr: Option<PageAttr>,
}

#[derive(Deserialize)]
struct PageAttr {
    #[serde(rename = "totalPages")]
    total_pages: String,
}

#[derive(Deserialize)]
struct ApiAlbum {
    name: String,
    playcount: String,
    artist: AlbumArtist,
}

#[derive(Deserialize)]
struct AlbumArtist {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_albums_page() {
        let json = r#"{"topalbums":{"album":[
            {"artist":{"url":"u","name":"Foo Fighters","mbid":""},"image":[],
             "mbid":"","url":"u","playcount":"512","@attr":{"rank":"1"},"name":"Wasting Light"}],
            "@attr":{"user":"example-user","totalPages":"14","page":"1","total":"13738","perPage":"1000"}}}"#;
        let response: TopAlbumsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.topalbums.attr.unwrap().total_pages, "14");
        assert_eq!(response.topalbums.album[0].artist.name, "Foo Fighters");
        assert_eq!(plays(&response.topalbums.album[0].playcount).unwrap(), 512);
    }

    #[test]
    fn error_body_is_recognised() {
        let error: ApiError = serde_json::from_str(
            r#"{"message":"Invalid API key - You must be granted a valid key by last.fm","error":10}"#,
        )
        .unwrap();
        assert_eq!(error.error, 10);
        assert!(serde_json::from_str::<ApiError>(r#"{"topalbums":{"album":[]}}"#).is_err());
    }

    #[test]
    fn cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck/lastfm-albums.json");
        assert!(read_cache(&path).is_none());
        let cache = AlbumCache {
            user: "example-user".to_owned(),
            min_plays: 3,
            fetched: 1,
            albums: vec![TopAlbum {
                artist: "Foo Fighters".to_owned(),
                album: "Wasting Light".to_owned(),
                plays: 512,
            }],
        };
        write_cache(&path, &cache).unwrap();
        let read = read_cache(&path).unwrap();
        assert_eq!(read.albums, cache.albums);
        assert_eq!(read.fetched, 1);

        std::fs::write(&path, "{").unwrap();
        assert!(read_cache(&path).is_none());
    }
}
