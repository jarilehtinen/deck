mod app;
mod catalog;
mod config;
mod curate;
mod curate_run;
mod genre_cli;
mod genres;
mod lastfm;
mod lists;
mod now_playing;
mod playback;
mod player;
mod radio;
mod rate_limit;
mod shelf;
mod store;
mod ui;

use std::{
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use jiff::Timestamp;
use ratatui::{
    DefaultTerminal,
    crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    time::MissedTickBehavior,
};

use crate::{
    app::{Action, Key, State},
    catalog::Catalog,
    config::Config,
    genres::{Favorites, MyGenres},
    lists::{AlbumList, CuratedFiles},
    playback::PlaybackFile,
    player::{Command, Player, SharedSession},
    shelf::Shelf,
};

/// Tick interval: must be shorter than `app::SEARCH_DEBOUNCE`. The tick also redraws
/// the elapsed time and the progress bar.
const TICK: Duration = Duration::from_millis(100);

const USAGE: &str = "usage: deck [taste | curate | curate submit [--dry-run] \
                     | genre add [--dry-run] | genre remove <name> | genre list | genre prompt \
                     | --help | --version]";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Plain `deck`: the player and the UI.
    Deck,
    Taste,
    /// The Curated run (`curate_run`).
    Curate,
    /// What the run's Claude calls: checks candidates and writes the list (`curate`).
    CurateSubmit {
        dry_run: bool,
    },
    Genre(GenreCommand),
    /// The Now Playing helper process Deck starts itself (`now_playing`), not in the help.
    NowPlaying,
    /// `--help` / `-h`: usage to stdout.
    Help,
    /// `--version` / `-V`.
    Version,
}

/// `deck genre …` (`genre_cli`).
#[derive(Debug, PartialEq, Eq)]
enum GenreCommand {
    Add {
        dry_run: bool,
    },
    /// The name may span several arguments: `deck genre remove 60s rock`.
    Remove(String),
    List,
    Prompt,
}

