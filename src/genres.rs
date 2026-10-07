//! The genre radio catalog and favourites.
//!
//! The catalog (`genres/genres.json`) is compiled into the program. Each genre has a
//! stable id, a display name and seeds: typical artists or tracks of the genre whose
//! Spotify station the radio plays ([`crate::radio`]). Seeds are chosen so that the
//! station hits the genre; the manual run `live_catalog_stations` fetches every station.
//!
//! Favourites (`~/.config/deck/genres.json`, `{"favorites": [...]}`) are handled like
//! the shelf: every change rereads the file first and saves it atomically right away.
//! A broken file is never overwritten: `load` and every change return an error, and if
//! the file is broken at startup the caller uses [`Favorites::unavailable`], which is
//! empty and read-only. An id that has left the catalog is skipped in the view but kept in
//! the file.
//!
//! The user's own genres (`~/.config/deck/my-genres.json`, [`MyGenres`]) have the same
//! shape as the catalog, and `deck genre add` (`genre_cli`) writes them. Deck reads them
//! at startup ([`install`]), and an own genre replaces the catalog genre of the same
//! name: the id is derived from the name ([`slug`]), so favourites and playback state
//! are kept.

use std::{
    collections::HashSet,
    hash::{BuildHasher, RandomState},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    config,
    store::{read_json, save_json},
};

const CATALOG: &str = include_str!("../genres/genres.json");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genre {
    /// Stable id by which favourites and playback state refer to the genre.
    pub id: String,
    /// Display name in lowercase.
    pub name: String,
    pub seeds: Vec<Seed>,
}

/// A radio seed: an artist or track URI. The name is for whoever reads the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seed {
    pub uri: String,
    pub name: String,
}

static OWN: OnceLock<Vec<Genre>> = OnceLock::new();

/// Includes the user's own genres in [`all`]. Called once at startup before the first
/// `all`; a later call has no effect. Tests see the catalog alone.
pub fn install(own: Vec<Genre>) {
    let _ = OWN.set(own);
}

/// The catalog and the user's own genres combined, in alphabetical order by name.
pub fn all() -> &'static [Genre] {
    static GENRES: OnceLock<Vec<Genre>> = OnceLock::new();
    GENRES.get_or_init(|| merge(catalog(), OWN.get().map_or(&[], Vec::as_slice)))
}

/// The catalog shipped with Deck, without the user's own genres.
pub fn catalog() -> Vec<Genre> {
    parse(CATALOG).expect("genres/genres.json is invalid")
}

/// The catalog where an own genre replaces the catalog genre with the same id and the
/// other own genres are added. In alphabetical order by name.
pub fn merge(mut catalog: Vec<Genre>, own: &[Genre]) -> Vec<Genre> {
    catalog.retain(|g| !own.iter().any(|o| o.id == g.id));
    catalog.extend(own.iter().cloned());
    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// An id from a name: lowercase, and anything other than letters and digits becomes a
/// hyphen (`60s rock` → `60s-rock`). Catalog ids follow the same rule.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

pub fn find(id: &str) -> Option<&'static Genre> {
    all().iter().find(|g| g.id == id)
}

/// Parses and checks the catalog: ids and names are unique, and every genre has
/// seeds that are artist or track URIs.
fn parse(text: &str) -> Result<Vec<Genre>> {
    let mut genres: Vec<Genre> = serde_json::from_str(text).context("invalid genre catalog")?;
    validate(&genres)?;
    genres.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(genres)
}

fn validate(genres: &[Genre]) -> Result<()> {
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    for genre in genres {
        if !ids.insert(genre.id.as_str()) {
            bail!("duplicate genre id {}", genre.id);
        }
        if !names.insert(genre.name.as_str()) {
            bail!("duplicate genre name {}", genre.name);
        }
        if genre.seeds.is_empty() {
            bail!("genre {} has no seeds", genre.id);
        }
        let mut uris = HashSet::new();
        for seed in &genre.seeds {
            if !is_seed_uri(&seed.uri) {
                bail!(
                    "genre {}: seed {} is not an artist or track URI",
                    genre.id,
                    seed.uri
                );
            }
            if !uris.insert(seed.uri.as_str()) {
                bail!("genre {}: duplicate seed {}", genre.id, seed.uri);
            }
        }
    }
    Ok(())
}

