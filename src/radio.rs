//! Spotify radio stations and their continuation. Choosing the seed, the track list
//! and playback belong to the app (`app`): this module only fetches a page and filters
//! out the new tracks.
//!
//! Spotify closes Deck's session after a few minutes (see `player`), so fetching takes
//! the session from [`SharedSession`], which signs in again when needed.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};
use librespot_core::{SpotifyUri, spotify_id::SpotifyId};
use librespot_metadata::{Album as SpotifyAlbum, Metadata};
use serde::{Deserialize, Serialize};

use crate::{catalog, player::SharedSession};

const PAGE_SIZE: usize = 50;
const PREVIOUS_LIMIT: usize = 250;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub uri: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub artist_uri: String,
    pub album: Album,
}

/// Radio requests on the shared session.
#[derive(Clone)]
pub struct Client {
    session: SharedSession,
}

impl Client {
    pub fn new(session: SharedSession) -> Self {
        Self { session }
    }

    /// Fetches a page from the station of the seed (a track or artist URI). `previous`
    /// is the tracks (URIs) already received from the station, in arrival order: at most
    /// the latest 250 of them are sent in `prev_tracks`, so the station continues.
    pub async fn page(&self, seed: &str, previous: &[String]) -> Result<Vec<Track>> {
        match SpotifyUri::from_uri(seed).context("invalid radio seed URI")? {
            SpotifyUri::Track { .. } | SpotifyUri::Artist { .. } => {}
            _ => bail!("radio seed must be a track or artist URI"),
        }
        let previous = previous_tracks(previous)?;
        let bytes = self
            .session
            .get()
            .await?
            .spclient()
            .get_apollo_station("stations", seed, Some(PAGE_SIZE), previous, true)
            .await
            .context("could not load Spotify radio station")?;
        parse_page(&bytes)
    }

    /// Fetches the album's artist and year for adding it to the shelf. The track's
    /// artist may differ from the album's artist, for example on a compilation.
    pub async fn album_for_shelf(&self, album_uri: &str) -> Result<catalog::Album> {
        let uri = SpotifyUri::from_uri(album_uri).context("invalid radio album URI")?;
        if !matches!(uri, SpotifyUri::Album { .. }) {
            bail!("radio album URI must refer to an album");
        }
        let session = self.session.get().await?;
        let album = SpotifyAlbum::get(&session, &uri)
            .await
            .context("could not load Spotify album")?;
        let (artist, artist_id) = match album.artists.first() {
            Some(artist) => (artist.name.clone(), artist.id.to_id()?),
            None => (String::new(), String::new()),
        };
        Ok(catalog::Album {
            id: uri.to_id()?,
            uri: album_uri.to_owned(),
            name: album.name,
            artist,
            artist_id,
            year: u16::try_from(album.date.year()).ok(),
        })
    }
}

/// The page's tracks that are not yet in `existing`, in the station's order.
/// Duplicates within the page are skipped too.
pub fn new_tracks(page: Vec<Track>, existing: &[Track]) -> Vec<Track> {
    let mut seen: HashSet<String> = existing.iter().map(|t| t.uri.clone()).collect();
    page.into_iter()
        .filter(|track| seen.insert(track.uri.clone()))
        .collect()
}

fn previous_tracks(tracks: &[String]) -> Result<Vec<SpotifyId>> {
    tracks
        .iter()
        .rev()
        .take(PREVIOUS_LIMIT)
        .map(|uri| match SpotifyUri::from_uri(uri)? {
            SpotifyUri::Track { id } => Ok(id),
            _ => Err(anyhow!("radio contains a non-track URI: {uri}")),
        })
        .collect()
}

#[derive(Deserialize)]
struct StationPage {
    tracks: Vec<StationTrack>,
}

#[derive(Deserialize)]
struct StationTrack {
    uri: String,
    artist_uri: String,
    album_uri: String,
    metadata: StationMetadata,
}

#[derive(Deserialize)]
struct StationMetadata {
    title: String,
    artist_name: String,
    album_title: String,
}

