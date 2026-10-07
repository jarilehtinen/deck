//! The subcommands `deck taste` and `deck curate`, with which the curation run (Claude)
//! reads the taste profile and offers its candidates for the Curated list.
//!
//! The check rules are a pure function, [`check`]: the candidates, their Spotify hits,
//! the shelf, the history and Last.fm's listened albums → a report and the list's albums.
//!
//! After too many Web API calls Spotify blocks the whole app for hours (429), Deck's
//! search included. That is why searches go through [`lookup`]: already known
//! candidates are not searched, a repeated search comes from a one-day cache
//! (`curate-searches.json`), there are at most [`SEARCH_BUDGET`] searches a day, and
//! the first 429 blocks searches for the `retry-after` period. The block is shared
//! with Deck itself ([`RateLimit`]). The same [`SearchCache`] also counts the searches
//! of `deck genre add`.

use std::{io::Read, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    catalog::{Album, Catalog, FoundAlbum, FoundSeed, RateLimited},
    config::{self, Config},
    lastfm::{self, LastFm, Period, TopAlbum, TopArtist},
    lists::{Curated, CuratedFiles, HistoryEntry, ListAlbum, normalize, same_album, track_key},
    rate_limit::RateLimit,
    shelf::Shelf,
    store::{read_json, save_json, unix_now},
};

/// Length of the Curated list.
pub const MAX_ALBUMS: usize = 20;
/// This many plays on Last.fm make an album listened.
pub const LISTENED_MIN_PLAYS: u32 = 3;
/// The period of the `3year` artist list in `deck taste`, in seconds.
const THREE_YEARS: u64 = 3 * 365 * 24 * 60 * 60;
/// At most this many Spotify searches a day, across all `deck curate` runs.
pub const SEARCH_BUDGET: usize = 30;
/// A search stays in the cache for a day, and [`SEARCH_BUDGET`] counts the same period.
const SEARCH_MAX_AGE: u64 = 24 * 60 * 60;
/// Pause between Spotify searches.
pub const SEARCH_PAUSE: Duration = Duration::from_millis(500);
/// Length of the block if the 429 response has no `retry-after`.
pub const DEFAULT_BLOCK: u64 = 60 * 60;

/// A suggestion from the curator, read from stdin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Candidate {
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// No matching album was found on Spotify.
    NotFound,
    /// The same URI, or the same artist and album, is already on the shelf.
    OnShelf,
    /// Suggested in an earlier round.
    SuggestedBefore,
    /// At least [`LISTENED_MIN_PLAYS`] plays on Last.fm.
    Listened,
    /// The same album is already on this list.
    Duplicate,
    /// Not searched on Spotify, because [`SEARCH_BUDGET`] has been used up.
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Accepted {
    pub artist: String,
    pub album: String,
    pub year: Option<u16>,
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub artist: String,
    pub album: String,
    pub reason: Rejection,
    /// The album found on Spotify, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// Output of `deck curate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The albums taken onto the list, in suggestion order, at most [`MAX_ALBUMS`].
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
    /// Albums that passed the check but did not fit on the list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unused: Vec<Accepted>,
    /// `MAX_ALBUMS` − the number accepted.
    pub missing: usize,
    /// Spotify searches in this run (cache misses).
    pub searched: usize,
    /// What is left of the day's [`SEARCH_BUDGET`].
    pub searches_left: usize,
}

/// Checks the candidates. `hits[i]` holds the Spotify search hits for `candidates[i]`,
/// `None` if it was not searched. Returns the report and the albums to put on the
/// list, with their reasons. The report's `searched` and `searches_left` stay zero.
pub fn check(
    candidates: &[Candidate],
    hits: &[Option<Vec<FoundAlbum>>],
    shelf: &[Album],
    history: &[HistoryEntry],
    listened: &[TopAlbum],
) -> (Report, Vec<ListAlbum>) {
    let mut passed: Vec<ListAlbum> = Vec::new();
    let mut rejected = Vec::new();

    for (candidate, found) in candidates.iter().zip(hits) {
        let reject = |reason, uri: Option<&str>| Rejected {
            artist: candidate.artist.clone(),
            album: candidate.album.clone(),
            reason,
            uri: uri.map(str::to_owned),
        };
        if let Some(reason) = known(candidate, shelf, history, listened) {
            rejected.push(reject(reason, None));
            continue;
        }
        let Some(found) = found else {
            rejected.push(reject(Rejection::NotChecked, None));
            continue;
        };
        let Some(album) = pick(candidate, found) else {
            rejected.push(reject(Rejection::NotFound, None));
            continue;
        };
        // The same album by Spotify's names ([`known`] already checked the candidate's).
        let same = |artist: &str, name: &str| same_album(artist, name, &album.artist, &album.name);

        let reason = if shelf
            .iter()
            .any(|a| a.uri == album.uri || same(&a.artist, &a.name))
        {
            Some(Rejection::OnShelf)
        } else if history
            .iter()
            .any(|h| h.uri == album.uri || same(&h.artist, &h.name))
        {
            Some(Rejection::SuggestedBefore)
        } else if listened
            .iter()
            .any(|l| l.plays >= LISTENED_MIN_PLAYS && same(&l.artist, &l.album))
        {
            Some(Rejection::Listened)
        } else if passed
            .iter()
            .any(|p| p.album.uri == album.uri || same(&p.album.artist, &p.album.name))
        {
            Some(Rejection::Duplicate)
        } else {
            None
        };
        match reason {
            Some(reason) => rejected.push(reject(reason, Some(&album.uri))),
            None => passed.push(ListAlbum {
                album,
                reason: Some(candidate.reason.trim().to_owned()).filter(|r| !r.is_empty()),
            }),
        }
    }

    let unused = passed.split_off(passed.len().min(MAX_ALBUMS));
    let accepted_of = |albums: &[ListAlbum]| -> Vec<Accepted> {
        albums
            .iter()
            .map(|a| Accepted {
                artist: a.album.artist.clone(),
                album: a.album.name.clone(),
                year: a.album.year,
                uri: a.album.uri.clone(),
            })
            .collect()
    };
    let report = Report {
        accepted: accepted_of(&passed),
        rejected,
        unused: accepted_of(&unused),
        missing: MAX_ALBUMS - passed.len(),
        searched: 0,
        searches_left: 0,
    };
    (report, passed)
}