impl Mode {
    /// The error is the usage to print: `deck genre …` has its own, the rest [`USAGE`].
    fn parse(args: &[String]) -> Result<Self, &'static str> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] => Ok(Self::Deck),
            ["taste"] => Ok(Self::Taste),
            ["curate"] => Ok(Self::Curate),
            ["curate", "submit"] => Ok(Self::CurateSubmit { dry_run: false }),
            ["curate", "submit", "--dry-run"] => Ok(Self::CurateSubmit { dry_run: true }),
            ["genre", "add"] => Ok(Self::Genre(GenreCommand::Add { dry_run: false })),
            ["genre", "add", "--dry-run"] => Ok(Self::Genre(GenreCommand::Add { dry_run: true })),
            ["genre", "remove", name @ ..] if !name.is_empty() => {
                Ok(Self::Genre(GenreCommand::Remove(name.join(" "))))
            }
            ["genre", "list"] => Ok(Self::Genre(GenreCommand::List)),
            ["genre", "prompt"] => Ok(Self::Genre(GenreCommand::Prompt)),
            ["now-playing"] => Ok(Self::NowPlaying),
            ["--help" | "-h"] => Ok(Self::Help),
            ["--version" | "-V"] => Ok(Self::Version),
            ["genre", ..] => Err(genre_cli::USAGE),
            _ => Err(USAGE),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match Mode::parse(&args) {
        Ok(mode) => mode,
        Err(usage) => return usage_error(usage),
    };
    match mode {
        Mode::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Mode::Version => {
            println!("deck {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    // Now Playing runs Cocoa's event loop on the main thread and needs no tokio.
    let outcome = if mode == Mode::NowPlaying {
        now_playing()
    } else {
        tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(run_mode(mode)))
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => match e.downcast_ref::<genre_cli::Usage>() {
            Some(usage) => usage_error(usage.0),
            None => {
                eprintln!("deck: {e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Prints the usage to stderr; exit code 2 as with invalid arguments.
fn usage_error(usage: &str) -> ExitCode {
    eprintln!("{}", usage.trim_end());
    ExitCode::from(2)
}

#[cfg(target_os = "macos")]
fn now_playing() -> Result<()> {
    now_playing::serve()
}

#[cfg(not(target_os = "macos"))]
fn now_playing() -> Result<()> {
    anyhow::bail!("now-playing is only available on macOS")
}

async fn run_mode(mode: Mode) -> Result<()> {
    match mode {
        Mode::Deck => {
            init_logging();
            run().await
        }
        Mode::Taste => {
            init_stderr_logging();
            curate::taste().await
        }
        Mode::Curate => {
            init_stderr_logging();
            curate_run::run()
        }
        Mode::CurateSubmit { dry_run } => {
            init_stderr_logging();
            curate::submit(dry_run).await
        }
        Mode::Genre(command) => {
            init_stderr_logging();
            match command {
                GenreCommand::Add { dry_run } => genre_cli::add(dry_run).await,
                GenreCommand::Remove(name) => genre_cli::remove(&name),
                GenreCommand::List => genre_cli::list(),
                GenreCommand::Prompt => {
                    print!("{}", genre_cli::PROMPT);
                    Ok(())
                }
            }
        }
        Mode::NowPlaying => now_playing(),
        // Already handled in `main` before tokio.
        Mode::Help | Mode::Version => Ok(()),
    }
}

/// Signs in before the UI: a failed sign-in ends the program with a clear message.
/// After that, errors are shown in the UI.
async fn run() -> Result<()> {
    let config = Config::load()?;
    // The user's own genres before anything reads the genre list; a broken file shows
    // up as an error, and Deck shows only the catalog.
    let my_genres_error = match MyGenres::default_path().and_then(|path| MyGenres::load(&path)) {
        Ok(my) => {
            genres::install(my.genres().to_vec());
            None
        }
        Err(e) => Some(format!("{e:#}")),
    };
    let session = player::connect().await?;
    let shared = SharedSession::new(session.clone());
    let catalog = Catalog::connect(shared.clone(), &config).await?;

    let (shelf, shelf_error) = match Shelf::default_path().and_then(|path| Shelf::load(&path)) {
        Ok(shelf) => (shelf, None),
        Err(e) => (Shelf::unavailable(), Some(format!("{e:#}"))),
    };
    let (favorites, favorites_error) =
        match Favorites::default_path().and_then(|path| Favorites::load(&path)) {
            Ok(favorites) => (favorites, None),
            Err(e) => (Favorites::unavailable(), Some(format!("{e:#}"))),
        };
    let lists = CuratedFiles::default_paths()?;
    let mut state = State::new(shelf.albums().to_vec())
        .with_favorites(favorite_ids(&favorites))
        .with_curated(curated_list(&lists))
        .with_likes(catalog.can_like());
    if let Some(error) = shelf_error.or(favorites_error).or(my_genres_error) {
        state.error = Some(error);
    }

    // Playback state of the previous session. The track loads in the background, so the
    // UI opens at once; a broken file shows up as an error and Deck opens empty.
    let playback = PlaybackFile::default_path()?;
    let restore = match playback.load() {
        Ok(Some(saved)) => state.update(app::Event::Restore(Box::new(saved)), Instant::now()),
        Ok(None) => Vec::new(),
        Err(e) => {
            state.error.get_or_insert(format!("{e:#}"));
            Vec::new()
        }
    };

    let radio = radio::Client::new(shared.clone());
    let (player, player_events) = Player::spawn(shared, session, catalog.clone(), config.bitrate);
    let (results, results_rx) = mpsc::unbounded_channel();
    let media = start_now_playing(&results);
    let mut deck = Deck {
        player,
        catalog,
        radio,
        shelf,
        favorites,
        lists,
        playback,
        results,
        media,
    };
    for action in restore {
        deck.perform(action);
    }

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, state, deck, player_events, results_rx).await;
    ratatui::restore();
    outcome
}

/// Feeds key presses, player events, action results and ticks into `State::update`
/// and carries out the actions it returns.
async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut state: State,
    mut deck: Deck,
    mut player_events: UnboundedReceiver<player::Event>,
    mut results: UnboundedReceiver<app::Event>,
) -> Result<()> {
    let mut input = spawn_input_reader();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| ui::draw(frame, &state, Instant::now()))?;
        let event = tokio::select! {
            Some(key) = input.recv() => match key {
                Some(key) => app::Event::Key(key),
                // The window was resized: redraw.
                None => continue,
            },
            Some(event) = player_events.recv() => app::Event::Player(event),
            Some(event) = results.recv() => event,
            _ = tick.tick() => app::Event::Tick,
        };
        let now = Instant::now();
        for action in state.update(event, now) {
            if action == Action::Quit {
                deck.save_playback(state.playback(now));
                return Ok(());
            }
            deck.perform(action);
        }
        if state.playback_changed() {
            deck.save_playback(state.playback(now));
        }
        if let Some(media) = &mut deck.media {
            media.update(state.now_playing_info(now), now);
        }
    }
}

/// macOS Now Playing and media keys. If the helper process does not start, Deck works
/// without them.
fn start_now_playing(results: &UnboundedSender<app::Event>) -> Option<now_playing::Session> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let results = results.clone();
    now_playing::Session::start(move |command| {
        let _ = results.send(app::Event::Media(command));
    })
    .inspect_err(|e| log::warn!("{e:#}"))
    .ok()
}

/// Carries out actions: player, Web API, radio, shelf, favourite genres and lists.
/// Results return to the loop as events through the `results` channel.
struct Deck {
    player: Player,
    catalog: Catalog,
    radio: radio::Client,
    shelf: Shelf,
    favorites: Favorites,
    lists: CuratedFiles,
    playback: PlaybackFile,
    results: UnboundedSender<app::Event>,
    /// macOS Now Playing; `None` elsewhere or if the helper process did not start.
    media: Option<now_playing::Session>,
}

impl Deck {
    fn perform(&mut self, action: Action) {
        match action {
            Action::Play(item) => self.player.send(Command::Play(item)),
            Action::Preload(item) => self.player.send(Command::Preload(item)),
            Action::Restore { item, position } => {
                self.player.send(Command::Restore { item, position });
            }
            Action::TogglePause => self.player.send(Command::TogglePause),
            Action::Stop => self.player.send(Command::Stop),
            Action::Search(query) => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let result = catalog
                        .search(&query)
                        .await
                        .map_err(failed("search failed"));
                    app::Event::Searched { query, result }
                });
            }
            Action::LoadArtistAlbums(artist_id) => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let result = catalog
                        .artist_albums(&artist_id)
                        .await
                        .map_err(failed("could not load the artist's albums"));
                    app::Event::ArtistAlbums { artist_id, result }
                });
            }
            Action::LoadAlbumTracks(uri) => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let id = uri.strip_prefix("spotify:album:").unwrap_or(&uri);
                    let result = catalog
                        .album_tracks(id)
                        .await
                        .map_err(failed("could not load the album's tracks"));
                    app::Event::AlbumTracks { uri, result }
                });
            }
            Action::QueueAlbum(album) => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let result = catalog
                        .album_tracks(&album.id)
                        .await
                        .map_err(failed("could not queue the album"));
                    app::Event::AlbumQueued { album, result }
                });
            }
            Action::AddToShelf(album) => {
                let result = self.shelf.add(album);
                self.shelf_changed(result);
            }
            Action::RemoveFromShelf(uri) => {
                let result = self.shelf.remove(&uri);
                self.shelf_changed(result);
            }
            Action::LoadLists => {
                let _ = self
                    .results
                    .send(app::Event::ListsLoaded(curated_list(&self.lists)));
            }
            Action::RejectCurated(uri) => {
                let _ = self.results.send(reject_curated(&self.lists, &uri));
            }
            Action::LoadRadio(request) => {
                let radio = self.radio.clone();
                self.spawn(async move {
                    let result = radio
                        .page(&request.seed, &request.previous)
                        .await
                        .map_err(failed("could not load the radio"));
                    app::Event::RadioLoaded { request, result }
                });
            }
            Action::AddRadioAlbumToShelf(uri) => {
                let radio = self.radio.clone();
                self.spawn(async move {
                    let result = radio
                        .album_for_shelf(&uri)
                        .await
                        .map_err(failed("could not add the album to the shelf"));
                    app::Event::RadioAlbum(result)
                });
            }
            Action::AddFavorite(id) => {
                let result = self.favorites.add(&id);
                self.favorites_changed(result);
            }
            Action::RemoveFavorite(id) => {
                let result = self.favorites.remove(&id);
                self.favorites_changed(result);
            }
            Action::CheckLiked(uris) => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let result = catalog
                        .liked(&uris)
                        .await
                        .map_err(failed("could not check Liked Songs"));
                    app::Event::LikedChecked { uris, result }
                });
            }
            Action::SetLiked { uri, liked } => {
                let catalog = self.catalog.clone();
                self.spawn(async move {
                    let result = catalog
                        .set_liked(&uri, liked)
                        .await
                        .map_err(failed("could not change Liked Songs"));
                    app::Event::LikeChanged { uri, liked, result }
                });
            }
            Action::Quit => {}
        }
    }

    /// Runs a task in the background and sends its result back to the loop as an event.
    fn spawn(&self, task: impl Future<Output = app::Event> + Send + 'static) {
        let results = self.results.clone();
        tokio::spawn(async move {
            let _ = results.send(task.await);
        });
    }

    /// Saves the playback state, or removes it when nothing is playing.
    fn save_playback(&self, playback: Option<app::SavedPlayback>) {
        if let Err(e) = self.playback.save(playback.as_ref()) {
            log::warn!("{e:#}");
            let _ = self.results.send(app::Event::Error(format!("{e:#}")));
        }
    }

    fn favorites_changed(&self, result: Result<bool>) {
        let event = match result {
            Ok(_) => app::Event::FavoritesChanged(favorite_ids(&self.favorites)),
            Err(e) => app::Event::Error(format!("{e:#}")),
        };
        let _ = self.results.send(event);
    }

    fn shelf_changed(&self, result: Result<bool>) {
        let event = match result {
            Ok(_) => app::Event::ShelfChanged(self.shelf.albums().to_vec()),
            Err(e) => app::Event::Error(format!("{e:#}")),
        };
        let _ = self.results.send(event);
    }
}