fn parse_page(bytes: &[u8]) -> Result<Vec<Track>> {
    let page: StationPage =
        serde_json::from_slice(bytes).context("invalid Spotify radio response")?;
    page.tracks
        .into_iter()
        .map(|t| {
            if !matches!(SpotifyUri::from_uri(&t.uri)?, SpotifyUri::Track { .. }) {
                bail!("radio response contains a non-track URI: {}", t.uri);
            }
            if !matches!(
                SpotifyUri::from_uri(&t.album_uri)?,
                SpotifyUri::Album { .. }
            ) {
                bail!("radio response contains a non-album URI: {}", t.album_uri);
            }
            Ok(Track {
                uri: t.uri,
                name: t.metadata.title,
                artist: t.metadata.artist_name,
                artist_uri: t.artist_uri,
                album: Album {
                    uri: t.album_uri,
                    name: t.metadata.album_title,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &[u8] = include_bytes!("../tests/fixtures/apollo-lofi-station.json");
    const LOFI: &str = "spotify:track:0HSwIZSDeOQX8Cu9pCGkjS";
    const SYNTHWAVE: &str = "spotify:track:6mB9A9YLbY4jxpKX5EYAnT";

    #[test]
    fn parses_saved_apollo_response_with_album_data() {
        let tracks = parse_page(RESPONSE).unwrap();
        assert_eq!(tracks.len(), 50);
        assert_eq!(
            tracks[0],
            Track {
                uri: LOFI.into(),
                name: "Affection".into(),
                artist: "Jinsang".into(),
                artist_uri: "spotify:artist:5FsfZj0Mp6YwEWytuJUcWt".into(),
                album: Album {
                    uri: "spotify:album:5dFInNoXmAlvU2oQ7LWK2h".into(),
                    name: "Life".into(),
                },
            }
        );
    }

    #[test]
    fn continuation_skips_tracks_already_in_the_radio() {
        let page = parse_page(RESPONSE).unwrap();
        let existing = vec![page[0].clone(), page[7].clone()];
        let added = new_tracks(page.clone(), &existing);
        assert_eq!(added.len(), 48);
        assert_eq!(added[0], page[1]);
        assert!(!added.contains(&page[7]));

        // The same page again adds nothing: the station repeats itself.
        let all: Vec<Track> = existing.into_iter().chain(added).collect();
        assert!(new_tracks(page, &all).is_empty());
    }

    #[test]
    fn duplicates_within_a_page_are_skipped() {
        let first = parse_page(RESPONSE).unwrap().remove(0);
        let added = new_tracks(vec![first.clone(), first.clone()], &[]);
        assert_eq!(added, [first]);
    }

    #[test]
    fn previous_tracks_uses_last_250_in_reverse_order() {
        let uris: Vec<String> = (0..300).map(|n| format!("spotify:track:{n:022}")).collect();
        let previous = previous_tracks(&uris).unwrap();
        assert_eq!(previous.len(), 250);
        assert_eq!(previous[0].to_base62().unwrap(), "0000000000000000000299");
        assert_eq!(previous[249].to_base62().unwrap(), "0000000000000000000050");
    }

    #[test]
    fn malformed_response_is_an_error() {
        assert!(parse_page(br#"{"tracks":[{"uri":"bad"}]}"#).is_err());
        assert!(parse_page(b"{").is_err());
    }

    /// Run by hand: `cargo test radio::tests::live_station -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "requires Spotify credentials and network"]
    async fn live_station() -> Result<()> {
        let client = Client::new(SharedSession::new(crate::player::connect().await?));
        for (genre, seed) in [("lofi", LOFI), ("synthwave", SYNTHWAVE)] {
            let mut tracks = client.page(seed, &[]).await?;
            println!("{genre}: first page {} tracks", tracks.len());
            while tracks.len() < 100 {
                let previous: Vec<String> = tracks.iter().map(|t| t.uri.clone()).collect();
                let added = new_tracks(client.page(seed, &previous).await?, &tracks);
                println!(
                    "{genre}: continuation +{}, total {}",
                    added.len(),
                    tracks.len() + added.len()
                );
                if added.is_empty() {
                    break;
                }
                tracks.extend(added);
            }
            assert!(tracks.len() >= 100, "{genre} yielded fewer than 100 tracks");
            let album = client.album_for_shelf(&tracks[0].album.uri).await?;
            println!(
                "{genre}: album {} · {} ({:?}) {}",
                album.artist, album.name, album.year, album.uri
            );
            assert!(album.year.is_some());
        }
        let artist = client
            .page("spotify:artist:5FsfZj0Mp6YwEWytuJUcWt", &[])
            .await?;
        println!("Jinsang artist seed: {} tracks", artist.len());
        assert_eq!(artist.len(), 50);
        Ok(())
    }
}
