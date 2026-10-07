//! Album lists alongside the shelf: Curated (`~/.config/deck/curated.json`) and its
//! history (`~/.config/deck/curated-history.json`).
//!
//! `deck curate` writes the whole list and adds the suggestions to the history. Deck
//! removes a rejected album from the list and marks the rejection in the history
//! ([`CuratedFiles::reject`]). The files are saved atomically ([`crate::store`]) like the
//! shelf, and a broken file is never overwritten: loading returns an error and nothing
//! is written.

use std::path::{Path, PathBuf};

use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{
    catalog::Album,
    config,
    store::{read_json, save_json},
};

/// An album list to show: name, subtitle and albums. `stale`: the list is more than a
/// week old, i.e. the weekly update has been missed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumList {
    pub name: String,
    pub subtitle: String,
    pub stale: bool,
    pub albums: Vec<ListAlbum>,
}

/// "Now" in tests: 2026-10-06 09:00 UTC, a day after the list of round 2026-W41.
#[cfg(test)]
pub const TEST_NOW: Timestamp = Timestamp::constant(1_791_277_200, 0);

/// How old a list may be before it is shown as stale.
const STALE_AFTER: SignedDuration = SignedDuration::from_hours(7 * 24);

/// An album on a list and an optional reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListAlbum {
    pub album: Album,
    pub reason: Option<String>,
}

/// A curation round: `round` is an ISO week (`2026-W41`), `created` an RFC 3339 time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Curated {
    pub round: String,
    pub created: String,
    pub albums: Vec<ListAlbum>,
}

impl Curated {
    /// The list for the view: "Curated", with the subtitle "20 albums · week 41". For a
    /// list more than a week old the subtitle also has the age:
    /// "20 albums · week 40 · 9 days old". An unparsable `created` does not make the
    /// list stale.
    pub fn to_list(&self, now: Timestamp) -> AlbumList {
        let count = match self.albums.len() {
            1 => "1 album".to_owned(),
            n => format!("{n} albums"),
        };
        let mut subtitle = match self.round.split_once("-W") {
            Some((_, week)) => format!("{count} · week {}", week.trim_start_matches('0')),
            None => count,
        };
        let age = self
            .created
            .parse::<Timestamp>()
            .ok()
            .map(|created| now.duration_since(created))
            .filter(|age| *age > STALE_AFTER);
        if let Some(age) = age {
            subtitle.push_str(&format!(" · {} days old", age.as_hours() / 24));
        }
        AlbumList {
            name: "Curated".to_owned(),
            subtitle,
            stale: age.is_some(),
            albums: self.albums.clone(),
        }
    }
}

/// A history row: an album suggested once, and whether it was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub uri: String,
    pub name: String,
    pub artist: String,
    pub round: String,
    pub rejected: bool,
}

/// The files of the Curated list and its history.
#[derive(Debug, Clone)]
pub struct CuratedFiles {
    curated: PathBuf,
    history: PathBuf,
}

impl CuratedFiles {
    /// The files in the directory `~/.config/deck/`.
    pub fn default_paths() -> Result<Self> {
        Ok(Self::in_dir(&config::config_dir()?))
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self {
            curated: dir.join("curated.json"),
            history: dir.join("curated-history.json"),
        }
    }

    /// Reads the current list. A missing file is `None`.
    pub fn load(&self) -> Result<Option<Curated>> {
        let file: Option<CuratedFile> = read_json(&self.curated, "curated list")?;
        Ok(file.map(Curated::from))
    }

    /// Reads the history. A missing file is an empty history.
    pub fn history(&self) -> Result<Vec<HistoryEntry>> {
        Ok(read_json(&self.history, "curated history")?.unwrap_or_default())
    }

    /// Replaces the list with a new round and adds its albums to the history.
    /// The history is written first: if saving the list fails, the albums are
    /// still not suggested again.
    pub fn publish(&self, curated: &Curated) -> Result<()> {
        let mut history = self.history()?;
        history.extend(curated.albums.iter().map(|a| HistoryEntry {
            uri: a.album.uri.clone(),
            name: a.album.name.clone(),
            artist: a.album.artist.clone(),
            round: curated.round.clone(),
            rejected: false,
        }));
        save_json(&self.history, &history)?;
        save_json(&self.curated, &CuratedFile::from(curated))
    }

