//! The subcommands `deck genre add | remove | list | prompt`: the user's AI adds own
//! genres ([`MyGenres`]) following the instructions in `genres/prompt.md`.
//!
//! The check is [`check`]: seeds → accepted, and rejected with reasons. Spotify is
//! behind the [`Spotify`] trait so the rules can be tested without the network. URIs
//! are checked with librespot (metadata and the apollo station), which does not use
//! the Web API. Only a seed given by name is searched on the Web API, with Curated's
//! search cap and cache ([`SearchCache`]); the first 429 ends the run.

use std::{
    collections::HashSet,
    fmt,
    io::{IsTerminal, Read},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use librespot_core::SpotifyUri;
use librespot_metadata::{Artist as SpotifyArtist, Metadata, Track as SpotifyTrack};
use serde::{Deserialize, Serialize};

use crate::{
    catalog::{Catalog, FoundSeed, RateLimited},
    config::Config,
    curate::{SEARCH_PAUSE, SearchCache},
    genres::{self, Genre, MyGenres, Seed, is_seed_uri, slug},
    lists::{normalize, track_key},
    player::{self, SharedSession},
    radio,
    store::unix_now,
};

/// Instructions for the AI, `deck genre prompt`.
pub const PROMPT: &str = include_str!("../genres/prompt.md");
/// Usage of `deck genre`: shown for a bare `deck genre` or an unknown subcommand.
pub const USAGE: &str = "\
usage: deck genre add [--dry-run] < genre.json
       deck genre remove <name>
       deck genre list
       deck genre prompt

Own genres are put together with an AI: `deck genre prompt` prints its instructions.
Run `deck genre add` without input to see an example genre.json.
";
/// Usage of `deck genre add` when stdin is a terminal and no input is redirected.
pub const ADD_USAGE: &str = r#"usage: deck genre add [--dry-run] < genre.json

Reads a genre as JSON from stdin, checks its seeds on Spotify and prints a report.
Without --dry-run the genre is saved if at least 3 seeds are accepted.

Example genre.json, one seed with a Spotify URI and one by the artist's name:

  {"name": "60s rock", "seeds": [
    {"artist": "The Troggs", "uri": "spotify:artist:57xdnSVt4ahJCIXYLieQ25"},
    {"artist": "Love"}
  ]}

Tip: `deck genre prompt` prints instructions for an AI that puts a genre together.
`deck genre list` shows all genres, `deck genre remove <name>` removes an own genre.
"#;

/// A command-line usage error: `main` prints the usage as it is and exits with code 2.
#[derive(Debug)]
pub struct Usage(pub &'static str);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Usage {}

/// A genre is saved only once this many seeds have been accepted.
pub const MIN_SEEDS: usize = 3;
/// At most this many seeds in one genre.
pub const MAX_SEEDS: usize = 20;
/// A station must give at least this many tracks.
const STATION_MIN: usize = 20;
/// Sample tracks from a station in the report.
const STATION_SAMPLE: usize = 6;

/// Input of `deck genre add` from stdin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Input {
    pub name: String,
    pub seeds: Vec<SeedInput>,
}

/// A suggested seed: an artist or (with `track`) a track, with or without a URI.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SeedInput {
    pub artist: String,
    #[serde(default)]
    pub track: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rejection {
    /// Not an artist or track URI, or the type does not match `track`.
    InvalidUri,
    /// The same seed is already earlier in the list.
    Duplicate,
    /// Not searched, because the daily search cap has been reached.
    NotChecked,
    /// The search found no match, or the URI's metadata could not be fetched.
    NotFound,
    /// The name of the URI's artist or track does not match the given one.
    NameMismatch,
    /// The station gave fewer than [`STATION_MIN`] tracks, or fetching it failed.
    NoStation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Accepted {
    pub artist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    pub uri: String,
    /// Sample tracks from the station: `Artist · Track (year)`.
    pub station: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub artist: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub reason: Rejection,
    /// Spotify's name when it did not match (`name_mismatch`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Replaces {
    /// A genre shipped with Deck.
    BuiltIn,
    /// An earlier own genre.
    Own,
}

/// Output of `deck genre add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaces: Option<Replaces>,
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
    /// [`MIN_SEEDS`] − the number of accepted seeds.
    pub missing: usize,
    /// Web API searches in this run (cache misses).
    pub searched: usize,
    pub searches_left: usize,
    pub written: bool,
}

/// Result of the check: the parts of the report and the seeds to save.
#[derive(Debug, Default)]
pub struct Checked {
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
    pub seeds: Vec<Seed>,
    pub searched: usize,
}

/// Spotify for the check.
pub trait Spotify {
    /// Web API search: an artist (`track` is `None`) or a track.
    async fn search(&mut self, artist: &str, track: Option<&str>) -> Result<Vec<FoundSeed>>;
    /// A seed's names via librespot: the track name and artists, or the artist name.
    async fn names(&mut self, uri: &str) -> Result<FoundSeed>;
    /// The first page of the seed's apollo station.
    async fn station(&mut self, uri: &str) -> Result<Vec<radio::Track>>;
    /// The album's release year for the sample.
    async fn year(&mut self, album_uri: &str) -> Option<u16>;
}

/// Checks the seeds in order; the first matching rule rejects (see [`Rejection`]).
/// Searching uses up `cache`'s daily cap. A 429 records the block in `cache` and
/// returns an error; the cache must be saved after an error too.
pub async fn check(
    seeds: &[SeedInput],
    cache: &mut SearchCache,
    now: u64,
    pause: Duration,
    spotify: &mut impl Spotify,
) -> Result<Checked> {
    let mut out = Checked::default();
    let mut seen_uris: HashSet<String> = HashSet::new();
    let mut seen_names: HashSet<(String, Option<String>)> = HashSet::new();

    for seed in seeds {
        let reject = |reason, uri: Option<&str>, spotify_name: Option<String>| Rejected {
            artist: seed.artist.clone(),
            track: seed.track.clone(),
            uri: uri.map(str::to_owned),
            reason,
            spotify_name,
        };
        if let Some(uri) = &seed.uri
            && !uri_fits(uri, seed.track.is_some())
        {
            out.rejected
                .push(reject(Rejection::InvalidUri, Some(uri), None));
            continue;
        }
        let name_key = (
            normalize(&seed.artist),
            seed.track.as_deref().map(track_key),
        );
        let duplicate = match &seed.uri {
            Some(uri) => seen_uris.contains(uri),
            None => seen_names.contains(&name_key),
        };
        if duplicate {
            out.rejected
                .push(reject(Rejection::Duplicate, seed.uri.as_deref(), None));
            continue;
        }
        seen_names.insert(name_key);

        let found = match &seed.uri {
            Some(uri) => match spotify.names(uri).await {
                Ok(found) if matches(seed, &found) => found,
                Ok(found) => {
                    let name = display_name(seed, &found);
                    out.rejected
                        .push(reject(Rejection::NameMismatch, Some(uri), Some(name)));
                    continue;
                }
                Err(e) => {
                    log::warn!("{uri}: {e:#}");
                    out.rejected
                        .push(reject(Rejection::NotFound, Some(uri), None));
                    continue;
                }
            },
            None => {
                let track = seed.track.as_deref();
                let hits = match cache.get_seed(&seed.artist, track) {
                    Some(hits) => hits.to_vec(),
                    None if cache.searches_left() == 0 => {
                        out.rejected.push(reject(Rejection::NotChecked, None, None));
                        continue;
                    }
                    None => {
                        cache.ensure_open(now)?;
                        if out.searched > 0 {
                            tokio::time::sleep(pause).await;
                        }
                        out.searched += 1;
                        let hits = match spotify.search(&seed.artist, track).await {
                            Ok(hits) => hits,
                            Err(e) => match e.downcast_ref::<RateLimited>() {
                                Some(limited) => return Err(cache.block(now, limited)),
                                None => {
                                    return Err(e.context(format!(
                                        "Spotify search failed for {}",
                                        seed_label(seed)
                                    )));
                                }
                            },
                        };
                        cache.add_seed(&seed.artist, track, now, hits.clone());
                        hits
                    }
                };
                match hits.into_iter().find(|hit| matches(seed, hit)) {
                    Some(hit) => hit,
                    None => {
                        out.rejected.push(reject(Rejection::NotFound, None, None));
                        continue;
                    }
                }
            }
        };
        if !seen_uris.insert(found.uri.clone()) {
            out.rejected
                .push(reject(Rejection::Duplicate, Some(&found.uri), None));
            continue;
        }

        let tracks = match spotify.station(&found.uri).await {
            Ok(tracks) if tracks.len() >= STATION_MIN => tracks,
            Ok(_) => {
                out.rejected
                    .push(reject(Rejection::NoStation, Some(&found.uri), None));
                continue;
            }
            Err(e) => {
                log::warn!("{}: {e:#}", found.uri);
                out.rejected
                    .push(reject(Rejection::NoStation, Some(&found.uri), None));
                continue;
            }
        };
        let mut station = Vec::new();
        for track in sample(&tracks) {
            let year = spotify.year(&track.album.uri).await;
            station.push(match year {
                Some(year) => format!("{} · {} ({year})", track.artist, track.name),
                None => format!("{} · {}", track.artist, track.name),
            });
        }
        out.seeds.push(Seed {
            uri: found.uri.clone(),
            name: display_name(seed, &found),
        });
        out.accepted.push(Accepted {
            artist: seed.artist.clone(),
            track: seed.track.clone(),
            uri: found.uri,
            station,
        });
    }
    Ok(out)
}

/// The URI is an artist's or a track's, and a track URI is given only with `track`.
fn uri_fits(uri: &str, is_track: bool) -> bool {
    is_seed_uri(uri) && uri.starts_with("spotify:track:") == is_track
}

/// Whether Spotify's artist or track matches the suggested one: for an artist the
/// name, for a track the name and one of the artists.
fn matches(seed: &SeedInput, found: &FoundSeed) -> bool {
    let artist = normalize(&seed.artist);
    let artist_matches = found.artists.iter().any(|a| normalize(a) == artist);
    match &seed.track {
        Some(track) => artist_matches && track_key(track) == track_key(&found.name),
        None => normalize(&found.name) == artist,
    }
}

/// Spotify's name in the catalog's style: `Artist` or `Artist · Track`. For a track
/// the artist is the one matching the suggestion, otherwise the first one.
fn display_name(seed: &SeedInput, found: &FoundSeed) -> String {
    if seed.track.is_none() && !found.uri.starts_with("spotify:track:") {
        return found.name.clone();
    }
    let artist = normalize(&seed.artist);
    let shown = found
        .artists
        .iter()
        .find(|a| normalize(a) == artist)
        .or(found.artists.first())
        .map_or("", String::as_str);
    format!("{shown} · {}", found.name)
}

fn seed_label(seed: &SeedInput) -> String {
    match &seed.track {
        Some(track) => format!("{} – {track}", seed.artist),
        None => seed.artist.clone(),
    }
}

/// [`STATION_SAMPLE`] tracks at even intervals. The first is skipped, because it is
/// often the seed itself.
fn sample(tracks: &[radio::Track]) -> Vec<&radio::Track> {
    let rest = tracks.get(1..).unwrap_or_default();
    let step = (rest.len() / STATION_SAMPLE).max(1);
    rest.iter().step_by(step).take(STATION_SAMPLE).collect()
}

/// The genre's name and id from the input: the name in lowercase, as in the catalog.
fn genre_name(input: &Input) -> Result<(String, String)> {
    let name = input.name.trim().to_lowercase();
    let id = slug(&name);
    if id.is_empty() {
        bail!("the genre needs a name");
    }
    if input.seeds.is_empty() || input.seeds.len() > MAX_SEEDS {
        bail!("give 1–{MAX_SEEDS} seeds, not {}", input.seeds.len());
    }
    Ok((id, name))
}

/// Whether the genre replaces a catalog genre or an earlier own genre.
fn replaces(id: &str, my: &MyGenres, catalog: &[Genre]) -> Option<Replaces> {
    if my.find(id).is_some() {
        Some(Replaces::Own)
    } else if catalog.iter().any(|g| g.id == id) {
        Some(Replaces::BuiltIn)
    } else {
        None
    }
}

/// `deck genre add [--dry-run]`: reads a genre from stdin, prints a report and
/// (without `--dry-run`) saves the genre if at least [`MIN_SEEDS`] seeds are accepted.
pub async fn add(dry_run: bool) -> Result<()> {
    let stdin = std::io::stdin();
    let text = read_input(stdin.is_terminal(), stdin)?;
    let input: Input = serde_json::from_str(&text).context(
        "stdin must be JSON {\"name\", \"seeds\": [{\"artist\", \"track\"?, \"uri\"?}]}",
    )?;
    let (id, name) = genre_name(&input)?;

    // A broken own-genres file stops the run before Spotify is contacted.
    let my_path = MyGenres::default_path()?;
    let mut my = MyGenres::load(&my_path)?;
    let replaces = replaces(&id, &my, &genres::catalog());

    let cache_path = SearchCache::default_path()?;
    let mut cache = SearchCache::load(&cache_path, unix_now());
    let mut spotify = Live::connect(Config::load()?).await?;
    let checked = check(
        &input.seeds,
        &mut cache,
        unix_now(),
        SEARCH_PAUSE,
        &mut spotify,
    )
    .await;
    // Keep the searches and the 429 block of an interrupted run too.
    cache.save(&cache_path)?;
    let checked = checked?;

    let report_seeds = checked.seeds.len();
    let enough = report_seeds >= MIN_SEEDS;
    let written = !dry_run && enough;
    if written {
        my.put(Genre {
            id: id.clone(),
            name: name.clone(),
            seeds: checked.seeds,
        })?;
    }
    let report = Report {
        id,
        name: name.clone(),
        replaces,
        accepted: checked.accepted,
        rejected: checked.rejected,
        missing: MIN_SEEDS.saturating_sub(report_seeds),
        searched: checked.searched,
        searches_left: cache.searches_left(),
        written,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    if written {
        eprintln!(
            "Saved {name} to {}. Restart Deck to see it in Genres.",
            my_path.display()
        );
    } else if !dry_run {
        bail!("at least {MIN_SEEDS} seeds must be accepted, the genre was not saved");
    }
    Ok(())
}

/// Reads the input of `deck genre add`. A terminal is not read: without redirection
/// the command would wait for typing, so it returns the usage ([`ADD_USAGE`]).
fn read_input(is_terminal: bool, mut stdin: impl Read) -> Result<String> {
    if is_terminal {
        return Err(Usage(ADD_USAGE).into());
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .context("cannot read stdin")?;
    Ok(text)
}

/// `deck genre remove <name>`: removes an own genre. A catalog genre cannot be removed.
pub fn remove(name: &str) -> Result<()> {
    let id = slug(name);
    let path = MyGenres::default_path()?;
    let mut my = MyGenres::load(&path)?;
    let built_in = genres::catalog().iter().any(|g| g.id == id);
    if my.remove(&id)? {
        let back = if built_in {
            " The built-in version is back."
        } else {
            ""
        };
        eprintln!("Removed {name}.{back} Restart Deck to see the change.");
        Ok(())
    } else if built_in {
        bail!("{name} is a built-in genre and cannot be removed")
    } else {
        bail!("there is no own genre named {name}")
    }
}

#[derive(Serialize)]
struct Listed<'a> {
    #[serde(flatten)]
    genre: &'a Genre,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    own: bool,
}

/// `deck genre list`: all genres with their seeds as JSON, own ones with `"own": true`.
pub fn list() -> Result<()> {
    let my = MyGenres::load(&MyGenres::default_path()?)?;
    let all = genres::merge(genres::catalog(), my.genres());
    let listed: Vec<Listed> = all
        .iter()
        .map(|genre| Listed {
            genre,
            own: my.find(&genre.id).is_some(),
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&listed)?);
    Ok(())
}

/// The real Spotify: a librespot session for metadata and stations, the Web API only
/// for search.
struct Live {
    config: Config,
    session: SharedSession,
    radio: radio::Client,
    /// Opened only for the first search.
    catalog: Option<Catalog>,
}

impl Live {
    async fn connect(config: Config) -> Result<Self> {
        let session = SharedSession::new(player::connect().await?);
        Ok(Self {
            config,
            radio: radio::Client::new(session.clone()),
            session,
            catalog: None,
        })
    }
}

impl Spotify for Live {
    async fn search(&mut self, artist: &str, track: Option<&str>) -> Result<Vec<FoundSeed>> {
        if self.catalog.is_none() {
            self.catalog = Some(Catalog::connect(self.session.clone(), &self.config).await?);
        }
        let catalog = self.catalog.as_ref().expect("connected above");
        match track {
            Some(track) => catalog.search_tracks(artist, track).await,
            None => catalog.search_artists(artist).await,
        }
    }

    async fn names(&mut self, uri: &str) -> Result<FoundSeed> {
        let parsed = SpotifyUri::from_uri(uri).context("invalid seed URI")?;
        match parsed {
            SpotifyUri::Artist { .. } => {
                let artist = SpotifyArtist::get(&self.session.get().await?, &parsed).await?;
                Ok(FoundSeed {
                    uri: uri.to_owned(),
                    artists: vec![artist.name.clone()],
                    name: artist.name,
                })
            }
            SpotifyUri::Track { .. } => {
                let track = SpotifyTrack::get(&self.session.get().await?, &parsed).await?;
                Ok(FoundSeed {
                    uri: uri.to_owned(),
                    name: track.name,
                    artists: track.artists.iter().map(|a| a.name.clone()).collect(),
                })
            }
            _ => bail!("seed must be an artist or track URI"),
        }
    }

    async fn station(&mut self, uri: &str) -> Result<Vec<radio::Track>> {
        self.radio.page(uri, &[]).await
    }

    async fn year(&mut self, album_uri: &str) -> Option<u16> {
        self.radio
            .album_for_shelf(album_uri)
            .await
            .ok()
            .and_then(|album| album.year)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that must not be read.
    struct Untouched;

    impl Read for Untouched {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("a terminal must not be read")
        }
    }

    #[test]
    fn terminal_input_returns_add_usage_without_reading() {
        let error = read_input(true, Untouched).unwrap_err();
        let usage = error.downcast_ref::<Usage>().expect("a usage error");
        assert_eq!(usage.0, ADD_USAGE);
    }

    #[test]
    fn redirected_input_is_read() {
        let text = read_input(false, &b"{\"name\": \"x\"}"[..]).unwrap();
        assert_eq!(text, "{\"name\": \"x\"}");
    }

    /// The example in the usage is valid input.
    #[test]
    fn add_usage_example_parses() {
        let start = ADD_USAGE.find("  {").unwrap();
        let end = ADD_USAGE.find("]}").unwrap() + 2;
        let input: Input = serde_json::from_str(&ADD_USAGE[start..end]).unwrap();
        assert_eq!(input.name, "60s rock");
        assert_eq!(input.seeds.len(), 2);
        assert!(input.seeds[0].uri.is_some());
        assert_eq!(input.seeds[1].uri, None);
    }

    const NOW: u64 = 1_800_000_000;
    const TROGGS: &str = "spotify:artist:57xdnSVt4ahJCIXYLieQ25";
    const TROGSS: &str = "spotify:artist:6oc0uG7r137XS4DUc6UQU9";
    const LOVE: &str = "spotify:artist:3Q6OOkfssqoMSTtl11J5Uk";
    const ZOMBIES: &str = "spotify:track:5BATmTqGopeifUzHN2bE0f";
    const QUIET: &str = "spotify:artist:1YqGsKpdixxSVgpfaL2AEQ";

    /// A fake Spotify: known URIs, searches and stations, and a record of the calls.
    #[derive(Default)]
    struct Fake {
        names: Vec<FoundSeed>,
        searchable: Vec<FoundSeed>,
        /// Stations with fewer than 20 tracks.
        short_stations: Vec<String>,
        rate_limited: bool,
        searches: Vec<String>,
        stations: Vec<String>,
    }

    fn found(uri: &str, name: &str, artists: &[&str]) -> FoundSeed {
        FoundSeed {
            uri: uri.into(),
            name: name.into(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn artist(uri: &str, name: &str) -> FoundSeed {
        found(uri, name, &[name])
    }

    fn fake() -> Fake {
        Fake {
            names: vec![
                artist(TROGGS, "The Troggs"),
                artist(TROGSS, "The Trogss"),
                artist(QUIET, "Small Faces"),
                found(
                    ZOMBIES,
                    "She's Not There - Mono",
                    &["The Zombies", "Rod Argent"],
                ),
            ],
            searchable: vec![artist(TROGSS, "The Trogss"), artist(LOVE, "Love")],
            short_stations: vec![QUIET.into()],
            ..Fake::default()
        }
    }

    impl Spotify for Fake {
        async fn search(&mut self, artist: &str, track: Option<&str>) -> Result<Vec<FoundSeed>> {
            self.searches.push(artist.to_owned());
            if self.rate_limited {
                return Err(RateLimited {
                    retry_after: Some(600),
                }
                .into());
            }
            Ok(match track {
                Some(_) => Vec::new(),
                None => self.searchable.clone(),
            })
        }

        async fn names(&mut self, uri: &str) -> Result<FoundSeed> {
            self.names
                .iter()
                .find(|f| f.uri == uri)
                .cloned()
                .context("404")
        }

        async fn station(&mut self, uri: &str) -> Result<Vec<radio::Track>> {
            self.stations.push(uri.to_owned());
            let count = if self.short_stations.iter().any(|s| s == uri) {
                5
            } else {
                50
            };
            Ok((0..count)
                .map(|n| radio::Track {
                    uri: format!("spotify:track:{n:022}"),
                    name: format!("Song {n}"),
                    artist: format!("Band {n}"),
                    artist_uri: format!("spotify:artist:{n:022}"),
                    album: radio::Album {
                        uri: format!("spotify:album:{n:022}"),
                        name: format!("Album {n}"),
                    },
                })
                .collect())
        }

        async fn year(&mut self, album_uri: &str) -> Option<u16> {
            (!album_uri.ends_with('9')).then_some(1966)
        }
    }

    fn seed(artist: &str, track: Option<&str>, uri: Option<&str>) -> SeedInput {
        SeedInput {
            artist: artist.into(),
            track: track.map(str::to_owned),
            uri: uri.map(str::to_owned),
        }
    }

    async fn run(seeds: &[SeedInput], cache: &mut SearchCache, spotify: &mut Fake) -> Checked {
        check(seeds, cache, NOW, Duration::ZERO, spotify)
            .await
            .unwrap()
    }

    fn reasons(checked: &Checked) -> Vec<(String, Rejection)> {
        checked
            .rejected
            .iter()
            .map(|r| (r.artist.clone(), r.reason))
            .collect()
    }

    #[tokio::test]
    async fn uris_are_checked_without_searching() {
        let mut spotify = fake();
        let mut cache = SearchCache::default();
        let checked = run(
            &[
                seed("The Troggs", None, Some(TROGGS)),
                seed("the zombies", Some("She's Not There"), Some(ZOMBIES)),
            ],
            &mut cache,
            &mut spotify,
        )
        .await;
        assert!(checked.rejected.is_empty(), "{:?}", checked.rejected);
        assert_eq!(checked.searched, 0);
        assert!(spotify.searches.is_empty());
        assert_eq!(
            checked.seeds,
            [
                Seed {
                    uri: TROGGS.into(),
                    name: "The Troggs".into()
                },
                Seed {
                    uri: ZOMBIES.into(),
                    name: "The Zombies · She's Not There - Mono".into()
                },
            ]
        );
        let station = &checked.accepted[0].station;
        assert_eq!(station.len(), STATION_SAMPLE);
        assert_eq!(station[0], "Band 1 · Song 1 (1966)");
        // An album whose year could not be fetched is shown without a year.
        assert!(
            station.iter().any(|s| s == "Band 9 · Song 9"),
            "{station:?}"
        );
    }

    #[tokio::test]
    async fn rejections_have_reasons() {
        let mut spotify = fake();
        let mut cache = SearchCache::default();
        let checked = run(
            &[
                seed(
                    "The Troggs",
                    None,
                    Some("spotify:album:57xdnSVt4ahJCIXYLieQ25"),
                ),
                seed("The Troggs", Some("Wild Thing"), Some(TROGGS)),
                seed("The Troggs", None, Some(TROGSS)),
                seed("The Troggs", None, Some(TROGGS)),
                seed("Troggs again", None, Some(TROGGS)),
                seed("Ghost", None, Some("spotify:artist:0000000000000000000000")),
                seed("Small Faces", None, Some(QUIET)),
            ],
            &mut cache,
            &mut spotify,
        )
        .await;
        assert_eq!(
            reasons(&checked),
            [
                ("The Troggs".into(), Rejection::InvalidUri),
                ("The Troggs".into(), Rejection::InvalidUri),
                ("The Troggs".into(), Rejection::NameMismatch),
                ("Troggs again".into(), Rejection::Duplicate),
                ("Ghost".into(), Rejection::NotFound),
                ("Small Faces".into(), Rejection::NoStation),
            ]
        );
        assert_eq!(
            checked.rejected[2].spotify_name.as_deref(),
            Some("The Trogss")
        );
        assert_eq!(checked.seeds.len(), 1);
    }

    #[tokio::test]
    async fn names_are_searched_and_must_match() {
        let mut spotify = fake();
        let mut cache = SearchCache::default();
        let checked = run(
            &[
                seed("Love", None, None),
                // The search only returns an artist with an almost identical name.
                seed("The Troggs", None, None),
                seed("The Zombies", Some("Time of the Season"), None),
                seed("love", None, None),
            ],
            &mut cache,
            &mut spotify,
        )
        .await;
        assert_eq!(
            reasons(&checked),
            [
                ("The Troggs".into(), Rejection::NotFound),
                ("The Zombies".into(), Rejection::NotFound),
                ("love".into(), Rejection::Duplicate),
            ]
        );
        assert_eq!(checked.seeds[0].uri, LOVE);
        assert_eq!(checked.searched, 3);
        assert_eq!(cache.searches_left(), 27);

        // A new round does not search the same names again.
        let again = run(&[seed("Love", None, None)], &mut cache, &mut spotify).await;
        assert_eq!(again.searched, 0);
        assert_eq!(spotify.searches.len(), 3);
    }

    #[tokio::test]
    async fn budget_leaves_names_unchecked_but_uris_work() {
        let mut spotify = fake();
        let mut cache = SearchCache::default();
        for n in 0..crate::curate::SEARCH_BUDGET {
            cache.add_seed(&format!("artist {n}"), None, NOW, Vec::new());
        }
        let checked = run(
            &[
                seed("Love", None, None),
                seed("The Troggs", None, Some(TROGGS)),
            ],
            &mut cache,
            &mut spotify,
        )
        .await;
        assert_eq!(reasons(&checked), [("Love".into(), Rejection::NotChecked)]);
        assert_eq!(checked.seeds.len(), 1);
        assert!(spotify.searches.is_empty());
    }

    #[tokio::test]
    async fn rate_limit_stops_the_run_and_blocks_searches() {
        let mut spotify = Fake {
            rate_limited: true,
            ..fake()
        };
        let mut cache = SearchCache::default();
        let error = check(
            &[
                seed("Love", None, None),
                seed("The Troggs", None, Some(TROGGS)),
            ],
            &mut cache,
            NOW,
            Duration::ZERO,
            &mut spotify,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("Spotify is rate limiting Deck"),
            "{error}"
        );
        assert!(spotify.stations.is_empty());
        assert!(cache.ensure_open(NOW + 599).is_err());
        assert!(cache.ensure_open(NOW + 600).is_ok());
    }

    #[test]
    fn track_names_ignore_versions() {
        assert_eq!(
            track_key("Gimme Some Lovin' - 2015 Remaster"),
            track_key("Gimme Some Lovin'")
        );
        assert_ne!(track_key("Gimme Some Lovin'"), track_key("Gimme Gimme"));
    }

    #[test]
    fn genre_name_and_seed_count() {
        let input = |name: &str, seeds: usize| Input {
            name: name.into(),
            seeds: vec![seed("x", None, None); seeds],
        };
        assert_eq!(
            genre_name(&input(" 60s Rock ", 3)).unwrap(),
            ("60s-rock".into(), "60s rock".into())
        );
        assert!(genre_name(&input("!!", 3)).is_err());
        assert!(genre_name(&input("a", 0)).is_err());
        assert!(genre_name(&input("a", MAX_SEEDS + 1)).is_err());
    }

    #[test]
    fn replaces_own_before_built_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-genres.json");
        let mut my = MyGenres::load(&path).unwrap();
        let catalog = genres::catalog();
        assert_eq!(replaces("lofi", &my, &catalog), Some(Replaces::BuiltIn));
        assert_eq!(replaces("60s-surf", &my, &catalog), None);
        my.put(Genre {
            id: "lofi".into(),
            name: "lofi".into(),
            seeds: vec![Seed {
                uri: TROGGS.into(),
                name: "The Troggs".into(),
            }],
        })
        .unwrap();
        assert_eq!(replaces("lofi", &my, &catalog), Some(Replaces::Own));
    }

    #[test]
    fn input_and_report_json() {
        let input: Input = serde_json::from_str(
            r#"{"name": "60s rock", "seeds": [{"artist": "Love"}, {"artist": "The Zombies", "track": "She's Not There", "uri": "spotify:track:5BATmTqGopeifUzHN2bE0f"}]}"#,
        )
        .unwrap();
        assert_eq!(input.seeds[0], seed("Love", None, None));
        assert_eq!(input.seeds[1].uri.as_deref(), Some(ZOMBIES));

        let report = Report {
            id: "60s-rock".into(),
            name: "60s rock".into(),
            replaces: Some(Replaces::BuiltIn),
            accepted: vec![Accepted {
                artist: "Love".into(),
                track: None,
                uri: LOVE.into(),
                station: vec!["Band 1 · Song 1 (1966)".into()],
            }],
            rejected: vec![Rejected {
                artist: "The Troggs".into(),
                track: None,
                uri: Some(TROGSS.into()),
                reason: Rejection::NameMismatch,
                spotify_name: Some("The Trogss".into()),
            }],
            missing: 2,
            searched: 1,
            searches_left: 29,
            written: false,
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "id": "60s-rock", "name": "60s rock", "replaces": "built-in",
                "accepted": [{"artist": "Love", "uri": LOVE, "station": ["Band 1 · Song 1 (1966)"]}],
                "rejected": [{"artist": "The Troggs", "uri": TROGSS, "reason": "name_mismatch", "spotify_name": "The Trogss"}],
                "missing": 2, "searched": 1, "searches_left": 29, "written": false
            })
        );
    }

    #[test]
    fn listed_own_genres_are_marked() {
        let genre = &genres::catalog()[0];
        let own = serde_json::to_value(Listed { genre, own: true }).unwrap();
        let built_in = serde_json::to_value(Listed { genre, own: false }).unwrap();
        assert_eq!(own["own"], true);
        assert!(built_in.get("own").is_none());
        assert_eq!(built_in["id"], genre.id.as_str());
    }

    #[test]
    fn prompt_mentions_the_commands() {
        for command in [
            "deck genre list",
            "deck genre add --dry-run",
            "deck genre remove",
        ] {
            assert!(PROMPT.contains(command), "{command}");
        }
    }
}
