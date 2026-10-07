# CLAUDE.md

Deck is a terminal Spotify player for macOS written in Rust: ratatui for the UI,
librespot for sign-in and playback, Spotify's Web API for search and likes. See
`README.md` for what it does from the user's point of view.

## Build and test

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

All four must pass before a commit.

- Always pass `--locked`. `Cargo.lock` pins `vergen` to 9.0.6 because librespot 0.8 does
  not build with newer versions.
- **Never run `cargo update`** (not even for a single crate without checking the result):
  it breaks the build. Add dependencies with an explicit version and check that
  `Cargo.lock` only gained what you added.
- Tests must not touch the network or the real `~/.config`, `~/.cache` or
  `~/.local/state`. Use `tempfile` for files and fake responses (see
  `tests/fixtures/`) for Spotify. Tests that need a real Spotify account are `#[ignore]`d.
- Spotify blocks the whole app for hours after too many Web API requests (429). Do not
  write loops or experiments that call the Web API repeatedly.

## Architecture

- **`app`**: all state and logic. `State::update(Event, Instant) -> Vec<Action>` is pure:
  it changes the state and returns what should happen (play, search, save the shelf…),
  but does no IO itself. Events are key presses, player events, results of earlier
  actions and ticks. Nearly all behaviour is tested through `update`.
- **`main`**: the event loop and all IO. It reads keys and player events, calls
  `State::update`, carries out each `Action` (often in a tokio task that sends the result
  back as an `Event`) and draws. It also parses the subcommands (`deck taste`,
  `deck curate`, `deck curate submit`, `deck genre …`, and the internal
  `deck now-playing`).
- **`ui`**: draws the state with ratatui. No logic: decisions belong in `app`. The key
  help rows are in `key_help`.

Other modules:

| Module | What it does |
|---|---|
| `player` | Sign-in and playback with librespot (`Player` takes commands, sends events), and the Spotify session shared with `radio` and `catalog` (`SharedSession`) |
| `catalog` | Spotify Web API: search, artist albums, album tracks, Liked Songs |
| `radio` | Genre radio pages from Spotify's radio stations (apollo) |
| `genres` | The built-in genre catalog (`genres/genres.json`, compiled in), favourites and the user's own genres |
| `genre_cli` | `deck genre add / remove / list / prompt` |
| `shelf` | The album shelf (`~/.config/deck/shelf.json`) |
| `lists` | The Curated list and its history |
| `curate` | `deck taste` and `deck curate submit`, used by the weekly Curated run |
| `curate_run` | `deck curate`: the Curated run, which starts Claude Code with `curate/prompt.md` and logs to `~/.cache/deck/curate.log` |
| `lastfm` | Last.fm API |
| `playback` | Playback state across restarts (`~/.local/state/deck/playback.json`) |
| `now_playing` | macOS Now Playing and media keys, run in a helper process |
| `config` | `~/.config/deck/config.toml`, directory paths and the shared OAuth client |
| `rate_limit` | Spotify's 429 block shared by all Deck processes (`~/.cache/deck/rate-limit.json`) |
| `store` | Atomic JSON read and write shared by the modules that keep files |

Curated is built outside the app: `deck curate` runs Claude Code with `curate/prompt.md`
(compiled in), and Claude calls `deck taste` and `deck curate submit`.
`scripts/install-curate.sh` installs a launchd job that runs `deck curate` every Monday.
`genres/prompt.md` is the corresponding prompt for adding genres, printed by
`deck genre prompt`.

## Conventions

- **Everything the user sees is in English**: UI text, errors, command output, README.
- **The UI fits in 100 columns.** Deck is at most 100 columns wide; every help row must
  fit (there is a test for it). If a help row is too long, `[q] quit` is the first to go.
- **Code, comments and doc comments are in English**, like everything else in the repo.
- **Save files atomically** with `store::save_json` (or `store::write_atomic`): it writes a
  uniquely named temp file next to the target and renames it over the target, so a crash
  never leaves half a file and two processes (Deck and `deck curate submit`) never clash.
- **Never overwrite a broken file.** If a user file (shelf, genres, Curated…) cannot be
  parsed, show the error and leave the file alone, so the user can fix it by hand. A
  missing file is not an error: it means empty. `store::read_json` does both. Deck's own
  state and caches (`playback.json`, `webapi.json`, `rate-limit.json`, the search cache)
  are the exception: a broken one is logged and replaced.
- **Reread a user file before changing it** (see `Shelf::add`), so that edits made by
  hand while Deck is open are not overwritten.
- **Secrets are owner-only**: `~/.cache/deck` is 0700 and sign-in files are 0600.
- Match the surrounding code: small functions, `anyhow` errors with context
  (`cannot open <path>`), and tests next to the code in `mod tests`.
- Update `README.md` when keys, commands, files or setup change. The key table in the
  README must match `key_help` and the key handling in `app`.