/// An error as a UI message: `<what was attempted>: <cause>`.
fn failed(what: &'static str) -> impl Fn(anyhow::Error) -> String {
    move |e| format!("{what}: {e:#}")
}

/// IDs of the favourite genres for the view (catalog genres).
fn favorite_ids(favorites: &Favorites) -> Vec<String> {
    favorites.genres().iter().map(|g| g.id.clone()).collect()
}

/// The Curated list for the view. A broken file is an error.
fn curated_list(files: &CuratedFiles) -> Result<Option<AlbumList>, String> {
    files
        .load()
        .map(|curated| curated.map(|curated| curated.to_list(Timestamp::now())))
        .map_err(|e| format!("{e:#}"))
}

/// Rejects an album: removes it from `curated.json` and marks it in the history.
fn reject_curated(files: &CuratedFiles, uri: &str) -> app::Event {
    match files.reject(uri) {
        Ok(curated) => {
            app::Event::CuratedChanged(curated.map(|curated| curated.to_list(Timestamp::now())))
        }
        Err(e) => app::Event::Error(format!("{e:#}")),
    }
}

/// Reads terminal input on its own thread. `None` means that the window was
/// resized.
fn spawn_input_reader() -> UnboundedReceiver<Option<Key>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(input) = event::read() {
            let message = match input {
                event::Event::Key(key_event) => match key(key_event) {
                    Some(key) => Some(key),
                    None => continue,
                },
                event::Event::Resize(..) => None,
                _ => continue,
            };
            if sender.send(message).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Converts a crossterm key into an `app::Key`. Ctrl letters are lowercase.
fn key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let key = match event.code {
        KeyCode::Char(c) if event.modifiers.contains(KeyModifiers::CONTROL) => {
            Key::Ctrl(c.to_ascii_lowercase())
        }
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    };
    Some(key)
}