/// A rejection based on the candidate's own names alone: on the shelf, suggested
/// before or listened. Such a candidate need not be searched on Spotify.
fn known(
    candidate: &Candidate,
    shelf: &[Album],
    history: &[HistoryEntry],
    listened: &[TopAlbum],
) -> Option<Rejection> {
    let same =
        |artist: &str, name: &str| same_album(artist, name, &candidate.artist, &candidate.album);
    if shelf.iter().any(|a| same(&a.artist, &a.name)) {
        Some(Rejection::OnShelf)
    } else if history.iter().any(|h| same(&h.artist, &h.name)) {
        Some(Rejection::SuggestedBefore)
    } else if listened
        .iter()
        .any(|l| l.plays >= LISTENED_MIN_PLAYS && same(&l.artist, &l.album))
    {
        Some(Rejection::Listened)
    } else {
        None
    }
}

/// Picks the candidate's album from the hits: the artist and name must match when
/// normalised. Of several matches the original release is picked: an album before
/// a compilation or single, then the oldest, and on a tie the first in Spotify's
/// order.
fn pick(candidate: &Candidate, found: &[FoundAlbum]) -> Option<Album> {
    let artist = normalize(&candidate.artist);
    let name = normalize(&candidate.album);
    found
        .iter()
        .filter(|f| normalize(&f.album.name) == name)
        .filter(|f| f.artists.iter().any(|a| normalize(a) == artist))
        .min_by_key(|f| (f.album_type != "album", f.album.year.unwrap_or(u16::MAX)))
        .map(|f| f.album.clone())
}

/// `deck taste`: prints the taste profile as JSON.
pub async fn taste() -> Result<()> {
    let config = &Config::load()?;
    let lastfm = lastfm_client(config)?;
    let shelf = load_shelf()?;
    let files = CuratedFiles::default_paths()?;
    let history = files.history()?;

    let profile = Taste {
        lastfm: LastFmTaste {
            user: lastfm_user(config)?.to_owned(),
            top_artists: TopArtists {
                overall: lastfm.top_artists(Period::Overall, 200).await?,
                three_years: lastfm
                    .top_artists_since(unix_now().saturating_sub(THREE_YEARS), 100)
                    .await?,
                twelve_months: lastfm.top_artists(Period::TwelveMonths, 100).await?,
            },
            top_albums: lastfm.top_albums(Period::Overall, 200).await?,
        },
        shelf: shelf
            .iter()
            .map(|a| ShelfAlbum {
                artist: a.artist.clone(),
                album: a.name.clone(),
                year: a.year,
            })
            .collect(),
        history: history
            .iter()
            .map(|h| PastSuggestion {
                artist: h.artist.clone(),
                album: h.name.clone(),
                round: h.round.clone(),
                rejected: h.rejected,
                on_shelf: shelf
                    .iter()
                    .any(|a| a.uri == h.uri || same_album(&a.artist, &a.name, &h.artist, &h.name)),
            })
            .collect(),
    };
    println!("{}", serde_json::to_string_pretty(&profile)?);
    Ok(())
}