    /// Rejects an album: removes it from the list and marks it rejected in the history.
    /// Returns the updated list, or `None` if there is no list. The history is
    /// written first, so a failed save is safe to repeat.
    pub fn reject(&self, uri: &str) -> Result<Option<Curated>> {
        let Some(mut curated) = self.load()? else {
            return Ok(None);
        };
        let mut history = self.history()?;
        let Some(index) = curated.albums.iter().position(|a| a.album.uri == uri) else {
            return Ok(Some(curated));
        };
        let removed = curated.albums.remove(index);

        let mut marked = false;
        for entry in history.iter_mut().filter(|e| e.uri == uri) {
            entry.rejected = true;
            marked = true;
        }
        if !marked {
            history.push(HistoryEntry {
                uri: removed.album.uri.clone(),
                name: removed.album.name.clone(),
                artist: removed.album.artist.clone(),
                round: curated.round.clone(),
                rejected: true,
            });
        }
        save_json(&self.history, &history)?;
        save_json(&self.curated, &CuratedFile::from(&curated))?;
        Ok(Some(curated))
    }
}

/// Whether two albums are the same when normalised ([`normalize`]): same artist and name.
pub fn same_album(artist_a: &str, album_a: &str, artist_b: &str, album_b: &str) -> bool {
    normalize(artist_a) == normalize(artist_b) && normalize(album_a) == normalize(album_b)
}