/// The log goes to `~/.cache/deck/deck.log`, because stderr would mess up the UI.
/// The level can be set with `RUST_LOG`. The log of the previous start is kept as
/// `deck.log.1`, so the cause of a crash can still be seen after a restart.
fn init_logging() {
    let file = config::private_cache_dir().and_then(|dir| open_log(&dir.join("deck.log")));
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"));
    match file {
        Ok(file) => builder.target(env_logger::Target::Pipe(Box::new(file))),
        Err(_) => builder.filter_level(log::LevelFilter::Off),
    };
    builder.init();
}

/// Moves the previous log to `<log>.1` (over the older one) and opens a new one.
fn open_log(path: &std::path::Path) -> Result<std::fs::File> {
    let mut previous = path.as_os_str().to_owned();
    previous.push(".1");
    match std::fs::rename(path, &previous) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(anyhow::Error::from(e).context(format!("cannot rotate {}", path.display())));
        }
        _ => {}
    }
    std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))
}

/// Subcommands log to stderr, so the log of a running Deck stays intact.
fn init_stderr_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::app::View;

    /// Example `curated.json`: two albums with their reasons.
    const CURATED_JSON: &str = r##"{
  "round": "2026-W41",
  "created": "2026-10-05T07:00:00+03:00",
  "albums": [
    { "uri": "spotify:album:noother", "name": "No Other", "artist": "Gene Clark",
      "artist_id": "gc", "year": 1974,
      "reason": "Cosmic country-rock that Teenage Fanclub grew up on." },
    { "uri": "spotify:album:number1", "name": "#1 Record", "artist": "Big Star",
      "artist_id": "bs", "year": 1972, "reason": "Power pop at its source." }
  ]
}"##;

    const HISTORY_JSON: &str = r##"[
  { "uri": "spotify:album:noother", "name": "No Other", "artist": "Gene Clark",
    "round": "2026-W41", "rejected": false },
  { "uri": "spotify:album:number1", "name": "#1 Record", "artist": "Big Star",
    "round": "2026-W41", "rejected": false }
]"##;

    fn screen(state: &State) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(70, 17)).unwrap();
        terminal
            .draw(|frame| ui::draw(frame, state, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn curated_file_shows_on_shelf_and_ctrl_d_rejects_into_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("curated.json"), CURATED_JSON).unwrap();
        std::fs::write(dir.path().join("curated-history.json"), HISTORY_JSON).unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        let now = Instant::now();
        let shelf = vec![catalog::Album {
            id: "bleach".to_owned(),
            uri: "spotify:album:bleach".to_owned(),
            name: "Bleach".to_owned(),
            artist: "Nirvana".to_owned(),
            artist_id: "nirvana".to_owned(),
            year: Some(1989),
        }];
        let mut state = State::new(shelf).with_curated(curated_list(&files));

        // Shelf: list rows, an empty row and the artists.
        let rows = screen(&state);
        assert_eq!(
            rows[4],
            format!("✦ Curated{}2 albums · week 41", " ".repeat(43))
        );
        assert!(rows[5].starts_with("≋ Genres"));
        assert_eq!(rows[6], "");
        assert_eq!(rows[7], "Nirvana");

        // Enter opens the list and reads the file again.
        let actions = state.update(app::Event::Key(Key::Enter), now);
        assert_eq!(actions, [Action::LoadLists]);
        state.update(app::Event::ListsLoaded(curated_list(&files)), now);
        let rows = screen(&state);
        assert_eq!(rows[4], "Curated");
        assert_eq!(rows[7], "Gene Clark · No Other  1974");
        assert_eq!(rows[8], "Big Star · #1 Record  1972");
        assert_eq!(
            rows[12],
            "Cosmic country-rock that Teenage Fanclub grew up on."
        );

        // The reason follows the selection.
        state.update(app::Event::Key(Key::Down), now);
        assert_eq!(screen(&state)[12], "Power pop at its source.");

        // Ctrl+D removes the album from the list and the file and marks it in the history.
        let actions = state.update(app::Event::Key(Key::Ctrl('d')), now);
        assert_eq!(
            actions,
            [Action::RejectCurated("spotify:album:number1".to_owned())]
        );
        let event = reject_curated(&files, "spotify:album:number1");
        state.update(event, now);
        let rows = screen(&state);
        assert_eq!(rows[5], "1 album · week 41");
        assert_eq!(rows[7], "Gene Clark · No Other  1974");
        assert_eq!(rows[8], "");
        assert!(matches!(state.view(), View::Curated { selected: 0, .. }));
        let saved = files.load().unwrap().unwrap();
        assert_eq!(saved.albums.len(), 1);
        assert_eq!(saved.albums[0].album.name, "No Other");
        let history = files.history().unwrap();
        assert!(!history[0].rejected);
        assert!(history[1].rejected);
    }

    #[test]
    fn broken_or_missing_curated_file() {
        let dir = tempfile::tempdir().unwrap();
        let files = CuratedFiles::in_dir(dir.path());
        assert_eq!(curated_list(&files), Ok(None));

        std::fs::write(dir.path().join("curated.json"), "{").unwrap();
        let error = curated_list(&files).unwrap_err();
        assert!(error.contains("is broken"), "{error}");
        // Rejecting does not overwrite a broken file.
        assert!(matches!(
            reject_curated(&files, "spotify:album:x"),
            app::Event::Error(_)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("curated.json")).unwrap(),
            "{"
        );
    }

    #[test]
    fn saved_playback_shows_stopped_track_with_queue_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("playback.json");
        std::fs::write(
            &path,
            r#"{
  "album": { "uri": "spotify:album:nevermind", "len": 12, "index": 1 },
  "current": "album",
  "queue": [
    { "uri": "spotify:track:comeasyouare", "name": "Come As You Are", "artist": "Nirvana",
      "duration_ms": 218000, "album_uri": "spotify:album:nevermind", "index": 2 },
    { "uri": "spotify:track:breed", "name": "Breed", "artist": "Nirvana",
      "duration_ms": 183000, "album_uri": "spotify:album:nevermind", "index": 3 }
  ],
  "now_playing": { "artist": "Nirvana", "album": "Nevermind", "track": "In Bloom",
    "duration_ms": 254000 },
  "position_ms": 127000
}"#,
        )
        .unwrap();
        let file = PlaybackFile::at(path);
        let now = Instant::now();
        let mut state = State::new(Vec::new());
        let saved = file.load().unwrap().unwrap();
        let actions = state.update(app::Event::Restore(Box::new(saved)), now);
        assert!(matches!(actions.as_slice(), [Action::Restore { .. }]));

        let rows = screen(&state);
        assert_eq!(
            rows[0],
            format!("DECK{}+2 queued   ■ stopped", " ".repeat(45))
        );
        assert_eq!(
            rows[1],
            format!("Nirvana / Nevermind / In Bloom{}02:07", " ".repeat(35))
        );
        // The bar's ● is halfway (127 s / 254 s).
        assert_eq!(rows[2].chars().position(|c| c == '●'), Some(34));

        // On quit the state is saved unchanged, even though time passes.
        let later = now + Duration::from_secs(10);
        file.save(state.playback(later).as_ref()).unwrap();
        assert_eq!(file.load().unwrap(), state.playback(now));
        assert_eq!(state.playback(later).unwrap(), state.playback(now).unwrap());
    }

    #[test]
    fn log_of_the_previous_start_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.log");
        std::fs::write(&path, "oldest").unwrap();
        open_log(&path).unwrap();
        std::fs::write(&path, "previous").unwrap();
        open_log(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("deck.log.1")).unwrap(),
            "previous"
        );
        // The first time there is no previous log.
        let fresh = dir.path().join("new.log");
        open_log(&fresh).unwrap();
        assert!(fresh.exists());
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
        key(KeyEvent::new(code, modifiers))
    }

    fn mode(args: &[&str]) -> Option<Mode> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).ok()
    }

    fn usage(args: &[&str]) -> Option<&'static str> {
        Mode::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).err()
    }

    #[test]
    fn modes_from_arguments() {
        assert_eq!(mode(&[]), Some(Mode::Deck));
        assert_eq!(mode(&["taste"]), Some(Mode::Taste));
        assert_eq!(mode(&["curate"]), Some(Mode::Curate));
        assert_eq!(mode(&["now-playing"]), Some(Mode::NowPlaying));
        assert_eq!(
            mode(&["curate", "submit"]),
            Some(Mode::CurateSubmit { dry_run: false })
        );
        assert_eq!(
            mode(&["curate", "submit", "--dry-run"]),
            Some(Mode::CurateSubmit { dry_run: true })
        );
        assert_eq!(mode(&["curate", "--dry-run"]), None);
        assert_eq!(mode(&["curate", "submit", "--wet"]), None);
        assert_eq!(mode(&["taste", "--dry-run"]), None);
        assert_eq!(mode(&["play"]), None);
        assert_eq!(
            mode(&["genre", "add", "--dry-run"]),
            Some(Mode::Genre(GenreCommand::Add { dry_run: true }))
        );
        assert_eq!(
            mode(&["genre", "add"]),
            Some(Mode::Genre(GenreCommand::Add { dry_run: false }))
        );
        assert_eq!(
            mode(&["genre", "remove", "60s", "rock"]),
            Some(Mode::Genre(GenreCommand::Remove("60s rock".into())))
        );
        assert_eq!(mode(&["genre", "remove"]), None);
        assert_eq!(
            mode(&["genre", "list"]),
            Some(Mode::Genre(GenreCommand::List))
        );
        assert_eq!(
            mode(&["genre", "prompt"]),
            Some(Mode::Genre(GenreCommand::Prompt))
        );
        assert_eq!(mode(&["genre"]), None);
        assert_eq!(mode(&["--help"]), Some(Mode::Help));
        assert_eq!(mode(&["-h"]), Some(Mode::Help));
        assert_eq!(mode(&["--version"]), Some(Mode::Version));
        assert_eq!(mode(&["-V"]), Some(Mode::Version));
        assert_eq!(mode(&["--help", "taste"]), None);
    }

    #[test]
    fn genre_commands_get_genre_usage() {
        assert_eq!(usage(&["genre"]), Some(genre_cli::USAGE));
        assert_eq!(usage(&["genre", "play"]), Some(genre_cli::USAGE));
        assert_eq!(usage(&["genre", "remove"]), Some(genre_cli::USAGE));
        assert_eq!(usage(&["genre", "add", "--wet"]), Some(genre_cli::USAGE));
        assert_eq!(usage(&["play"]), Some(USAGE));
        assert_eq!(usage(&["genre", "list"]), None);
    }

    #[test]
    fn keys_convert_to_app_keys() {
        assert_eq!(
            press(KeyCode::Char('f'), KeyModifiers::CONTROL),
            Some(Key::Ctrl('f'))
        );
        assert_eq!(
            press(
                KeyCode::Char('A'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            Some(Key::Ctrl('a'))
        );
        assert_eq!(
            press(KeyCode::Char('Ä'), KeyModifiers::SHIFT),
            Some(Key::Char('Ä'))
        );
        assert_eq!(
            press(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(Key::Char(' '))
        );
        assert_eq!(press(KeyCode::Enter, KeyModifiers::NONE), Some(Key::Enter));
        assert_eq!(press(KeyCode::F(1), KeyModifiers::NONE), None);

        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(key(release), None);
    }
}
