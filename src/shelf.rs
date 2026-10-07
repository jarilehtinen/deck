//! The album shelf (`~/.config/deck/shelf.json`): Deck's own hand-picked album list.
//!
//! The file is a JSON list of `{uri, name, artist, artist_id, year}`. Every change
//! rereads the file first, so that edits made by hand while Deck is open are kept,
//! and is saved atomically right away ([`crate::store`]). A broken file is never
//! overwritten: `load` and every change return an error, and if the file is broken at
//! startup the caller uses `Shelf::unavailable()` for the session, which is empty and
//! read-only.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{
    catalog::{Album, Artist},
    config,
    store::{read_json, write_atomic},
};

#[derive(Debug)]
pub struct Shelf {
    /// `None` when the shelf is read-only.
    path: Option<PathBuf>,
    albums: Vec<Album>,
}

impl Shelf {
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::config_dir()?.join("shelf.json"))
    }

    /// Reads the shelf. A missing file means an empty shelf.
    pub fn load(path: &Path) -> Result<Self> {
        let albums = read_json::<Vec<Entry>>(path, "shelf")?
            .unwrap_or_default()
            .into_iter()
            .map(Album::from)
            .collect();
        Ok(Self {
            path: Some(path.to_owned()),
            albums,
        })
    }

    /// An empty shelf that cannot be saved: used when `load` failed.
    pub fn unavailable() -> Self {
        Self {
            path: None,
            albums: Vec::new(),
        }
    }

    pub fn contains(&self, uri: &str) -> bool {
        self.albums.iter().any(|a| a.uri == uri)
    }

    /// Adds the album and saves. Returns `false` if the album was already on the shelf.
    pub fn add(&mut self, album: Album) -> Result<bool> {
        self.reload()?;
        if self.contains(&album.uri) {
            return Ok(false);
        }
        self.albums.push(album);
        if let Err(e) = self.save() {
            self.albums.pop();
            return Err(e);
        }
        Ok(true)
    }

    /// Removes the album and saves. Returns `false` if the album was not on the shelf.
    pub fn remove(&mut self, uri: &str) -> Result<bool> {
        self.reload()?;
        let Some(index) = self.albums.iter().position(|a| a.uri == uri) else {
            return Ok(false);
        };
        let album = self.albums.remove(index);
        if let Err(e) = self.save() {
            self.albums.insert(index, album);
            return Err(e);
        }
        Ok(true)
    }

    /// The shelf's albums in the order they were saved.
    pub fn albums(&self) -> &[Album] {
        &self.albums
    }

    /// Rereads the file before a change. If it has become broken, the error is
    /// returned and the albums in memory stay as they were.
    fn reload(&mut self) -> Result<()> {
        if let Some(path) = &self.path {
            self.albums = Self::load(path)?.albums;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            bail!("the shelf is read-only because the shelf file could not be read");
        };
        let entries: Vec<Entry> = self.albums.iter().map(Entry::from).collect();
        let mut json = serde_json::to_string_pretty(&entries)?;
        json.push('\n');
        write_atomic(path, json.as_bytes())
            .with_context(|| format!("cannot save the shelf to {}", path.display()))
    }
}

/// The albums' artists in alphabetical order ([`sort_key`]), each once.
pub fn artists_of(albums: &[Album]) -> Vec<Artist> {
    let mut artists: Vec<Artist> = Vec::new();
    for album in albums {
        if !artists.iter().any(|a| a.id == album.artist_id) {
            artists.push(Artist {
                id: album.artist_id.clone(),
                name: album.artist.clone(),
            });
        }
    }
    artists.sort_by_cached_key(|a| (sort_key(&a.name), a.name.to_lowercase(), a.id.clone()));
    artists
}

/// The artist's albums from the list, sorted by year (see [`sort_by_year`]).
pub fn albums_of(albums: &[Album], artist_id: &str) -> Vec<Album> {
    let mut albums: Vec<Album> = albums
        .iter()
        .filter(|a| a.artist_id == artist_id)
        .cloned()
        .collect();
    sort_by_year(&mut albums);
    albums
}