pub fn is_seed_uri(uri: &str) -> bool {
    let id = uri
        .strip_prefix("spotify:artist:")
        .or_else(|| uri.strip_prefix("spotify:track:"));
    id.is_some_and(|id| id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl Genre {
    /// Picks a random seed that is not `current`. Seeds not in `played` are picked
    /// first; once all have been played, the draw starts over.
    /// `random` is a random number (see [`random`]), so the draw can be tested.
    pub fn pick_seed(&self, current: Option<&str>, played: &[String], random: u64) -> &Seed {
        let others: Vec<&Seed> = self
            .seeds
            .iter()
            .filter(|s| Some(s.uri.as_str()) != current)
            .collect();
        let unplayed: Vec<&Seed> = others
            .iter()
            .copied()
            .filter(|s| !played.contains(&s.uri))
            .collect();
        let pool = match (unplayed.is_empty(), others.is_empty()) {
            (false, _) => unplayed,
            (true, false) => others,
            // The only seed is the one playing: better to play it again than nothing.
            (true, true) => self.seeds.iter().collect(),
        };
        pool[(random % pool.len() as u64) as usize]
    }
}

/// A random number for the draw without an extra dependency: `RandomState` gets new
/// keys every time.
pub fn random() -> u64 {
    RandomState::new().hash_one(())
}

#[derive(Debug)]
pub struct Favorites {
    /// `None` when the favourites are read-only.
    path: Option<PathBuf>,
    /// The ids in the file as they are, including those no longer in the catalog.
    ids: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct FavoritesFile {
    favorites: Vec<String>,
}

impl Favorites {
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::config_dir()?.join("genres.json"))
    }

    /// Reads the favourites. A missing file means there are no favourites.
    pub fn load(path: &Path) -> Result<Self> {
        let file: Option<FavoritesFile> = read_json(path, "genre favorites")?;
        Ok(Self {
            path: Some(path.to_owned()),
            ids: file.map(|f| f.favorites).unwrap_or_default(),
        })
    }

    /// Empty favourites that cannot be saved: used when `load` failed.
    pub fn unavailable() -> Self {
        Self {
            path: None,
            ids: Vec::new(),
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|f| f == id)
    }

    /// The favourite genres from the catalog in alphabetical order by name. An unknown
    /// id is skipped.
    pub fn genres(&self) -> Vec<&'static Genre> {
        all().iter().filter(|g| self.contains(&g.id)).collect()
    }

    /// Adds a catalog genre and saves. Returns `false` if the genre was already a
    /// favourite.
    pub fn add(&mut self, id: &str) -> Result<bool> {
        if find(id).is_none() {
            bail!("unknown genre {id}");
        }
        self.reload()?;
        if self.contains(id) {
            return Ok(false);
        }
        self.ids.push(id.to_owned());
        if let Err(e) = self.save() {
            self.ids.pop();
            return Err(e);
        }
        Ok(true)
    }

    /// Removes the genre and saves. Returns `false` if the genre was not a favourite.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        self.reload()?;
        let Some(index) = self.ids.iter().position(|f| f == id) else {
            return Ok(false);
        };
        let removed = self.ids.remove(index);
        if let Err(e) = self.save() {
            self.ids.insert(index, removed);
            return Err(e);
        }
        Ok(true)
    }

    /// Rereads the file before a change, so that edits made by hand while Deck is open
    /// are kept. If it has become broken, the error is returned and the favourites in
    /// memory stay as they were.
    fn reload(&mut self) -> Result<()> {
        if let Some(path) = &self.path {
            self.ids = Self::load(path)?.ids;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            bail!("genre favorites are read-only because the favorites file could not be read");
        };
        save_json(
            path,
            &FavoritesFile {
                favorites: self.ids.clone(),
            },
        )
    }
}