/// `deck curate [--dry-run]`: reads the candidates from stdin, prints a report and
/// (without `--dry-run`) publishes the list. Zero accepted without `--dry-run` is an error.
pub async fn curate(dry_run: bool) -> Result<()> {
    let config = &Config::load()?;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("cannot read stdin")?;
    let candidates: Vec<Candidate> = serde_json::from_str(&input)
        .context("stdin must be a JSON list of {\"artist\", \"album\", \"reason\"}")?;

    // The block is checked only right before the first real search ([`lookup`]), and
    // Deck signs in to Spotify only then: a run that needs no searches (all candidates
    // known or cached) succeeds even while blocked.
    let cache_path = SearchCache::default_path()?;
    let mut cache = SearchCache::load(&cache_path, unix_now());

    let shelf = load_shelf()?;
    let files = CuratedFiles::default_paths()?;
    let history = files.history()?;
    let mut catalog: Option<Catalog> = None;
    let listened = lastfm_client(config)?
        .listened_albums(&lastfm::albums_cache_path()?, LISTENED_MIN_PLAYS)
        .await?;

    let skip: Vec<bool> = candidates
        .iter()
        .map(|c| known(c, &shelf, &history, &listened).is_some())
        .collect();
    let looked_up = lookup(
        &candidates,
        &skip,
        &mut cache,
        unix_now(),
        SEARCH_PAUSE,
        async |artist: &str, album: &str| {
            if catalog.is_none() {
                catalog = Some(Catalog::connect_stored(config).await?);
            }
            let catalog = catalog.as_ref().expect("connected above");
            catalog.search_albums(artist, album).await
        },
    )
    .await;
    // Keep the searches of an interrupted run too.
    cache.save(&cache_path)?;
    let (hits, searched) = looked_up?;

    let (mut report, albums) = check(&candidates, &hits, &shelf, &history, &listened);
    report.searched = searched;
    report.searches_left = cache.searches_left();
    println!("{}", serde_json::to_string_pretty(&report)?);

    if dry_run {
        return Ok(());
    }
    if albums.is_empty() {
        bail!("no candidate was accepted, the current list was left as it is");
    }
    let now = jiff::Zoned::now();
    let week = now.date().iso_week_date();
    let curated = Curated {
        round: format!("{:04}-W{:02}", week.year(), week.week()),
        created: now.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        albums,
    };
    files.publish(&curated)
}

/// Fetches the candidates' hits with `search`. `skip`ped (already known) candidates
/// are not searched, a repeated search comes from the cache, and once
/// [`SEARCH_BUDGET`] is used up the rest are left unsearched (`None`). Returns the
/// hits and the number of searches made.
///
/// An active block is an error only when some candidate actually needs a search.
/// A 429 records a block in the cache for the `retry-after` period and stops at
/// once without retrying. The cache must be saved after an error too.
async fn lookup(
    candidates: &[Candidate],
    skip: &[bool],
    cache: &mut SearchCache,
    now: u64,
    pause: Duration,
    mut search: impl AsyncFnMut(&str, &str) -> Result<Vec<FoundAlbum>>,
) -> Result<(Vec<Option<Vec<FoundAlbum>>>, usize)> {
    let mut hits = Vec::with_capacity(candidates.len());
    let mut searched = 0;
    for (c, &skip) in candidates.iter().zip(skip) {
        if skip {
            hits.push(None);
            continue;
        }
        if let Some(found) = cache.get(&c.artist, &c.album) {
            hits.push(Some(found.to_vec()));
            continue;
        }
        if cache.searches_left() == 0 {
            hits.push(None);
            continue;
        }
        cache.ensure_open(now)?;
        if searched > 0 {
            tokio::time::sleep(pause).await;
        }
        searched += 1;
        match search(&c.artist, &c.album).await {
            Ok(found) => {
                cache.searches.push(CachedSearch {
                    artist: c.artist.clone(),
                    album: c.album.clone(),
                    fetched: now,
                    hits: found.clone(),
                });
                hits.push(Some(found));
            }
            Err(e) => {
                if let Some(limited) = e.downcast_ref::<RateLimited>() {
                    return Err(cache.block(now, limited));
                }
                return Err(e.context(format!(
                    "Spotify search failed for {} – {}",
                    c.artist, c.album
                )));
            }
        }
    }
    Ok((hits, searched))
}

/// Spotify searches for curation and own genres (`~/.cache/deck/curate-searches.json`):
/// the last day's searches with their hits. Dry-run rounds of the same run do not
/// search the same thing again, and both count toward [`SEARCH_BUDGET`]. The 429 block
/// is in `rate-limit.json` next to the cache, shared with Deck ([`RateLimit`]).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchCache {
    /// The 429 block; a cache made with `default` keeps it in memory only.
    #[serde(skip)]
    rate_limit: RateLimit,
    #[serde(default)]
    searches: Vec<CachedSearch>,
    /// Seed searches of `deck genre add`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seed_searches: Vec<CachedSeedSearch>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSeedSearch {
    artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track: Option<String>,
    fetched: u64,
    hits: Vec<FoundSeed>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedSearch {
    artist: String,
    album: String,
    /// Search time in Unix seconds.
    fetched: u64,
    hits: Vec<FoundAlbum>,
}

impl SearchCache {
    pub fn default_path() -> Result<std::path::PathBuf> {
        Ok(config::cache_dir()?.join("curate-searches.json"))
    }

    /// Reads the cache and drops expired entries. A missing file is an empty cache.
    /// A broken file (saving is atomic, so it only happens through hand editing) is
    /// empty too, but logs a warning, because the day's search count is lost with it.
    /// The 429 block is read from `rate-limit.json` in the same directory.
    pub fn load(path: &Path, now: u64) -> Self {
        let mut cache: Self = read_json(path, "search cache")
            .unwrap_or_else(|e| {
                log::warn!(
                    "{e:#}; starting with an empty cache, so today's search count is forgotten"
                );
                None
            })
            .unwrap_or_default();
        cache.rate_limit = RateLimit::at(&path.with_file_name("rate-limit.json"));
        cache.expire(now);
        cache
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        save_json(path, self)
    }