/// Oldest first, albums without a year last, albums from the same year by name
/// ([`sort_key`]).
pub fn sort_by_year(albums: &mut [Album]) {
    albums.sort_by_cached_key(|a| (a.year.is_none(), a.year, sort_key(&a.name)));
}

/// Sort key for a name according to the user's locale ([`nordic_collation`], see
/// [`sort_key_with`]).
fn sort_key(name: &str) -> String {
    sort_key_with(name, nordic_collation())
}

/// Whether å, ä and ö sort as letters of their own after z. Decided once from
/// the environment variables ([`is_nordic_locale`]).
fn nordic_collation() -> bool {
    static NORDIC: OnceLock<bool> = OnceLock::new();
    *NORDIC.get_or_init(|| is_nordic_locale(|name| std::env::var(name).ok()))
}

/// Whether the collation locale is Finnish or Swedish. As in POSIX, the locale is the
/// first non-empty one of `LC_ALL`, `LC_COLLATE` and `LANG`. `var` reads a variable, so
/// the decision can be tested without touching the environment.
fn is_nordic_locale(var: impl Fn(&str) -> Option<String>) -> bool {
    ["LC_ALL", "LC_COLLATE", "LANG"]
        .into_iter()
        .filter_map(var)
        .find(|value| !value.is_empty())
        .is_some_and(|locale| locale.starts_with("fi") || locale.starts_with("sv"))
}

/// Sort key for a name; the displayed name stays as it is. A leading "The " is skipped,
/// case does not matter and accents are removed (é → e, ü → u, ñ → n).
/// `nordic`: as in Finnish and Swedish, å, ä and ö are letters of their own after z
/// (æ = ä, ø = ö). Otherwise they too fold to their base letter (å, ä → a,
/// ö, ø → o, æ → ae).
fn sort_key_with(name: &str, nordic: bool) -> String {
    // NFC first, so that an ä stored decomposed (a + ¨) is recognised too.
    let lower = name.trim().nfc().collect::<String>().to_lowercase();
    let lower = match lower.strip_prefix("the ") {
        Some(rest) if !rest.trim().is_empty() => rest.trim_start(),
        _ => &lower,
    };
    let mut key = String::with_capacity(lower.len());
    for c in lower.chars() {
        // Characters from a Private Use plane sort after everything else.
        match c {
            'å' if nordic => key.push('\u{F0001}'),
            'ä' | 'æ' if nordic => key.push('\u{F0002}'),
            'ö' | 'ø' if nordic => key.push('\u{F0003}'),
            'æ' => key.push_str("ae"),
            'ø' => key.push('o'),
            _ => key.extend(c.nfd().filter(|&c| !is_combining_mark(c))),
        }
    }
    key
}

/// An album in the shelf file. The album id is the last part of the Spotify URI.
#[derive(Serialize, Deserialize)]
struct Entry {
    uri: String,
    name: String,
    artist: String,
    artist_id: String,
    year: Option<u16>,
}

impl From<&Album> for Entry {
    fn from(a: &Album) -> Self {
        Self {
            uri: a.uri.clone(),
            name: a.name.clone(),
            artist: a.artist.clone(),
            artist_id: a.artist_id.clone(),
            year: a.year,
        }
    }
}

