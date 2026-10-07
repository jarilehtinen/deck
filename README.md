# Deck

Deck is a minimal Spotify player for the terminal on macOS. It plays music itself (you do
not need the Spotify app open), and it is built around your own **album shelf**: a
hand-picked collection of records instead of playlists and endless feeds.

```
DECK
Gene Clark / No Other / Life's Greatest Fool ♥                                 01:42
─────────────────────────────●──────────────────────────────────────────────────────

✦ Curated                                                        20 albums · week 41
≋ lofi                                                                         radio
≋ synthwave                                                                    radio
≋ Genres                                                                   26 genres

Big Star
Foo Fighters
Gene Clark
Nirvana
Teenage Fanclub

────────────────────────────────────────────────────────────────────────────────────
[Space] pause  [←→] track  [l] unlike playing  [f] search  [u] show queue  [q] quit
```

The top rows show what is playing and how far along it is. Below them are your lists:
**Curated**, your favourite **genre radios** and all **Genres**, and then the artists on
your shelf. The bottom row always shows the keys you can use in the current view.

## Curated: 20 new albums every week, each with a reason

Curated is the heart of Deck. Every Monday it puts together a list of 20 albums you have
not heard yet, picked for your taste. **Every album comes with a short reason** – why
this record is on the list for _you_ – so choosing what to play next is easy:

```
Curated
20 albums · week 41

Gene Clark · No Other  1974
Big Star · #1 Record  1972
Judee Sill · Heart Food  1973
The dB's · Repercussion  1982
Teenage Fanclub · Grand Prix  1995 ●
…

Cosmic country-rock that Teenage Fanclub and The Posies grew up on, overlooked when
it came out, and a record you return to for decades.

────────────────────────────────────────────────────────────────────────────────────
[e] add to queue  [a] add to shelf  [d] not for me  [l] unlike playing  [Esc] back
```

The reason of the selected album is shown at the bottom, and also above the track list
when you open the album.

The list is based on:

- your **Last.fm listening history** (most played artists of all time and of the last 3
  years and 12 months, and most played albums),
- your **shelf** (the records you value most),
- **earlier lists**: what was suggested before, what you put on your shelf and what you
  dismissed.

Albums you already know (3 or more plays on Last.fm), albums on your shelf and earlier
suggestions never appear again. Deck learns as you go: **adding an album to your shelf**
tells the next list that you liked it, and **`d` (not for me)** removes the album and tells
the next list to avoid records like it.