    fn expire(&mut self, now: u64) {
        self.searches
            .retain(|s| now.saturating_sub(s.fetched) < SEARCH_MAX_AGE);
        self.seed_searches
            .retain(|s| now.saturating_sub(s.fetched) < SEARCH_MAX_AGE);
    }

    /// An error if Spotify has blocked searches, in this or another Deck process.
    pub fn ensure_open(&self, now: u64) -> Result<()> {
        match self.rate_limit.blocked_until(now) {
            Some(until) => bail!(blocked_message(until, until - now)),
            None => Ok(()),
        }
    }

    /// An earlier search for the same artist and album. Names are compared normalised
    /// ([`normalize`]), because [`pick`] compares the hits the same way:
    /// "No Other (Remastered)" uses the search for "No Other".
    fn get(&self, artist: &str, album: &str) -> Option<&[FoundAlbum]> {
        let (artist, album) = (normalize(artist), normalize(album));
        self.searches
            .iter()
            .find(|s| normalize(&s.artist) == artist && normalize(&s.album) == album)
            .map(|s| s.hits.as_slice())
    }

    pub fn searches_left(&self) -> usize {
        SEARCH_BUDGET.saturating_sub(self.searches.len() + self.seed_searches.len())
    }

    /// An earlier search for a seed (`track` `None` = artist search). Names are
    /// compared normalised, the same way `deck genre add` compares hits ([`normalize`],
    /// [`track_key`]).
    pub fn get_seed(&self, artist: &str, track: Option<&str>) -> Option<&[FoundSeed]> {
        let artist = normalize(artist);
        let track = track.map(track_key);
        self.seed_searches
            .iter()
            .find(|s| normalize(&s.artist) == artist && s.track.as_deref().map(track_key) == track)
            .map(|s| s.hits.as_slice())
    }

    pub fn add_seed(&mut self, artist: &str, track: Option<&str>, now: u64, hits: Vec<FoundSeed>) {
        self.seed_searches.push(CachedSeedSearch {
            artist: artist.to_owned(),
            track: track.map(str::to_owned),
            fetched: now,
            hits,
        });
    }

    /// Records a 429 block for the `retry-after` period and returns an error that
    /// ends the run.
    pub fn block(&self, now: u64, limited: &RateLimited) -> anyhow::Error {
        let wait = limited.retry_after.unwrap_or(DEFAULT_BLOCK);
        self.rate_limit.block(now + wait);
        anyhow::anyhow!(blocked_message(now + wait, wait))
    }
}

/// Error message for a block. The curation run (curate.sh) logs it to `curate.log`.
fn blocked_message(until: u64, retry_after: u64) -> String {
    let until = i64::try_from(until)
        .ok()
        .and_then(|s| jiff::Timestamp::from_second(s).ok())
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_owned());
    format!(
        "Spotify is rate limiting Deck (429, retry-after {retry_after} s, until {until}). \
         Stop this run: Deck makes no Spotify searches before that."
    )
}

/// The Last.fm user from the config. There is no default: the error says what to
/// write in the file.
fn lastfm_user(config: &Config) -> Result<&str> {
    match config.lastfm_user.as_deref().map(str::trim) {
        Some(user) if !user.is_empty() => Ok(user),
        _ => bail!(
            "lastfm_user is missing from ~/.config/deck/config.toml. Add the line \
             lastfm_user = \"<your Last.fm username>\" (Curated needs your Last.fm \
             listening history)"
        ),
    }
}

fn lastfm_client(config: &Config) -> Result<LastFm> {
    let Some(key) = &config.lastfm_api_key else {
        bail!(
            "lastfm_api_key is missing from ~/.config/deck/config.toml. Create an API \
             account at https://www.last.fm/api/account/create and add the line \
             lastfm_api_key = \"<your API key>\""
        );
    };
    LastFm::new(key, lastfm_user(config)?)
}

/// The shelf, read-only. A broken shelf is an error: the check does not work without it.
fn load_shelf() -> Result<Vec<Album>> {
    Ok(Shelf::load(&Shelf::default_path()?)?.albums().to_vec())
}

#[derive(Serialize)]
struct Taste {
    lastfm: LastFmTaste,
    shelf: Vec<ShelfAlbum>,
    history: Vec<PastSuggestion>,
}

#[derive(Serialize)]
struct LastFmTaste {
    user: String,
    top_artists: TopArtists,
    /// The most played albums of all time.
    top_albums: Vec<TopAlbum>,
}

#[derive(Serialize)]
struct TopArtists {
    overall: Vec<TopArtist>,
    #[serde(rename = "3year")]
    three_years: Vec<TopArtist>,
    #[serde(rename = "12month")]
    twelve_months: Vec<TopArtist>,
}

#[derive(Serialize)]
struct ShelfAlbum {
    artist: String,
    album: String,
    year: Option<u16>,
}