impl From<Entry> for Album {
    fn from(e: Entry) -> Self {
        Self {
            id: e.uri.rsplit(':').next().unwrap_or_default().to_owned(),
            uri: e.uri,
            name: e.name,
            artist: e.artist,
            artist_id: e.artist_id,
            year: e.year,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn album(id: &str, name: &str, artist: &str, year: Option<u16>) -> Album {
        Album {
            id: id.to_owned(),
            uri: format!("spotify:album:{id}"),
            name: name.to_owned(),
            artist: artist.to_owned(),
            artist_id: format!("id-{}", artist.to_lowercase().replace(' ', "-")),
            year,
        }
    }

    fn names(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|a| a.name.as_str()).collect()
    }

    fn shelf_in(dir: &tempfile::TempDir) -> (PathBuf, Shelf) {
        let path = dir.path().join("deck/shelf.json");
        let shelf = Shelf::load(&path).unwrap();
        (path, shelf)
    }

    #[test]
    fn missing_file_is_empty_shelf() {
        let dir = tempfile::tempdir().unwrap();
        let (path, shelf) = shelf_in(&dir);
        assert!(artists_of(shelf.albums()).is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn add_and_remove_survive_reload() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        let colour = album(
            "c0l0ur",
            "The Colour And The Shape",
            "Foo Fighters",
            Some(1997),
        );
        let wasting = album("w4st1ng", "Wasting Light", "Foo Fighters", Some(2011));

        assert!(shelf.add(colour.clone()).unwrap());
        assert!(shelf.add(wasting.clone()).unwrap());

        let mut reloaded = Shelf::load(&path).unwrap();
        assert_eq!(
            albums_of(reloaded.albums(), &colour.artist_id),
            vec![colour.clone(), wasting]
        );

        assert!(reloaded.remove(&colour.uri).unwrap());
        let reloaded = Shelf::load(&path).unwrap();
        assert_eq!(
            names(&albums_of(reloaded.albums(), &colour.artist_id)),
            ["Wasting Light"]
        );
        assert!(!reloaded.contains(&colour.uri));
    }

    #[test]
    fn file_has_spec_fields() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        shelf
            .add(album("abc", "Nevermind", "Nirvana", Some(1991)))
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!([{
                "uri": "spotify:album:abc",
                "name": "Nevermind",
                "artist": "Nirvana",
                "artist_id": "id-nirvana",
                "year": 1991,
            }])
        );
        let files: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["shelf.json"]);
    }

    #[test]
    fn adding_twice_keeps_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        let a = album("abc", "Nevermind", "Nirvana", Some(1991));
        assert!(shelf.add(a.clone()).unwrap());
        assert!(!shelf.add(a.clone()).unwrap());
        assert_eq!(
            albums_of(Shelf::load(&path).unwrap().albums(), &a.artist_id).len(),
            1
        );
    }

    #[test]
    fn removing_missing_album_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        assert!(!shelf.remove("spotify:album:nope").unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn artists_alphabetical_and_unique() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut shelf) = shelf_in(&dir);
        shelf
            .add(album("1", "In Utero", "Nirvana", Some(1993)))
            .unwrap();
        shelf
            .add(album("2", "Ok Computer", "Radiohead", Some(1997)))
            .unwrap();
        shelf
            .add(album("3", "Nevermind", "Nirvana", Some(1991)))
            .unwrap();
        shelf.add(album("4", "Post", "björk", Some(1995))).unwrap();
        shelf
            .add(album("5", "Wasting Light", "Foo Fighters", Some(2011)))
            .unwrap();

        let artists: Vec<String> = artists_of(shelf.albums())
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(artists, ["björk", "Foo Fighters", "Nirvana", "Radiohead"]);
    }

    #[test]
    fn artists_sorted_without_the_and_accents() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut shelf) = shelf_in(&dir);
        let given = ["The Beatles", "Édith Piaf", "abba", "Zen Café", "Motörhead"];
        for (i, artist) in given.iter().enumerate() {
            shelf
                .add(album(&i.to_string(), "Album", artist, Some(2000)))
                .unwrap();
        }

        let artists: Vec<String> = artists_of(shelf.albums())
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(
            artists,
            ["abba", "The Beatles", "Édith Piaf", "Motörhead", "Zen Café"]
        );
    }

    /// The names sorted by the `sort_key_with` key.
    fn sorted(names: &[&'static str], nordic: bool) -> Vec<&'static str> {
        let mut names = names.to_vec();
        names.sort_by_cached_key(|name| sort_key_with(name, nordic));
        names
    }

    #[test]
    fn sort_key_rules() {
        for nordic in [false, true] {
            let key = |name| sort_key_with(name, nordic);
            assert_eq!(key("The Beatles"), "beatles");
            assert_eq!(key("THE  Cure"), "cure");
            // "The" alone, or a word starting with "The", is not an article.
            assert_eq!(key("The"), "the");
            assert_eq!(key("Them"), "them");
            assert_eq!(key("Édith Piaf"), "edith piaf");
            assert_eq!(key("Mötley Crüe"), key("Mötley Crue"));
            assert_eq!(key("Señor Coconut"), "senor coconut");
            assert_eq!(key("Sigur Rós"), "sigur ros");
            assert_eq!(key("Røyksopp"), key("Röyksopp"));
            // An ä written decomposed (a + ¨) is the same letter too.
            assert_eq!(key("A\u{308}ly"), key("Äly"));
        }
    }

    #[test]
    fn nordic_collation_puts_a_ring_a_umlaut_and_o_umlaut_after_z() {
        let names = ["Älymystö", "Zen Café", "Åke", "abba", "Öljy", "Motörhead"];
        assert_eq!(
            sorted(&names, true),
            ["abba", "Motörhead", "Zen Café", "Åke", "Älymystö", "Öljy"]
        );
        assert!(sort_key_with("Zz", true) < sort_key_with("Åke", true));
        assert!(sort_key_with("Åke", true) < sort_key_with("Ärsyttävä", true));
        assert!(sort_key_with("Ärsyttävä", true) < sort_key_with("Öljy", true));
        assert_eq!(sort_key_with("Æble", true), sort_key_with("Äble", true));
    }

    #[test]
    fn other_locales_fold_nordic_letters() {
        let names = ["Älymystö", "Zen Café", "Åke", "abba", "Öljy", "Motörhead"];
        assert_eq!(
            sorted(&names, false),
            ["abba", "Åke", "Älymystö", "Motörhead", "Öljy", "Zen Café"]
        );
        assert_eq!(sort_key_with("Älymystö", false), "alymysto");
        assert_eq!(sort_key_with("Åke", false), "ake");
        assert_eq!(sort_key_with("Røyksopp", false), "royksopp");
        assert_eq!(sort_key_with("Mæstro", false), "maestro");
    }

    #[test]
    fn nordic_locale_comes_from_lc_all_lc_collate_or_lang() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert!(is_nordic_locale(env(&[("LANG", "fi_FI.UTF-8")])));
        assert!(is_nordic_locale(env(&[("LANG", "sv_SE.UTF-8")])));
        assert!(!is_nordic_locale(env(&[("LANG", "en_US.UTF-8")])));
        assert!(!is_nordic_locale(env(&[])));
        // LC_ALL overrides the others, LC_COLLATE overrides LANG, and empty values
        // are skipped.
        assert!(!is_nordic_locale(env(&[
            ("LC_ALL", "C"),
            ("LC_COLLATE", "fi_FI.UTF-8"),
            ("LANG", "fi_FI.UTF-8"),
        ])));
        assert!(is_nordic_locale(env(&[
            ("LC_COLLATE", "sv_FI.UTF-8"),
            ("LANG", "en_GB.UTF-8"),
        ])));
        assert!(is_nordic_locale(env(&[
            ("LC_ALL", ""),
            ("LC_COLLATE", ""),
            ("LANG", "fi_FI.UTF-8"),
        ])));
    }

    #[test]
    fn same_year_albums_use_sort_key() {
        let mut albums = vec![
            album("1", "Zuma", "X", Some(2001)),
            album("2", "The Yes Album", "X", Some(2001)),
            album("3", "Ébauche", "X", Some(2001)),
            album("4", "Bleach", "X", Some(1989)),
        ];
        sort_by_year(&mut albums);
        assert_eq!(
            names(&albums),
            ["Bleach", "Ébauche", "The Yes Album", "Zuma"]
        );
    }

    #[test]
    fn albums_by_oldest_first_without_year_last() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mut shelf) = shelf_in(&dir);
        shelf
            .add(album("1", "In Utero", "Nirvana", Some(1993)))
            .unwrap();
        shelf.add(album("2", "Unplugged", "Nirvana", None)).unwrap();
        shelf
            .add(album("3", "Nevermind", "Nirvana", Some(1991)))
            .unwrap();
        shelf
            .add(album("4", "Bleach", "Nirvana", Some(1989)))
            .unwrap();
        shelf
            .add(album("5", "Ok Computer", "Radiohead", Some(1997)))
            .unwrap();

        let nirvana = albums_of(shelf.albums(), "id-nirvana");
        assert_eq!(
            names(&nirvana),
            ["Bleach", "Nevermind", "In Utero", "Unplugged"]
        );
        assert!(albums_of(shelf.albums(), "id-unknown").is_empty());
    }

    #[test]
    fn album_id_comes_from_uri_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        shelf
            .add(album("4bc", "Nevermind", "Nirvana", Some(1991)))
            .unwrap();
        let loaded = albums_of(Shelf::load(&path).unwrap().albums(), "id-nirvana");
        assert_eq!(loaded[0].id, "4bc");
    }

    #[test]
    fn broken_file_is_error_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shelf.json");
        let broken = "[{\"uri\": \"spotify:album:abc\", \"name\": ";
        fs::write(&path, broken).unwrap();

        let err = Shelf::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("is broken"), "{err:#}");
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn wrong_shape_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shelf.json");
        fs::write(&path, r#"{"albums": []}"#).unwrap();
        assert!(Shelf::load(&path).is_err());
    }

    #[test]
    fn unavailable_shelf_is_read_only() {
        let mut shelf = Shelf::unavailable();
        let a = album("abc", "Nevermind", "Nirvana", Some(1991));
        assert!(shelf.add(a.clone()).is_err());
        assert!(!shelf.contains(&a.uri));
        assert!(artists_of(shelf.albums()).is_empty());
        assert!(!shelf.remove(&a.uri).unwrap());
    }

    #[test]
    fn failed_save_rolls_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        let a = album("abc", "Nevermind", "Nirvana", Some(1991));
        shelf.add(a.clone()).unwrap();
        // The directory becomes read-only: the file can be read but not saved.
        let deck = path.parent().unwrap();
        fs::set_permissions(deck, fs::Permissions::from_mode(0o500)).unwrap();
        let b = album("def", "Bleach", "Nirvana", Some(1989));
        let added = shelf.add(b.clone());
        let removed = shelf.remove(&a.uri);
        fs::set_permissions(deck, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(added.is_err());
        assert!(removed.is_err());
        assert!(!shelf.contains(&b.uri));
        assert!(shelf.contains(&a.uri));
    }

    #[test]
    fn changes_keep_edits_made_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        let nevermind = album("abc", "Nevermind", "Nirvana", Some(1991));
        let bleach = album("def", "Bleach", "Nirvana", Some(1989));
        shelf.add(nevermind.clone()).unwrap();

        // The user adds an album by hand while Deck is open.
        let mut by_hand = Shelf::load(&path).unwrap();
        by_hand.add(bleach.clone()).unwrap();

        shelf
            .add(album("ghi", "In Utero", "Nirvana", Some(1993)))
            .unwrap();
        assert_eq!(names(shelf.albums()), ["Nevermind", "Bleach", "In Utero"]);
        assert!(shelf.remove(&nevermind.uri).unwrap());
        assert_eq!(
            names(Shelf::load(&path).unwrap().albums()),
            ["Bleach", "In Utero"]
        );
    }

    #[test]
    fn file_broken_by_hand_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut shelf) = shelf_in(&dir);
        let nevermind = album("abc", "Nevermind", "Nirvana", Some(1991));
        shelf.add(nevermind.clone()).unwrap();
        fs::write(&path, "[{").unwrap();

        let error = shelf
            .add(album("def", "Bleach", "Nirvana", Some(1989)))
            .unwrap_err();
        assert!(format!("{error:#}").contains("is broken"), "{error:#}");
        assert!(shelf.remove(&nevermind.uri).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[{");
        assert_eq!(names(shelf.albums()), ["Nevermind"]);
    }
}