The list is put together by [Claude Code](https://claude.com/claude-code) following the
instructions in [`curate/prompt.md`](curate/prompt.md), and Deck checks every suggestion
against Spotify. Setting up Curated is optional, but recommended: see
[Setting up Curated](#setting-up-curated-optional-recommended). Everything else in Deck
works without it.

## Other features

- **Shelf:** your albums, grouped by artist. Add any album with `a`.
- **Search** for artists and albums.
- **Queue:** add tracks or whole albums; the queue plays right after the current track.
- **Genre radio:** endless music from a genre (lofi, synthwave, shoegaze, 60s rock…),
  and you can add [your own genres](#your-own-genres).
- **Like** the playing track with `l`. It goes to your Liked Songs in Spotify (♥).
- **Media keys and Now Playing:** the Mac's ⏯ ⏮ ⏭ keys control Deck, and the playing
  track shows in Control Center and on the lock screen with its cover.
- Deck remembers what was playing when you quit, and `Space` continues from there.

## Requirements

- **Spotify Premium.** Spotify only lets Premium accounts play music in third-party
  players.
- **macOS.**
- **Rust**, to build Deck from source.
- For Curated (optional): a free **Last.fm account** that has been recording what you
  listen to, and **Claude Code**.

## Installation

### 1. Install the tools

Install Apple's command line tools (they include the compiler Rust needs) and Rust:

```sh
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the instructions of the Rust installer, then open a new terminal window so that
`cargo` is found. If `xcode-select` says the tools are already installed, that is fine.

### 2. Create your own Spotify app

Deck uses Spotify's Web API for search and likes, and Spotify requires every user of the
Web API to have their own "app". It takes a couple of minutes and is free:

1. Go to [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) and
   log in with your Spotify account. Accept the developer terms if asked.
2. Click **Create app** and fill in:
   - **App name** and **App description:** anything, for example "Deck".
   - **Redirect URIs:** exactly `http://127.0.0.1:8898/login` – click **Add** after
     typing it. (Use `127.0.0.1`, not `localhost`.)
   - **Which API/SDKs are you planning to use?** Check **Web API**.
3. Accept the terms and click **Save**.
4. Open the app's **Settings** and copy the **Client ID**. You do not need the client
   secret.

The app is in "development mode", which is all Deck needs. Spotify requires the app's
owner to have Premium. Only the account that created the app can sign in to it, so create
the app with the same Spotify account you play music with. (To let up to five other
people use your app, add them under **User Management**. It is simpler for each person to
create their own app.)

**Note:** Spotify blocks requests for many hours (about 10) after too many of them. The
limit is shared by all the apps of one developer account, so a second app does not help:
if you experiment with Deck's code, avoid loops that call the Web API.

### 3. Write the config file

Create the file `~/.config/deck/config.toml` and put your Client ID in it:

```sh
mkdir -p ~/.config/deck
echo 'client_id = "PASTE-YOUR-CLIENT-ID-HERE"' > ~/.config/deck/config.toml
```

Replace `PASTE-YOUR-CLIENT-ID-HERE` with the Client ID, keeping the quotes.

Deck plays at 160 kbps (Ogg Vorbis) by default. For the best sound quality, add a line
with `bitrate = 320` (the allowed values are 96, 160 and 320):

```sh
echo 'bitrate = 320' >> ~/.config/deck/config.toml
```

### 4. Build and install Deck

```sh
git clone https://github.com/jarilehtinen/deck.git
cd deck
cargo install --locked --path .
```

The first build takes a few minutes. It installs the `deck` command in `~/.cargo/bin`,
which the Rust installer added to your `PATH`.

**Why `--locked`:** it makes Cargo use exactly the library versions listed in
`Cargo.lock`. Deck's Spotify library (librespot 0.8) does not build with some newer
versions of its own dependencies, so without `--locked` the build can fail.

Keep the `deck` folder: Curated's weekly job is installed from there. To update Deck
later, run `git pull` and the same `cargo install` command in it. `deck --help` lists the
commands
and `deck --version` shows the installed version.

## First start

Run:

```sh
deck
```

The first time, your browser opens **twice**:

1. **"Sign in to Spotify"** – this is for playing music. Log in and accept.
2. **"Sign in to the Deck Spotify app"** – this is your own app from step 2, for search
   and likes. Spotify asks you to allow access to your library: Deck needs it to read and
   change your Liked Songs.

After each one the browser says "Deck is signed in", and you can close the tab. Deck
saves the sign-ins in `~/.cache/deck/`, so next time it starts straight away. If signing
in fails, Deck tells you why and quits.

Your shelf is empty at first: press `f`, search for an album, and press `a` to add it.

## Using Deck

### Keys

The bottom row of every view shows the most useful keys. All of them:

| Key | What it does |
|---|---|
| ↑ ↓ | Move the selection |
| Enter | Open the selected artist, list or album. In a track list: play the album from the selected track. On a genre: start its radio. In a radio: play from the selected track |
| Space | Play / pause |
| ← → | Previous / next track |
| `f` or Ctrl+F | Search |
| `n` or Ctrl+N | Now playing: go to the album (or radio) that is playing, with the playing track selected |
| `l` or Ctrl+L | Like the playing track, or unlike it if it is already liked |
| `e` or Ctrl+E | Add to queue: the selected track in a track list or radio, the whole album in album lists |
| `a` or Ctrl+A | Add the album to your shelf, or remove it if it is already there. In a radio: the playing track's album. In Genres: add the genre to the home view, or remove it |
| `d` or Ctrl+D | Remove: an album from your shelf (in an artist opened from the shelf), a track from the queue, an album from Curated (not for me) or a genre from the home view |
| `r` or Ctrl+R | In a radio: start a new radio of the same genre |
| `u` or Ctrl+Q | Show the queue |
| Esc | Back |
| `q` | Quit (in the search field, use Ctrl+C) |
| Ctrl+C | Quit, in any view |

In the search field, letters type into the search, so use the Ctrl versions there. The
Ctrl versions work everywhere else too. `l` and `n` always concern the **playing** track, not
the selected row.

### The screen

- **Top row:** `DECK`, and on the right the queue length (`+2 queued`), short
  confirmations (`+ queued: Nevermind`, `♥ liked: Everlong`) and `■ stopped` when paused.
  Errors are shown on the second row in red.
- **Second row:** artist / album / track of the playing track, ♥ if it is liked, and the
  elapsed time.
- **Third row:** progress bar.

### Shelf and albums

The home view lists the artists on your shelf in alphabetical order, ignoring a leading
"The" and accents. With a Finnish or Swedish locale (`LC_ALL`, `LC_COLLATE` or `LANG`
starting with `fi` or `sv`), å, ä and ö sort after z as their own letters. Enter on an
artist shows their albums on your shelf, oldest first, and Enter on an album opens its
track list without starting playback. In the
track list, Enter plays the album from the selected track to the end, and the playing
track is marked with ♪. When the album ends, playback stops.

Albums on your shelf have a green ● after them in every list.

### Search

Type to search; the search starts when you pause typing. ← → move the cursor. Results are
grouped into artists and albums. ↓ or Enter moves from the search field to the results,
where the keys work as in any other list: Enter opens an artist or album, `a` adds an
album to your shelf and `e` adds it to the queue. ↑ on the first result or `f` goes back
to the search field. Esc goes back from search.

### Queue

The queue plays right after the current track, like in Spotify, and when it is empty the
album continues where it left off. If nothing is playing, adding to the queue starts it.
`u` shows the queue, where `d` removes a track.

### Genres and genre radio

A genre plays as a radio: an endless stream of tracks from different artists and albums.
Your favourite genres are on the home view under Curated, and `≋ Genres` at the end lists
all genres that come with Deck (26) plus your own. Favourites have a ★ in that list.

- Enter on a genre starts its radio and opens the radio view. If the genre is already
  playing, Enter just opens the view. The playing genre has `♪ playing` on the home view.
- In Genres, `a` adds a genre to the home view or removes it. On the home view, `d`
  removes it.
- In the radio view, Enter plays from the selected track, `e` adds the track to the
  queue, `a` adds the track's album to your shelf and `r` starts a new radio of the same
  genre.

The radio uses Spotify's radio stations, starting from a typical artist or track of the
genre. When a station starts repeating itself, the radio continues seamlessly from
another starting point. Compilation albums go on the shelf under their album artist,
usually "Various Artists".

### Your own genres

You can add genres with the help of an AI assistant such as Claude Code. Ask it to run
`deck genre prompt` and to add a genre, for example "50s rock". The instructions tell it
to choose about ten typical artists or tracks and to find their Spotify links. Deck checks
each one and only saves the genre if at least three of them work.

- `deck genre add [--dry-run] < genre.json` checks the genre and prints a report. Without
  `--dry-run` it saves the genre. Run it without input to see an example `genre.json`.
- `deck genre remove <name>` removes your genre.
- `deck genre list` lists all genres. Yours have `"own": true`.

Your genres are in `~/.config/deck/my-genres.json`, and they appear in Genres the next time
you start Deck. A genre of your own with the same name as a built-in genre replaces it,
and removing yours brings the built-in one back.

### Likes

`l` likes the playing track: it is saved to your Liked Songs in Spotify and gets a ♥.
Pressing `l` again removes the like. Liked tracks have a ♥ in track lists and the radio,
too. Likes need your own Spotify app (`client_id` in `config.toml`).

### Media keys and Now Playing

The Mac's media keys control Deck in any view: ⏯ works like Space and ⏮ ⏭ like ← →. The
playing track shows in Control Center's Now Playing and on the lock screen. macOS gives
the media keys to the app that started playing most recently: if Spotify or Music plays
after Deck, the keys go to it, and they come back when Deck plays again. For this, Deck
starts a small helper process (`deck now-playing`) that quits with Deck.

## Setting up Curated (optional, recommended)

Curated is put together once a week by Claude Code. You need:

- **A Last.fm account** with your listening history. Spotify can record your listening
  there: in Last.fm, go to **Settings → Applications** and connect Spotify. The more
  history there is, the better the list.
- **A Last.fm API key:** create one at
  [last.fm/api/account/create](https://www.last.fm/api/account/create) (any application
  name will do; the callback URL can be left empty). Copy the **API key**.
- **[Claude Code](https://claude.com/claude-code)**, installed and signed in, so that the
  `claude` command works in the terminal. Each weekly run uses your Claude plan or API
  credits like any other Claude Code session.
`deck` and `claude` must both be found in your terminal's `PATH` (check with
`command -v deck claude`).

### 1. Add Last.fm to the config file

Add two lines to `~/.config/deck/config.toml`:

```toml
lastfm_api_key = "your Last.fm API key"
lastfm_user = "your Last.fm username"
```

Check that it works (this prints your taste profile as JSON):

```sh
deck taste
```

If something is missing, the error tells you what to add.

### 2. Make the first list

Run Deck at least once before this (the sign-ins from [First start](#first-start)), then
run:

```sh
deck curate
```

It takes a few minutes. The progress is written to the terminal and to
`~/.cache/deck/curate.log`. You can run it again any time to replace the list with a new
one. When it is done, `✦ Curated` appears on Deck's home view.

### 3. Make it run every week

```sh
scripts/install-curate.sh
```

Run this in the `deck` folder. It installs a background job (a launchd agent) that runs
`deck curate` every Monday at 9:00. If your Mac is asleep at that time, the list is made
when it wakes up; if it is shut down, that week is skipped. Background jobs do not get
your terminal's `PATH`, so the installer looks up where `deck` and `claude` are and
writes those folders into the job; it stops with an error if one of them is missing. Run
`scripts/install-curate.sh` again if `deck` or `claude` moves (for example after
reinstalling Claude Code another way). If the job cannot find `claude`,
`~/.cache/deck/curate.log` says `claude not found`. To remove the job:

```sh
scripts/install-curate.sh --uninstall
```

### How Curated works

`deck curate` runs Claude Code with the instructions in `curate/prompt.md` (built into
Deck). Claude can only run two Deck commands, write candidate files in
`~/.cache/deck/curate/` and search the web:

- `deck taste` prints your taste profile: Last.fm history, shelf and earlier lists.
- `deck curate submit [--dry-run]` reads album candidates with reasons, finds them on
  Spotify and reports which ones are accepted and why others are not (not on Spotify, on
  your shelf, suggested before, already listened to). Without `--dry-run` it saves the
  list.

Claude picks about 24 candidates, checks them, replaces the rejected ones and saves the
best 20. If the run fails, the previous list stays.

Spotify blocks apps that make too many Web API requests for many hours, which would also
stop Deck's search. So `deck curate submit` is careful: it makes at most **30 Spotify
searches a day**, remembers searches for a day, and stops at once when Spotify says "too
many requests" (429), making no more searches until the block has passed. The 30
searches are shared with `deck genre add`, and the block with Deck itself: whichever of
them meets it first, the others wait too.

## Files

| Path | What is there |
|---|---|
| `~/.config/deck/config.toml` | Settings: `client_id`, `bitrate`, `lastfm_api_key`, `lastfm_user` (other keys are an error, so a typo does not go unnoticed) |
| `~/.config/deck/shelf.json` | Your shelf |
| `~/.config/deck/genres.json` | Your favourite genres on the home view |
| `~/.config/deck/my-genres.json` | Your own genres |
| `~/.config/deck/curated.json` | This week's Curated list |
| `~/.config/deck/curated-history.json` | Everything Curated has suggested |
| `~/.cache/deck/` | Sign-ins (`credentials.json`, `webapi.json`), Deck's log `deck.log` (the previous run's log is `deck.log.1`), Curated's log `curate.log` and the caches below |
| `~/.cache/deck/curate-searches.json` | Spotify searches of the last day by `deck curate submit` and `deck genre add`, and the daily search count |
| `~/.cache/deck/rate-limit.json` | A possible 429 block from Spotify, shared by Deck, `deck curate submit` and `deck genre add` |
| `~/.cache/deck/lastfm-albums.json` | Albums you have listened to on Last.fm, fetched again after a day |
| `~/.cache/deck/curate/` | The weekly Curated run's working folder: Claude's candidate files from the latest run |
| `~/.local/state/deck/playback.json` | What was playing when you quit |

Deck writes its files safely (a crash never leaves half a file). If one of the files in
`~/.config/deck/` is broken, for example after editing it by hand, Deck shows an error
and does not overwrite it, so you can fix it. You can edit the shelf and the favourite
genres while Deck is open: Deck reads the file again before it changes it, so your
edits are kept.

## Troubleshooting

- **Something went wrong:** look at the log, `~/.cache/deck/deck.log`, or after a crash
  and restart the previous run's log, `~/.cache/deck/deck.log.1`. For more detail, start
  Deck with `RUST_LOG=deck=debug deck`.
- **Signing in does not work, or you want to sign in with another account:** delete the
  saved sign-ins and start Deck again:

  ```sh
  rm ~/.cache/deck/credentials.json ~/.cache/deck/webapi.json
  ```

- **The browser says the redirect URI is invalid:** check that your Spotify app has
  exactly `http://127.0.0.1:8898/login` as a Redirect URI and that `client_id` in
  `config.toml` is that app's Client ID.
- **Search or likes fail with 429 / "rate limiting":** Spotify has blocked your app for
  too many requests. It passes by itself, usually within some hours. Playback and genre
  radio keep working meanwhile.
- **Curated did not update:** see `~/.cache/deck/curate.log`. If the background job did
  not start at all, the error is in `~/Library/Logs/deck-curate.log`.
- **The build fails:** make sure you used `cargo install --locked --path .` and that
  `xcode-select --install` has been run.

## Notes

Deck is not an official Spotify app and is not affiliated with Spotify. It plays music
with [librespot](https://github.com/librespot-org/librespot), an open-source Spotify
client library, and uses some of Spotify's internal interfaces (for example for the
radio). Spotify may change them at any time, which can break features until Deck is
updated.

Inspired by [cliamp](https://github.com/bjarneo/cliamp) by Bjarne Øverli.

## License

[MIT](LICENSE)