#[derive(Serialize)]
struct PastSuggestion {
    artist: String,
    album: String,
    round: String,
    rejected: bool,
    on_shelf: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, artist: &str, name: &str, year: Option<u16>) -> Album {
        Album {
            id: id.to_owned(),
            uri: format!("spotify:album:{id}"),
            name: name.to_owned(),
            artist: artist.to_owned(),
            artist_id: format!("id-{artist}"),
            year,
        }
    }

    fn found(id: &str, artist: &str, name: &str, year: u16, album_type: &str) -> FoundAlbum {
        FoundAlbum {
            album: album(id, artist, name, Some(year)),
            artists: vec![artist.to_owned()],
            album_type: album_type.to_owned(),
        }
    }

    fn candidate(artist: &str, album: &str) -> Candidate {
        Candidate {
            artist: artist.to_owned(),
            album: album.to_owned(),
            reason: format!("Because {album}."),
        }
    }

    fn history(uri: &str, artist: &str, name: &str) -> HistoryEntry {
        HistoryEntry {
            uri: uri.to_owned(),
            name: name.to_owned(),
            artist: artist.to_owned(),
            round: "2026-W40".to_owned(),
            rejected: false,
        }
    }

    /// One candidate whose only hits are `hit`.
    fn check_one(
        hit: Vec<FoundAlbum>,
        shelf: &[Album],
        history: &[HistoryEntry],
        listened: &[TopAlbum],
    ) -> Report {
        check(
            &[candidate("Gene Clark", "No Other")],
            &[Some(hit)],
            shelf,
            history,
            listened,
        )
        .0
    }

    fn reasons(report: &Report) -> Vec<Rejection> {
        report.rejected.iter().map(|r| r.reason).collect()
    }

    fn no_other() -> Vec<FoundAlbum> {
        vec![found("noother", "Gene Clark", "No Other", 1974, "album")]
    }

    #[test]
    fn accepts_matching_album_with_reason() {
        let (report, albums) = check(
            &[candidate("gene clark", "No Other (Remastered)")],
            &[Some(no_other())],
            &[],
            &[],
            &[],
        );
        assert_eq!(
            report.accepted,
            vec![Accepted {
                artist: "Gene Clark".to_owned(),
                album: "No Other".to_owned(),
                year: Some(1974),
                uri: "spotify:album:noother".to_owned(),
            }]
        );
        assert!(report.rejected.is_empty());
        assert_eq!(report.missing, 19);
        assert_eq!(albums[0].album.uri, "spotify:album:noother");
        assert_eq!(
            albums[0].reason.as_deref(),
            Some("Because No Other (Remastered).")
        );
    }

    #[test]
    fn not_found_when_no_hit_matches() {
        let report = check_one(Vec::new(), &[], &[], &[]);
        assert_eq!(reasons(&report), [Rejection::NotFound]);
        assert_eq!(report.rejected[0].uri, None);

        // A wrong artist or a wrong album is not accepted.
        let wrong = vec![
            found("x", "Gene Clark", "White Light", 1971, "album"),
            found("y", "The Byrds", "No Other", 1974, "album"),
        ];
        assert_eq!(
            reasons(&check_one(wrong, &[], &[], &[])),
            [Rejection::NotFound]
        );
    }

    #[test]
    fn on_shelf_by_uri_or_name() {
        let by_uri = [album("noother", "Someone", "Else", None)];
        let report = check_one(no_other(), &by_uri, &[], &[]);
        assert_eq!(reasons(&report), [Rejection::OnShelf]);
        assert_eq!(
            report.rejected[0].uri.as_deref(),
            Some("spotify:album:noother")
        );

        let by_name = [album(
            "other-id",
            "Gene Clark",
            "No Other (Deluxe)",
            Some(1974),
        )];
        assert_eq!(
            reasons(&check_one(no_other(), &by_name, &[], &[])),
            [Rejection::OnShelf]
        );
    }

    #[test]
    fn suggested_before_by_uri_or_name() {
        let by_uri = [history("spotify:album:noother", "x", "y")];
        assert_eq!(
            reasons(&check_one(no_other(), &[], &by_uri, &[])),
            [Rejection::SuggestedBefore]
        );
        let by_name = [history("spotify:album:other", "Gene Clark", "No Other")];
        assert_eq!(
            reasons(&check_one(no_other(), &[], &by_name, &[])),
            [Rejection::SuggestedBefore]
        );
    }

    #[test]
    fn listened_needs_three_plays() {
        let listened = |plays| TopAlbum {
            artist: "Gene Clark".to_owned(),
            album: "No Other".to_owned(),
            plays,
        };
        assert_eq!(
            reasons(&check_one(no_other(), &[], &[], &[listened(3)])),
            [Rejection::Listened]
        );
        assert!(
            check_one(no_other(), &[], &[], &[listened(2)])
                .rejected
                .is_empty()
        );
    }

    #[test]
    fn duplicate_within_the_list() {
        let (report, albums) = check(
            &[
                candidate("Gene Clark", "No Other"),
                candidate("Gene Clark", "No Other (2003 Remaster)"),
            ],
            &[Some(no_other()), Some(no_other())],
            &[],
            &[],
            &[],
        );
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(albums.len(), 1);
        assert_eq!(reasons(&report), [Rejection::Duplicate]);
    }

    #[test]
    fn first_matching_rule_wins() {
        // On the shelf, in the history and listened: the shelf decides.
        let report = check_one(
            no_other(),
            &[album("noother", "Gene Clark", "No Other", None)],
            &[history("spotify:album:noother", "Gene Clark", "No Other")],
            &[TopAlbum {
                artist: "Gene Clark".to_owned(),
                album: "No Other".to_owned(),
                plays: 10,
            }],
        );
        assert_eq!(reasons(&report), [Rejection::OnShelf]);
    }

    #[test]
    fn picks_original_album_over_compilation_and_single() {
        let hits = vec![
            found("single", "Gene Clark", "No Other", 1974, "single"),
            found("reissue", "Gene Clark", "No Other (Deluxe)", 2019, "album"),
            found("comp", "Gene Clark", "No Other", 1970, "compilation"),
            found("orig", "Gene Clark", "No Other", 1974, "album"),
        ];
        let (report, _) = check(
            &[candidate("Gene Clark", "No Other")],
            &[Some(hits)],
            &[],
            &[],
            &[],
        );
        assert_eq!(report.accepted[0].uri, "spotify:album:orig");

        // A single alone is accepted when there is no album.
        let hits = vec![found("single", "Gene Clark", "No Other", 1974, "single")];
        let (report, _) = check(
            &[candidate("Gene Clark", "No Other")],
            &[Some(hits)],
            &[],
            &[],
            &[],
        );
        assert_eq!(report.accepted[0].uri, "spotify:album:single");
    }

    #[test]
    fn matches_any_album_artist() {
        let mut hit = found("csn", "Crosby, Stills & Nash", "CSN", 1977, "album");
        hit.artists = vec!["David Crosby".to_owned(), "Graham Nash".to_owned()];
        let (report, _) = check(
            &[candidate("Graham Nash", "CSN")],
            &[Some(vec![hit])],
            &[],
            &[],
            &[],
        );
        assert_eq!(report.accepted.len(), 1);
    }

    #[test]
    fn keeps_order_and_caps_at_twenty() {
        let candidates: Vec<Candidate> = (0..25)
            .map(|i| candidate(&format!("Artist {i}"), &format!("Album {i}")))
            .collect();
        let hits: Vec<Option<Vec<FoundAlbum>>> = (0..25)
            .map(|i| {
                Some(vec![found(
                    &format!("a{i}"),
                    &format!("Artist {i}"),
                    &format!("Album {i}"),
                    2000,
                    "album",
                )])
            })
            .collect();
        // The second candidate is on the shelf.
        let shelf = [album("a1", "Artist 1", "Album 1", None)];
        let (report, albums) = check(&candidates, &hits, &shelf, &[], &[]);

        assert_eq!(report.accepted.len(), MAX_ALBUMS);
        assert_eq!(report.missing, 0);
        assert_eq!(albums.len(), MAX_ALBUMS);
        let order: Vec<&str> = report.accepted.iter().map(|a| a.album.as_str()).collect();
        assert_eq!(order[..3], ["Album 0", "Album 2", "Album 3"]);
        assert_eq!(order[19], "Album 20");
        let unused: Vec<&str> = report.unused.iter().map(|a| a.album.as_str()).collect();
        assert_eq!(unused, ["Album 21", "Album 22", "Album 23", "Album 24"]);
        assert_eq!(reasons(&report), [Rejection::OnShelf]);
    }

    #[test]
    fn report_json_shape() {
        let (report, _) = check(
            &[
                candidate("Gene Clark", "No Other"),
                candidate("Nobody", "Nothing"),
            ],
            &[Some(no_other()), Some(Vec::new())],
            &[],
            &[],
            &[],
        );
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "accepted": [{
                    "artist": "Gene Clark",
                    "album": "No Other",
                    "year": 1974,
                    "uri": "spotify:album:noother",
                }],
                "rejected": [{"artist": "Nobody", "album": "Nothing", "reason": "not_found"}],
                "missing": 19,
                "searched": 0,
                "searches_left": 0,
            })
        );
    }

    #[test]
    fn candidates_parse_without_reason() {
        let parsed: Vec<Candidate> =
            serde_json::from_str(r#"[{"artist":"Gene Clark","album":"No Other"}]"#).unwrap();
        assert_eq!(parsed[0].reason, "");
        let (_, albums) = check(&parsed, &[Some(no_other())], &[], &[], &[]);
        assert_eq!(albums[0].reason, None);
    }

    /// A search function for tests: `results` answers the searches in order, and
    /// `calls` records the albums searched.
    fn fake_search<'a>(
        calls: &'a mut Vec<String>,
        mut results: Vec<Result<Vec<FoundAlbum>>>,
    ) -> impl AsyncFnMut(&str, &str) -> Result<Vec<FoundAlbum>> + 'a {
        results.reverse();
        async move |_artist: &str, album: &str| {
            calls.push(album.to_owned());
            results.pop().expect("unexpected search")
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[tokio::test]
    async fn known_candidates_are_not_searched() {
        let candidates = [
            candidate("Gene Clark", "No Other"),
            candidate("Ultra Bra", "Kroketti"),
        ];
        let mut cache = SearchCache::default();
        let mut calls = Vec::new();
        let search = fake_search(&mut calls, vec![Ok(no_other())]);
        let (hits, searched) = lookup(
            &candidates,
            &[false, true],
            &mut cache,
            NOW,
            Duration::ZERO,
            search,
        )
        .await
        .unwrap();
        assert_eq!(calls, ["No Other"]);
        assert_eq!(searched, 1);
        assert_eq!(hits, [Some(no_other()), None]);

        let listened = [TopAlbum {
            artist: "Ultra Bra".to_owned(),
            album: "Kroketti".to_owned(),
            plays: 5,
        }];
        let (report, _) = check(&candidates, &hits, &[], &[], &listened);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(reasons(&report), [Rejection::Listened]);
    }

    #[tokio::test]
    async fn repeated_dry_runs_search_only_new_candidates() {
        let mut cache = SearchCache::default();
        let mut calls = Vec::new();
        let first = [candidate("Gene Clark", "No Other")];
        lookup(
            &first,
            &[false],
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(&mut calls, vec![Ok(no_other())]),
        )
        .await
        .unwrap();

        // The next round an hour later: the same candidate comes from the cache.
        let second = [
            candidate("Gene Clark", "No Other"),
            candidate("Gene Clark", "White Light"),
        ];
        let (hits, searched) = lookup(
            &second,
            &[false, false],
            &mut cache,
            NOW + 3600,
            Duration::ZERO,
            fake_search(&mut calls, vec![Ok(Vec::new())]),
        )
        .await
        .unwrap();
        assert_eq!(calls, ["No Other", "White Light"]);
        assert_eq!(searched, 1);
        assert_eq!(hits, [Some(no_other()), Some(Vec::new())]);
        assert_eq!(cache.searches_left(), SEARCH_BUDGET - 2);
    }

    #[tokio::test]
    async fn budget_leaves_the_rest_unchecked() {
        let candidates: Vec<Candidate> = (0..SEARCH_BUDGET + 2)
            .map(|i| candidate(&format!("Artist {i}"), &format!("Album {i}")))
            .collect();
        let mut cache = SearchCache::default();
        let mut calls = Vec::new();
        let results = (0..SEARCH_BUDGET).map(|_| Ok(Vec::new())).collect();
        let (hits, searched) = lookup(
            &candidates,
            &vec![false; candidates.len()],
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(&mut calls, results),
        )
        .await
        .unwrap();
        assert_eq!(searched, SEARCH_BUDGET);
        assert_eq!(calls.len(), SEARCH_BUDGET);
        assert_eq!(cache.searches_left(), 0);
        assert_eq!(hits[SEARCH_BUDGET..], [None, None]);

        let (report, _) = check(&candidates, &hits, &[], &[], &[]);
        assert_eq!(
            reasons(&report)[SEARCH_BUDGET..],
            [Rejection::NotChecked, Rejection::NotChecked]
        );
    }

    #[test]
    fn cache_forgets_searches_after_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("curate-searches.json");
        let mut cache = SearchCache::default();
        cache.searches.push(CachedSearch {
            artist: "Gene Clark".to_owned(),
            album: "No Other".to_owned(),
            fetched: NOW,
            hits: no_other(),
        });
        cache.save(&path).unwrap();

        let cache = SearchCache::load(&path, NOW + SEARCH_MAX_AGE - 1);
        assert_eq!(cache.get("Gene Clark", "No Other"), Some(&no_other()[..]));
        assert_eq!(cache.searches_left(), SEARCH_BUDGET - 1);
        let cache = SearchCache::load(&path, NOW + SEARCH_MAX_AGE);
        assert_eq!(cache.get("Gene Clark", "No Other"), None);
        assert_eq!(cache.searches_left(), SEARCH_BUDGET);
    }

    #[tokio::test]
    async fn rate_limit_stops_the_run_and_blocks_until_retry_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("curate-searches.json");
        let candidates = [
            candidate("Gene Clark", "No Other"),
            candidate("Gene Clark", "White Light"),
            candidate("Gene Clark", "Roadmaster"),
        ];
        let skip = [false; 3];
        let mut cache = SearchCache::load(&path, NOW);
        let mut calls = Vec::new();
        let limited = RateLimited {
            retry_after: Some(34560),
        };
        let error = lookup(
            &candidates,
            &skip,
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(&mut calls, vec![Ok(no_other()), Err(limited.into())]),
        )
        .await
        .unwrap_err();
        cache.save(&path).unwrap();

        // The first 429 stops the run: the third is not searched, the second not retried.
        assert_eq!(calls, ["No Other", "White Light"]);
        let message = error.to_string();
        assert!(
            message.starts_with("Spotify is rate limiting Deck (429, retry-after 34560 s, until "),
            "{message}"
        );
        assert!(message.ends_with("Stop this run: Deck makes no Spotify searches before that."));

        // The next run searches nothing until the block is over, not even cache misses.
        let mut cache = SearchCache::load(&path, NOW + 60);
        assert_eq!(cache.rate_limit.blocked_until(NOW + 60), Some(NOW + 34560));
        assert!(
            cache
                .ensure_open(NOW + 60)
                .unwrap_err()
                .to_string()
                .contains("retry-after 34500 s")
        );
        let mut calls = Vec::new();
        let error = lookup(
            &candidates,
            &skip,
            &mut cache,
            NOW + 60,
            Duration::ZERO,
            fake_search(&mut calls, Vec::new()),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("429"));
        assert!(calls.is_empty());

        // After the block searching resumes, and what was searched before it is cached.
        let mut cache = SearchCache::load(&path, NOW + 34560);
        assert!(cache.ensure_open(NOW + 34560).is_ok());
        let mut calls = Vec::new();
        let results = vec![Ok(Vec::new()), Ok(Vec::new())];
        let (_, searched) = lookup(
            &candidates,
            &skip,
            &mut cache,
            NOW + 34560,
            Duration::ZERO,
            fake_search(&mut calls, results),
        )
        .await
        .unwrap();
        assert_eq!(calls, ["White Light", "Roadmaster"]);
        assert_eq!(searched, 2);
    }

    #[tokio::test]
    async fn block_does_not_stop_a_run_that_needs_no_search() {
        let mut cache = SearchCache::default();
        cache.searches.push(CachedSearch {
            artist: "Gene Clark".to_owned(),
            album: "No Other".to_owned(),
            fetched: NOW,
            hits: no_other(),
        });
        cache.rate_limit.block(NOW + 600);
        let candidates = [
            candidate("Gene Clark", "No Other"),
            candidate("Ultra Bra", "Kroketti"),
        ];
        let mut calls = Vec::new();
        let (hits, searched) = lookup(
            &candidates,
            &[false, true],
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(&mut calls, Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(hits, [Some(no_other()), None]);
        assert_eq!(searched, 0);
        assert!(calls.is_empty());
    }

    #[test]
    fn cache_matches_normalized_names() {
        let mut cache = SearchCache::default();
        cache.searches.push(CachedSearch {
            artist: "Gene Clark".to_owned(),
            album: "No Other".to_owned(),
            fetched: NOW,
            hits: no_other(),
        });
        assert_eq!(
            cache.get("gene clark", "No Other (Remastered)"),
            Some(&no_other()[..])
        );
        assert_eq!(cache.get("Gene Clark", "White Light"), None);

        cache.add_seed("The Troggs", Some("Wild Thing"), NOW, Vec::new());
        cache.add_seed("Love", None, NOW, Vec::new());
        assert!(
            cache
                .get_seed("the troggs", Some("Wild Thing - Mono"))
                .is_some()
        );
        assert!(cache.get_seed("Love", None).is_some());
        assert!(cache.get_seed("Love", Some("Alone Again Or")).is_none());
        assert!(cache.get_seed("The Troggs", None).is_none());
    }

    #[test]
    fn broken_cache_file_is_an_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("curate-searches.json");
        std::fs::write(&path, "{\"searches\": ").unwrap();
        let cache = SearchCache::load(&path, NOW);
        assert_eq!(cache.searches_left(), SEARCH_BUDGET);
    }

    #[tokio::test]
    async fn rate_limit_without_retry_after_blocks_an_hour() {
        let mut cache = SearchCache::default();
        let mut calls = Vec::new();
        let limited = RateLimited { retry_after: None };
        lookup(
            &[candidate("Gene Clark", "No Other")],
            &[false],
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(&mut calls, vec![Err(limited.into())]),
        )
        .await
        .unwrap_err();
        assert_eq!(
            cache.rate_limit.blocked_until(NOW),
            Some(NOW + DEFAULT_BLOCK)
        );
    }

    #[tokio::test]
    async fn other_errors_name_the_candidate() {
        let mut cache = SearchCache::default();
        let mut calls = Vec::new();
        let error = lookup(
            &[candidate("Gene Clark", "No Other")],
            &[false],
            &mut cache,
            NOW,
            Duration::ZERO,
            fake_search(
                &mut calls,
                vec![Err(anyhow::anyhow!("Spotify is not responding"))],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "Spotify search failed for Gene Clark – No Other: Spotify is not responding"
        );
        assert!(cache.ensure_open(NOW).is_ok());
        assert_eq!(cache.searches_left(), SEARCH_BUDGET);
    }

    #[test]
    fn lastfm_user_is_required() {
        let config = Config {
            lastfm_api_key: Some("key".to_owned()),
            ..Config::default()
        };
        let Err(error) = lastfm_client(&config) else {
            panic!("lastfm_client accepted a config without lastfm_user");
        };
        let error = format!("{error:#}");
        assert!(
            error.contains("lastfm_user = \"<your Last.fm username>\""),
            "{error}"
        );

        let blank = Config {
            lastfm_user: Some("  ".to_owned()),
            ..config
        };
        assert!(lastfm_user(&blank).is_err());
    }
}