/// The user's own genres (`~/.config/deck/my-genres.json`, `{"genres": [...]}`).
/// A broken file is not overwritten: `load` returns an error.
#[derive(Debug)]
pub struct MyGenres {
    path: PathBuf,
    genres: Vec<Genre>,
}

#[derive(Serialize, Deserialize)]
struct MyGenresFile {
    genres: Vec<Genre>,
}

impl MyGenres {
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::config_dir()?.join("my-genres.json"))
    }

    /// Reads and checks the user's own genres like the catalog; in addition, the id must
    /// be the [`slug`] of the name. A missing file means there are no own genres.
    pub fn load(path: &Path) -> Result<Self> {
        let file: Option<MyGenresFile> = read_json(path, "my genres")?;
        let genres = file.map(|f| f.genres).unwrap_or_default();
        (|| {
            validate(&genres)?;
            for genre in &genres {
                if genre.id != slug(&genre.name) {
                    bail!("genre {}: id must be {}", genre.name, slug(&genre.name));
                }
            }
            Ok(())
        })()
        .with_context(|| format!("my genres file {} is broken", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            genres,
        })
    }

    pub fn genres(&self) -> &[Genre] {
        &self.genres
    }

    pub fn find(&self, id: &str) -> Option<&Genre> {
        self.genres.iter().find(|g| g.id == id)
    }

    /// Adds the genre, or replaces the own genre with the same id, and saves.
    pub fn put(&mut self, genre: Genre) -> Result<()> {
        let mut genres = self.genres.clone();
        match genres.iter_mut().find(|g| g.id == genre.id) {
            Some(old) => *old = genre,
            None => genres.push(genre),
        }
        validate(&genres)?;
        save_json(
            &self.path,
            &MyGenresFile {
                genres: genres.clone(),
            },
        )?;
        self.genres = genres;
        Ok(())
    }

    /// Removes an own genre and saves. Returns `false` if there was no such genre.
    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let Some(index) = self.genres.iter().position(|g| g.id == id) else {
            return Ok(false);
        };
        let mut genres = self.genres.clone();
        genres.remove(index);
        save_json(
            &self.path,
            &MyGenresFile {
                genres: genres.clone(),
            },
        )?;
        self.genres = genres;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(n: u32) -> Seed {
        Seed {
            uri: format!("spotify:artist:{n:022}"),
            name: format!("Artist {n}"),
        }
    }

    fn genre(seeds: u32) -> Genre {
        Genre {
            id: "test".into(),
            name: "test".into(),
            seeds: (0..seeds).map(seed).collect(),
        }
    }

    fn uris(seeds: &[&Seed]) -> Vec<String> {
        seeds.iter().map(|s| s.uri.clone()).collect()
    }

    /// The files in `path`'s directory: saving leaves no temporary files behind.
    fn dir_files(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn favorites_in(dir: &tempfile::TempDir) -> (PathBuf, Favorites) {
        let path = dir.path().join("deck/genres.json");
        let favorites = Favorites::load(&path).unwrap();
        (path, favorites)
    }

    fn file_ids(path: &Path) -> Vec<String> {
        let file: FavoritesFile =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        file.favorites
    }

    fn own(name: &str, seeds: u32) -> Genre {
        Genre {
            id: slug(name),
            name: name.into(),
            seeds: (0..seeds).map(seed).collect(),
        }
    }

    #[test]
    fn slug_from_name() {
        assert_eq!(slug("60s rock"), "60s-rock");
        assert_eq!(slug("R&B / Soul"), "r-b-soul");
        assert_eq!(slug("  Hip Hop! "), "hip-hop");
        assert_eq!(slug("iskelmä"), "iskelmä");
    }

    #[test]
    fn catalog_ids_are_slugs_of_names() {
        for genre in all() {
            assert_eq!(genre.id, slug(&genre.name));
        }
    }

    #[test]
    fn own_genre_replaces_catalog_genre_and_others_are_added() {
        let catalog = vec![own("lofi", 2), own("synthwave", 2)];
        let merged = merge(catalog, &[own("lofi", 5), own("60s rock", 3)]);
        let names: Vec<&str> = merged.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["60s rock", "lofi", "synthwave"]);
        assert_eq!(merged[1].seeds.len(), 5);
    }

    #[test]
    fn missing_my_genres_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let my = MyGenres::load(&dir.path().join("my-genres.json")).unwrap();
        assert!(my.genres().is_empty());
    }

    #[test]
    fn my_genres_put_replaces_and_remove_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck/my-genres.json");
        let mut my = MyGenres::load(&path).unwrap();
        my.put(own("60s rock", 3)).unwrap();
        my.put(own("lofi", 3)).unwrap();
        my.put(own("60s rock", 4)).unwrap();

        let reloaded = MyGenres::load(&path).unwrap();
        assert_eq!(reloaded.genres().len(), 2);
        assert_eq!(reloaded.find("60s-rock").unwrap().seeds.len(), 4);

        assert!(my.remove("lofi").unwrap());
        assert!(!my.remove("lofi").unwrap());
        let reloaded = MyGenres::load(&path).unwrap();
        assert!(reloaded.find("lofi").is_none());
        assert_eq!(dir_files(&path), ["my-genres.json"]);
    }

    #[test]
    fn broken_my_genres_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-genres.json");
        let cases = [
            ("{ not json", "my genres file"),
            (
                r#"{"genres": [{"id": "x", "name": "60s rock", "seeds": [{"uri": "spotify:artist:5FsfZj0Mp6YwEWytuJUcWt", "name": "x"}]}]}"#,
                "id must be 60s-rock",
            ),
            (
                r#"{"genres": [{"id": "a", "name": "a", "seeds": []}]}"#,
                "no seeds",
            ),
        ];
        for (text, error) in cases {
            std::fs::write(&path, text).unwrap();
            let e = format!("{:#}", MyGenres::load(&path).unwrap_err());
            assert!(e.contains("my genres file") && e.contains(error), "{e}");
        }
    }

    #[test]
    fn failed_put_keeps_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-genres.json");
        let mut my = MyGenres::load(&path).unwrap();
        // A directory where the file should be: saving fails.
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "").unwrap();
        assert!(my.put(own("60s rock", 3)).is_err());
        assert!(my.genres().is_empty());
    }

    #[test]
    fn catalog_meets_the_minimum() {
        let genres = all();
        assert!(genres.len() >= 15, "only {} genres", genres.len());
        for genre in genres {
            assert!(
                genre.seeds.len() >= 6,
                "{} has only {} seeds",
                genre.id,
                genre.seeds.len()
            );
            assert_eq!(genre.name, genre.name.to_lowercase(), "{}", genre.id);
        }
        assert!(find("lofi").is_some());
        assert!(find("synthwave").is_some());
    }

    #[test]
    fn catalog_is_sorted_by_name() {
        let names: Vec<&str> = all().iter().map(|g| g.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn unknown_id_is_not_found() {
        assert!(find("polka-noir").is_none());
    }

    #[test]
    fn parse_rejects_invalid_catalogs() {
        let ok = r#"[{"id":"a","name":"a","seeds":[{"uri":"spotify:track:0HSwIZSDeOQX8Cu9pCGkjS","name":"x"}]}]"#;
        assert_eq!(parse(ok).unwrap().len(), 1);

        let cases = [
            (r#"[{"id":"a","name":"a","seeds":[]}]"#, "no seeds"),
            (
                r#"[{"id":"a","name":"a","seeds":[{"uri":"spotify:playlist:37i9dQZF1DWWQRwui0ExPn","name":"x"}]}]"#,
                "not an artist or track",
            ),
            (
                r#"[{"id":"a","name":"a","seeds":[{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"},{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"}]}]"#,
                "duplicate seed",
            ),
            (
                r#"[{"id":"a","name":"a","seeds":[{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"}]},
                    {"id":"a","name":"b","seeds":[{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"}]}]"#,
                "duplicate genre id",
            ),
            (
                r#"[{"id":"a","name":"a","seeds":[{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"}]},
                    {"id":"b","name":"a","seeds":[{"uri":"spotify:artist:5FsfZj0Mp6YwEWytuJUcWt","name":"x"}]}]"#,
                "duplicate genre name",
            ),
            ("{", "invalid genre catalog"),
        ];
        for (text, error) in cases {
            let e = format!("{:#}", parse(text).unwrap_err());
            assert!(e.contains(error), "{e}");
        }
    }

    #[test]
    fn pick_seed_differs_from_current() {
        let genre = genre(6);
        let current = genre.seeds[2].uri.clone();
        for random in 0..60 {
            assert_ne!(genre.pick_seed(Some(&current), &[], random).uri, current);
        }
    }

    #[test]
    fn pick_seed_prefers_unplayed_seeds() {
        let genre = genre(6);
        let played: Vec<String> = genre.seeds[..4].iter().map(|s| s.uri.clone()).collect();
        let picked: HashSet<String> = (0..60)
            .map(|random| {
                genre
                    .pick_seed(Some(&played[3]), &played, random)
                    .uri
                    .clone()
            })
            .collect();
        let expected: HashSet<String> = uris(&genre.seeds[4..].iter().collect::<Vec<_>>())
            .into_iter()
            .collect();
        assert_eq!(picked, expected);
    }

    #[test]
    fn pick_seed_starts_over_when_all_are_played() {
        let genre = genre(6);
        let played: Vec<String> = genre.seeds.iter().map(|s| s.uri.clone()).collect();
        let current = &played[0];
        let picked: HashSet<String> = (0..60)
            .map(|random| genre.pick_seed(Some(current), &played, random).uri.clone())
            .collect();
        assert_eq!(picked.len(), 5);
        assert!(!picked.contains(current));
    }

    #[test]
    fn pick_seed_without_current_uses_every_seed() {
        let genre = genre(6);
        let picked: HashSet<String> = (0..60)
            .map(|random| genre.pick_seed(None, &[], random).uri.clone())
            .collect();
        assert_eq!(picked.len(), 6);
    }

    #[test]
    fn pick_seed_with_a_single_seed_repeats_it() {
        let genre = genre(1);
        let only = genre.seeds[0].uri.clone();
        assert_eq!(
            genre
                .pick_seed(Some(&only), std::slice::from_ref(&only), 7)
                .uri,
            only
        );
    }

    #[test]
    fn random_varies() {
        let values: HashSet<u64> = (0..10).map(|_| random()).collect();
        assert!(values.len() > 1);
    }

    #[test]
    fn missing_file_means_no_favorites() {
        let dir = tempfile::tempdir().unwrap();
        let (path, favorites) = favorites_in(&dir);
        assert!(favorites.genres().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn add_saves_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut favorites) = favorites_in(&dir);
        assert!(favorites.add("synthwave").unwrap());
        assert!(favorites.add("lofi").unwrap());
        assert!(!favorites.add("lofi").unwrap());
        assert_eq!(file_ids(&path), ["synthwave", "lofi"]);
        assert_eq!(dir_files(&path), ["genres.json"]);

        let reloaded = Favorites::load(&path).unwrap();
        let ids: Vec<&str> = reloaded.genres().iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["lofi", "synthwave"]);
    }

    #[test]
    fn add_rejects_unknown_genre() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut favorites) = favorites_in(&dir);
        assert!(favorites.add("polka-noir").is_err());
        assert!(!favorites.contains("polka-noir"));
        assert!(!path.exists());
    }

    #[test]
    fn remove_saves() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut favorites) = favorites_in(&dir);
        favorites.add("lofi").unwrap();
        favorites.add("synthwave").unwrap();
        assert!(favorites.remove("lofi").unwrap());
        assert!(!favorites.remove("lofi").unwrap());
        assert_eq!(file_ids(&path), ["synthwave"]);
        assert!(!Favorites::load(&path).unwrap().contains("lofi"));
    }

    #[test]
    fn unknown_id_is_hidden_but_kept_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("genres.json");
        std::fs::write(&path, r#"{"favorites": ["polka-noir", "lofi"]}"#).unwrap();
        let mut favorites = Favorites::load(&path).unwrap();
        let ids: Vec<&str> = favorites.genres().iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["lofi"]);

        favorites.add("synthwave").unwrap();
        assert_eq!(file_ids(&path), ["polka-noir", "lofi", "synthwave"]);
        assert!(favorites.remove("polka-noir").unwrap());
        assert_eq!(file_ids(&path), ["lofi", "synthwave"]);
    }

    #[test]
    fn broken_file_is_an_error_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("genres.json");
        std::fs::write(&path, "{ not json").unwrap();
        let e = format!("{:#}", Favorites::load(&path).unwrap_err());
        assert!(e.contains("genre favorites file"), "{e}");

        let mut favorites = Favorites::unavailable();
        assert!(favorites.genres().is_empty());
        assert!(favorites.add("lofi").is_err());
        assert!(!favorites.contains("lofi"));
        assert!(!favorites.remove("lofi").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn changes_keep_edits_made_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let (path, mut favorites) = favorites_in(&dir);
        favorites.add("lofi").unwrap();
        std::fs::write(&path, r#"{"favorites": ["lofi", "synthwave"]}"#).unwrap();

        favorites.remove("lofi").unwrap();
        assert_eq!(file_ids(&path), ["synthwave"]);
        assert!(favorites.contains("synthwave"));

        // A file broken by hand is an error and is left as it is.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(favorites.add("lofi").is_err());
        assert!(favorites.remove("synthwave").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        assert!(favorites.contains("synthwave"));
    }

    /// Manual run:
    /// `cargo test genres::tests::live_catalog_stations -- --ignored --nocapture`.
    /// Fetches the station of every seed and prints a sample of it.
    #[tokio::test]
    #[ignore = "requires Spotify credentials and network"]
    async fn live_catalog_stations() -> Result<()> {
        let client = crate::radio::Client::new(crate::player::SharedSession::new(
            crate::player::connect().await?,
        ));
        let mut failed = Vec::new();
        for genre in all() {
            for seed in &genre.seeds {
                match client.page(&seed.uri, &[]).await {
                    Ok(tracks) if tracks.len() >= 20 => {
                        let sample: Vec<String> = tracks
                            .iter()
                            .skip(1)
                            .step_by(12)
                            .map(|t| format!("{} · {}", t.artist, t.name))
                            .collect();
                        println!(
                            "{} | {} | {} | {}",
                            genre.name,
                            seed.name,
                            tracks.len(),
                            sample.join(" / ")
                        );
                    }
                    Ok(tracks) => failed.push(format!(
                        "{} / {}: only {} tracks",
                        genre.id,
                        seed.name,
                        tracks.len()
                    )),
                    Err(e) => failed.push(format!("{} / {}: {e:#}", genre.id, seed.name)),
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
        Ok(())
    }

    #[test]
    fn failed_save_keeps_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("genres.json");
        let mut favorites = Favorites::load(&path).unwrap();
        // A directory where the file should be: saving fails.
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), "").unwrap();
        assert!(favorites.add("lofi").is_err());
        assert!(!favorites.contains("lofi"));
    }
}