/// A name for comparison: lowercase, accents removed (ä → a too), & → and,
/// punctuation removed and bracketed extras skipped ("OK Computer (Remastered)"
/// = "ok computer"). A name made only of brackets keeps what is inside them.
pub fn normalize(name: &str) -> String {
    let without_extras = strip_brackets(name);
    let source = if without_extras.trim().is_empty() {
        name
    } else {
        &without_extras
    };
    let mut out = String::with_capacity(source.len());
    for c in source.nfd().filter(|&c| !is_combining_mark(c)) {
        match c {
            '&' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            c if c.is_whitespace() || c == '-' || c == '/' => out.push(' '),
            // Other punctuation (periods, apostrophes…) is removed without a space.
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A track name for comparison: an extra " - …" is skipped ("Gimme Some Lovin' -
/// 2015 Remaster" = "Gimme Some Lovin'"), and the rest is normalised ([`normalize`]).
pub fn track_key(name: &str) -> String {
    normalize(name.split(" - ").next().unwrap_or(name))
}

/// Removes the parts inside ( ) and [ ].
fn strip_brackets(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0usize;
    for c in name.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

#[derive(Serialize, Deserialize)]
struct CuratedFile {
    round: String,
    created: String,
    albums: Vec<CuratedEntry>,
}

#[derive(Serialize, Deserialize)]
struct CuratedEntry {
    uri: String,
    name: String,
    artist: String,
    artist_id: String,
    year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl From<CuratedFile> for Curated {
    fn from(file: CuratedFile) -> Self {
        Self {
            round: file.round,
            created: file.created,
            albums: file
                .albums
                .into_iter()
                .map(|e| ListAlbum {
                    album: Album {
                        id: e.uri.rsplit(':').next().unwrap_or_default().to_owned(),
                        uri: e.uri,
                        name: e.name,
                        artist: e.artist,
                        artist_id: e.artist_id,
                        year: e.year,
                    },
                    reason: e.reason,
                })
                .collect(),
        }
    }
}

impl From<&Curated> for CuratedFile {
    fn from(c: &Curated) -> Self {
        Self {
            round: c.round.clone(),
            created: c.created.clone(),
            albums: c
                .albums
                .iter()
                .map(|a| CuratedEntry {
                    uri: a.album.uri.clone(),
                    name: a.album.name.clone(),
                    artist: a.album.artist.clone(),
                    artist_id: a.album.artist_id.clone(),
                    year: a.album.year,
                    reason: a.reason.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn entry(id: &str, name: &str, artist: &str, reason: &str) -> ListAlbum {
        ListAlbum {
            album: Album {
                id: id.to_owned(),
                uri: format!("spotify:album:{id}"),
                name: name.to_owned(),
                artist: artist.to_owned(),
                artist_id: format!("id-{}", artist.to_lowercase().replace(' ', "-")),
                year: Some(1974),
            },
            reason: Some(reason.to_owned()),
        }
    }

    fn round(albums: Vec<ListAlbum>) -> Curated {
        Curated {
            round: "2026-W41".to_owned(),
            created: "2026-10-05T07:00:00+03:00".to_owned(),
            albums,
        }
    }

    #[test]
    fn missing_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        assert_eq!(files.load().unwrap(), None);
        assert!(files.history().unwrap().is_empty());
        assert_eq!(files.reject("spotify:album:x").unwrap(), None);
        assert!(!dir.path().join("curated-history.json").exists());
    }

    #[test]
    fn publish_writes_spec_format_and_appends_history() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(&dir.path().join("deck"));
        let first = round(vec![entry("a1", "No Other", "Gene Clark", "Cosmic.")]);
        files.publish(&first).unwrap();

        let json: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join("deck/curated.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "round": "2026-W41",
                "created": "2026-10-05T07:00:00+03:00",
                "albums": [{
                    "uri": "spotify:album:a1",
                    "name": "No Other",
                    "artist": "Gene Clark",
                    "artist_id": "id-gene-clark",
                    "year": 1974,
                    "reason": "Cosmic.",
                }],
            })
        );
        assert_eq!(files.load().unwrap(), Some(first));

        let mut second = round(vec![entry("b2", "#1 Record", "Big Star", "Power pop.")]);
        second.round = "2026-W42".to_owned();
        files.publish(&second).unwrap();
        assert_eq!(files.load().unwrap(), Some(second));
        assert_eq!(
            files.history().unwrap(),
            vec![
                HistoryEntry {
                    uri: "spotify:album:a1".to_owned(),
                    name: "No Other".to_owned(),
                    artist: "Gene Clark".to_owned(),
                    round: "2026-W41".to_owned(),
                    rejected: false,
                },
                HistoryEntry {
                    uri: "spotify:album:b2".to_owned(),
                    name: "#1 Record".to_owned(),
                    artist: "Big Star".to_owned(),
                    round: "2026-W42".to_owned(),
                    rejected: false,
                },
            ]
        );
        let mut names: Vec<String> = fs::read_dir(dir.path().join("deck"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["curated-history.json", "curated.json"]);
    }

    #[test]
    fn reject_removes_from_list_and_marks_history() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        files
            .publish(&round(vec![
                entry("a1", "No Other", "Gene Clark", "x"),
                entry("b2", "#1 Record", "Big Star", "y"),
            ]))
            .unwrap();

        let updated = files.reject("spotify:album:a1").unwrap().unwrap();
        assert_eq!(updated.albums.len(), 1);
        assert_eq!(files.load().unwrap(), Some(updated));
        let history = files.history().unwrap();
        assert!(history[0].rejected);
        assert!(!history[1].rejected);

        // An unknown URI changes nothing.
        let same = files.reject("spotify:album:nope").unwrap().unwrap();
        assert_eq!(same.albums.len(), 1);
        assert_eq!(files.history().unwrap(), history);
    }

    #[test]
    fn reject_adds_missing_history_entry() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        save_json(
            &dir.path().join("curated.json"),
            &CuratedFile::from(&round(vec![entry("a1", "No Other", "Gene Clark", "x")])),
        )
        .unwrap();

        files.reject("spotify:album:a1").unwrap();
        let history = files.history().unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].rejected);
        assert_eq!(history[0].round, "2026-W41");
    }

    #[test]
    fn broken_files_are_errors_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        let broken = "{\"round\": ";
        fs::write(dir.path().join("curated.json"), broken).unwrap();
        let err = files.load().unwrap_err();
        assert!(format!("{err:#}").contains("is broken"), "{err:#}");
        assert!(files.reject("spotify:album:a1").is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("curated.json")).unwrap(),
            broken
        );

        // A broken history blocks both publishing and rejecting.
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        files
            .publish(&round(vec![entry("a1", "No Other", "Gene Clark", "x")]))
            .unwrap();
        fs::write(dir.path().join("curated-history.json"), broken).unwrap();
        assert!(files.history().is_err());
        assert!(files.reject("spotify:album:a1").is_err());
        assert!(files.publish(&round(Vec::new())).is_err());
        assert_eq!(files.load().unwrap().unwrap().albums.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("curated-history.json")).unwrap(),
            broken
        );
    }

    #[test]
    fn reason_is_optional_in_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("curated.json"),
            r#"{"round":"2026-W41","created":"x","albums":[
                {"uri":"spotify:album:a1","name":"No Other","artist":"Gene Clark",
                 "artist_id":"g","year":null}]}"#,
        )
        .unwrap();
        let curated = CuratedFiles::in_dir(dir.path()).load().unwrap().unwrap();
        assert_eq!(curated.albums[0].reason, None);
        assert_eq!(curated.albums[0].album.id, "a1");
    }

    #[test]
    fn list_title_and_subtitle() {
        let list = round(vec![entry("a1", "No Other", "Gene Clark", "x")]).to_list(TEST_NOW);
        assert_eq!(list.name, "Curated");
        assert_eq!(list.subtitle, "1 album · week 41");

        let mut curated = round(vec![
            entry("a1", "No Other", "Gene Clark", "x"),
            entry("b2", "#1 Record", "Big Star", "y"),
        ]);
        curated.round = "2027-W03".to_owned();
        assert_eq!(curated.to_list(TEST_NOW).subtitle, "2 albums · week 3");
        assert_eq!(curated.to_list(TEST_NOW).albums, curated.albums);
        assert!(!curated.to_list(TEST_NOW).stale);
    }

    #[test]
    fn list_older_than_a_week_is_stale_and_shows_its_age() {
        // The list was written at 2026-10-05 07:00 +03:00 = 04:00 UTC.
        let curated = round(vec![entry("a1", "No Other", "Gene Clark", "x")]);
        let at = |rfc3339: &str| curated.to_list(rfc3339.parse().unwrap());

        let week = at("2026-10-12T04:00:00Z");
        assert!(!week.stale);
        assert_eq!(week.subtitle, "1 album · week 41");

        let over_week = at("2026-10-12T04:00:01Z");
        assert!(over_week.stale);
        assert_eq!(over_week.subtitle, "1 album · week 41 · 7 days old");

        let nine_days = at("2026-10-14T09:00:00Z");
        assert_eq!(nine_days.subtitle, "1 album · week 41 · 9 days old");

        // An unparsable time does not make the list stale.
        let mut unknown = curated.clone();
        unknown.created = "x".to_owned();
        let list = unknown.to_list("2026-10-14T09:00:00Z".parse().unwrap());
        assert!(!list.stale);
        assert_eq!(list.subtitle, "1 album · week 41");
    }

    #[test]
    fn normalize_rules() {
        assert_eq!(normalize("OK Computer"), "ok computer");
        assert_eq!(normalize("Abbey Road (Remastered 2009)"), "abbey road");
        assert_eq!(normalize("Pet Sounds [Deluxe Edition]"), "pet sounds");
        assert_eq!(
            normalize("Sgt. Pepper's Lonely Hearts Club Band"),
            "sgt peppers lonely hearts club band"
        );
        assert_eq!(normalize("Hüsker Dü"), "husker du");
        assert_eq!(normalize("Sigur Rós"), "sigur ros");
        assert_eq!(normalize("Simon & Garfunkel"), "simon and garfunkel");
        assert_eq!(normalize("AC/DC"), normalize("AC DC"));
        assert_eq!(normalize("  #1   Record "), "1 record");
        assert_eq!(normalize("Älymystö"), "alymysto");
        // Only brackets: their content stays in the comparison.
        assert_eq!(normalize("(What's the Story)"), "whats the story");
    }

    #[test]
    fn same_album_compares_both_names() {
        assert!(same_album(
            "Radiohead",
            "OK Computer",
            "radiohead",
            "OK Computer (Collector's Edition)"
        ));
        assert!(same_album(
            "Gene Clark",
            "No Other",
            "Gene Clark",
            "No Other"
        ));
        assert!(!same_album(
            "Gene Clark",
            "No Other",
            "Gene Clark",
            "White Light"
        ));
        assert!(!same_album(
            "Radiohead",
            "OK Computer",
            "Radiohead",
            "OK Computer OKNOTOK 1997 2017"
        ));
        assert!(!same_album(
            "The Byrds",
            "No Other",
            "Gene Clark",
            "No Other"
        ));
    }
}
