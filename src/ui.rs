//! Draws the app state with ratatui. No logic: decisions are made in `app`.
//!
//! The top (title, queue length and pause mark, now playing or an error, progress
//! bar) is always visible. Below it is the current view and on the bottom row the
//! view's key help. There is always an empty row between the view and the help row:
//! a long list scrolls and never reaches the help row.
//!
//! The UI text is in English.
//!
//! Everything is drawn in Deck's area, which is at most `MAX_WIDTH` columns wide and
//! `MAX_HEIGHT` rows high and centred in the window. In a small window it fills the window.
//!
//! The colours are fixed RGB colours that stand out on both dark and light
//! backgrounds; the selected row sets both the text and the background colour, so it
//! does not depend on the terminal theme.

use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::{
        AlbumView, ArtistView, NoticeKind, QueuedTrack, RadioView, Scroll, SearchItem, SearchView,
        ShelfRow, Source, State, View,
    },
    catalog::Album,
    genres,
    lists::{AlbumList, ListAlbum},
    radio,
};

const SEARCH_PROMPT: &str = "Search: ";

/// Mark after the name of a liked track.
const LIKED: &str = " ♥";

/// Mark of the list rows above the shelf.
const LIST_MARK: &str = "✦";
/// Mark of a genre (radio) row.
const GENRE_MARK: &str = "≋";

/// Maximum height of the reason in the list view.
const REASON_LINES: usize = 2;
/// Maximum height of the reason under the track list heading.
const ALBUM_REASON_LINES: usize = 3;
/// After a stale list's subtitle: why the weekly update did not happen.
const STALE_HINT: &str = " – see ~/.cache/deck/curate.log";

/// Accent colour: DECK, keys, progress bar, search field underline, the playing
/// track's ♪, the liked ♥, the queue's + and the pause ■.
const ACCENT: Color = Color::Rgb(0xD0, 0x7A, 0x10);
/// Secondary text: labels, years, durations, reasons. Its contrast on a typical dark
/// terminal background is over 4.5:1, and it stays clearly dimmer than normal
/// text.
const MUTED: Color = Color::Rgb(0x8A, 0x8F, 0x99);
/// The remaining part of the progress bar and the rule above the help row: darker
/// than text, so that the full-width rule does not stand out.
const RULE: Color = Color::Rgb(0x4A, 0x4E, 0x58);
/// Group headings of the search results (Artists, Albums).
const HEADING: Color = Color::Rgb(0x6B, 0x80, 0xA0);
/// Subtitle of a stale list: the weekly update has been missed.
const WARNING: Color = Color::Rgb(0xE6, 0xC2, 0x4A);
/// The favourite genre's ★ in the Genres view.
const STAR: Color = Color::Rgb(0xE6, 0xC2, 0x4A);
/// Dim background and light text of the selected row.
const SELECTED_BG: Color = Color::Rgb(0x3A, 0x41, 0x50);
const SELECTED_FG: Color = Color::Rgb(0xF2, 0xF2, 0xF2);
/// Secondary text on the selected row: `MUTED` does not stand out from `SELECTED_BG`.
const SELECTED_MUTED: Color = Color::Rgb(0xA8, 0xB0, 0xC0);

/// Maximum size of Deck's area. A smaller window is used in full.
const MAX_WIDTH: u16 = 100;
const MAX_HEIGHT: u16 = 30;

pub fn draw(frame: &mut Frame, state: &State, now: Instant) {
    let [title, status, bar, _, body, _, rule, help] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(deck_area(frame.area()));

    let view = state.view();
    frame.render_widget("DECK".fg(ACCENT).bold(), title);
    frame.render_widget(title_right(state, now, title.width), title);
    status_line(frame, state, now, status);
    frame.render_widget(progress_bar(state, now, bar.width), bar);

    match view {
        View::Shelf { selected, scroll } => shelf(frame, state, *selected, scroll, body),
        View::Curated { selected, scroll } => curated(frame, state, *selected, scroll, body),
        View::Artist(view) => artist(frame, state, view, body),
        View::Search(view) => search(frame, state, view, body),
        View::Album(view) => album(frame, state, view, body),
        View::Queue { selected, scroll } => queue(frame, state, *selected, scroll, body),
        View::Genres { selected, scroll } => genre_list(frame, state, *selected, scroll, body),
        View::Radio(view) => radio_view(frame, state, view, body),
    }
    frame.render_widget(help_rule(rule.width), rule);
    frame.render_widget(help_line(state, view, help.width), help);
}

/// Deck's area centred in the window.
fn deck_area(window: Rect) -> Rect {
    let width = window.width.min(MAX_WIDTH);
    let height = window.height.min(MAX_HEIGHT);
    Rect {
        x: window.x + (window.width - width) / 2,
        y: window.y + (window.height - height) / 2,
        width,
        height,
    }
}

/// Row 2: an error in red, otherwise artist / album / track on the left and the
/// elapsed time on the right edge. The track name is bold and the separators dim, and
/// a liked track is followed by ♥. A long row is cut before the time, so the time is
/// always visible.
fn status_line(frame: &mut Frame, state: &State, now: Instant, area: Rect) {
    if let Some(error) = &state.error {
        frame.render_widget(Line::from(error.as_str()).red(), area);
        return;
    }
    let Some(playing) = &state.now_playing else {
        frame.render_widget(Line::from("—").fg(MUTED), area);
        return;
    };
    let time = clock(playing.elapsed(now).as_secs());
    let [left, _, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(2),
        Constraint::Length(time.width() as u16),
    ])
    .areas(area);
    let mut track = Line::from(vec![
        playing.artist.as_str().into(),
        " / ".fg(MUTED),
        playing.album.as_str().into(),
        " / ".fg(MUTED),
        playing.track.as_str().bold(),
    ]);
    if state.liked(&playing.uri) {
        track.push_span(LIKED.fg(ACCENT));
    }
    frame.render_widget(track, left);
    frame.render_widget(Line::from(time), right);
}

/// Right edge of the top row: the queue length (`+3 queued`), briefly replaced by a
/// confirmation after queueing or liking (`+ queued: Nevermind`, `♥ liked: Sunset`,
/// `unliked: Sunset`), and `■ stopped` last when paused. Marks in the accent colour,
/// text dimmed. A long name is truncated so that DECK stays visible.
fn title_right(state: &State, now: Instant, width: u16) -> Line<'_> {
    let paused = state
        .now_playing
        .as_ref()
        .is_some_and(|playing| playing.paused);
    let mut spans = Vec::new();
    if let Some((kind, name)) = state.notice(now) {
        let (mark, label) = match kind {
            NoticeKind::Queued => ("+ ", "queued: "),
            NoticeKind::Liked => ("♥ ", "liked: "),
            NoticeKind::Unliked => ("", "unliked: "),
        };
        // "DECK" and a space on the left, the mark, label and pause mark on the right.
        let used = 6 + mark.width() + label.width() + if paused { 12 } else { 0 };
        let name = truncate(name, usize::from(width).saturating_sub(used));
        spans.extend([mark.fg(ACCENT), label.fg(MUTED), name.into()]);
    } else if !state.queue().is_empty() {
        spans.push(format!("+{}", state.queue().len()).fg(ACCENT));
        spans.push(" queued".fg(MUTED));
    }
    if paused {
        if !spans.is_empty() {
            spans.push("   ".into());
        }
        spans.extend(["■".fg(ACCENT), " stopped".fg(MUTED)]);
    }
    Line::from(spans).right_aligned()
}

/// Row 3: the progress bar across Deck's area. When nothing is playing, ● is at the start.
fn progress_bar(state: &State, now: Instant, width: u16) -> Line<'static> {
    let width = usize::from(width);
    if width == 0 {
        return Line::default();
    }
    let ratio = match &state.now_playing {
        Some(playing) if !playing.duration.is_zero() => {
            playing.elapsed(now).as_secs_f64() / playing.duration.as_secs_f64()
        }
        _ => 0.0,
    };
    let head = ((ratio.clamp(0.0, 1.0) * (width - 1) as f64) as usize).min(width - 1);
    Line::from(vec![
        Span::raw("─".repeat(head)).fg(ACCENT),
        Span::raw("●").fg(ACCENT),
        Span::raw("─".repeat(width - head - 1)).fg(RULE),
    ])
}

/// The rule above the help row: the unplayed part of the progress bar without ●, so
/// that the bottom edge mirrors the top.
fn help_rule(width: u16) -> Line<'static> {
    Line::from("─".repeat(usize::from(width))).fg(RULE)
}

/// Shelf: at the top the group of lists (Curated, favourite genres and Genres) and an
/// empty row, below that the artists. The selection moves over the list rows and the
/// artists; the empty row cannot be selected. The list group always has at least the
/// Genres row.
fn shelf(frame: &mut Frame, state: &State, selected: usize, scroll: &Scroll, area: Rect) {
    let lists = state.shelf_lists();
    let width = usize::from(area.width);
    let mut rows: Vec<Line> = lists
        .iter()
        .map(|row| match row {
            ShelfRow::Curated(list) => list_row(list, width),
            ShelfRow::Genre(genre) => {
                let right = if state.playing_genre() == Some(genre.id.as_str()) {
                    vec!["♪".fg(ACCENT), " playing".fg(MUTED)]
                } else {
                    vec!["radio".fg(MUTED)]
                };
                spread(
                    vec![format!("{GENRE_MARK} {}", genre.name).into()],
                    right,
                    width,
                )
            }
            ShelfRow::Genres => spread(
                vec![format!("{GENRE_MARK} Genres").fg(MUTED)],
                vec![count(genres::all().len(), "genre").fg(MUTED)],
                width,
            ),
        })
        .collect();
    rows.push(Line::default());
    let selected_row = if selected < lists.len() {
        selected
    } else {
        selected + rows.len() - lists.len()
    };

    let artists = state.shelf_artists();
    if artists.is_empty() {
        // The hint for an empty shelf is shown below the lists.
        let [top, rest] =
            Layout::vertical([Constraint::Length(rows.len() as u16), Constraint::Min(0)])
                .areas(area);
        lines(frame, rows, Some(selected_row), scroll, top);
        frame.render_widget(
            hint("The shelf is empty – search for albums with Ctrl+F"),
            rest,
        );
        return;
    }
    rows.extend(
        artists
            .iter()
            .map(|artist| Line::from(artist.name.as_str())),
    );
    lines(frame, rows, Some(selected_row), scroll, area);
}

/// "✦ Curated" on the left, the album count and the week dimmed on the right edge.
/// A stale list's subtitle (with its age) is in the warning colour.
fn list_row(list: &AlbumList, width: usize) -> Line<'static> {
    spread(
        vec![format!("{LIST_MARK} {}", list.name).into()],
        vec![list.subtitle.clone().fg(subtitle_color(list))],
        width,
    )
}

/// A row with the left part at the start and the right part at the right edge, with at
/// least two spaces between them.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used: usize = left.iter().chain(&right).map(Span::width).sum();
    left.push(" ".repeat(width.saturating_sub(used).max(2)).into());
    left.extend(right);
    Line::from(left)
}

/// "1 genre", "18 genres".
fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    }
}

/// Genres: all catalog genres in alphabetical order, each favourite followed by a
/// yellow ★.
fn genre_list(frame: &mut Frame, state: &State, selected: usize, scroll: &Scroll, area: Rect) {
    let [title, meta, _, rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    let all = genres::all();
    frame.render_widget("Genres".bold(), title);
    let home = state.favorite_genres().len();
    frame.render_widget(
        Line::from(format!("{} · {home} on home", count(all.len(), "genre"))).fg(MUTED),
        meta,
    );
    let row = |index: usize| {
        let genre = &all[index];
        let mut line = Line::from(genre.name.as_str());
        if state.favorite(&genre.id) {
            line.push_span(" ★".fg(STAR));
        }
        line
    };
    list(frame, all.len(), row, Some(selected), scroll, rows);
}

/// Radio view: the genre and a dim "radio" as the heading, tracks in the order they
/// arrived at the station.
fn radio_view(frame: &mut Frame, state: &State, view: &RadioView, area: Rect) {
    let [title, meta, _, rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    let genre = &view.genre.id;
    frame.render_widget(view.genre.name.as_str().bold(), title);
    frame.render_widget("radio".fg(MUTED), meta);
    let Some(tracks) = state.radio_tracks(genre) else {
        frame.render_widget(hint("The radio has stopped – Ctrl+R starts it again"), rows);
        return;
    };
    let playing = state.playing_radio_track(genre);
    let width = usize::from(rows.width);
    let row = |index: usize| radio_row(state, &tracks[index], playing == Some(index), width);
    list(
        frame,
        tracks.len(),
        row,
        Some(view.selected),
        &view.scroll,
        rows,
    );
}

/// "♪ Artist · Track ♥  Album ●": album dimmed. A long row truncates the album first,
/// then the artist and the track.
fn radio_row(state: &State, track: &radio::Track, playing: bool, width: usize) -> Line<'static> {
    let mark = if playing {
        "♪ ".fg(ACCENT)
    } else {
        "  ".into()
    };
    let on_shelf = state.on_shelf(&track.album.uri);
    let liked = state.liked(&track.uri);
    let room = width
        .saturating_sub(2 + if on_shelf { 2 } else { 0 } + if liked { LIKED.width() } else { 0 });
    let name = truncate(&format!("{} · {}", track.artist, track.name), room);
    let rest = room.saturating_sub(name.width() + 2);
    let mut spans = vec![mark, name.into()];
    if liked {
        spans.push(LIKED.fg(ACCENT));
    }
    // Show the album only if at least its beginning fits.
    if rest > 0 && rest >= track.album.name.width().min(4) {
        spans.push("  ".into());
        spans.push(truncate(&track.album.name, rest).fg(MUTED));
    }
    if on_shelf {
        spans.push(" ●".green());
    }
    Line::from(spans)
}

fn subtitle_color(list: &AlbumList) -> Color {
    if list.stale { WARNING } else { MUTED }
}

/// List view: the list's name and subtitle as the heading, albums in curation order,
/// and the selected album's reason below the list. The list scrolls, the reason stays
/// in place. A stale list's subtitle is followed by a hint about the curation run's log.
fn curated(frame: &mut Frame, state: &State, selected: usize, scroll: &Scroll, area: Rect) {
    let [title, meta, _, rows, _, reason] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(REASON_LINES as u16),
    ])
    .areas(area);

    let shown = state.curated();
    let name = shown.map_or("Curated", |list| list.name.as_str());
    frame.render_widget(name.bold(), title);
    let Some(current) = shown.filter(|list| !list.albums.is_empty()) else {
        frame.render_widget(
            hint("Nothing left – a new list arrives with the next weekly run"),
            rows,
        );
        return;
    };
    let mut subtitle = vec![current.subtitle.as_str().fg(subtitle_color(current))];
    if current.stale {
        subtitle.push(STALE_HINT.fg(MUTED));
    }
    frame.render_widget(Line::from(subtitle), meta);

    let width = usize::from(rows.width);
    let row = |index: usize| list_album_row(state, &current.albums[index], width);
    list(
        frame,
        current.albums.len(),
        row,
        Some(selected),
        scroll,
        rows,
    );

    if let Some(text) = current
        .albums
        .get(selected)
        .and_then(|entry| entry.reason.as_deref())
    {
        let lines = wrap(text, usize::from(reason.width), REASON_LINES);
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()).fg(MUTED),
            reason,
        );
    }
}

/// "Artist · Album  1974 ●": names are truncated so that the year and the shelf mark
/// show. The artist is truncated only if not even the start of the album would show
/// otherwise.
fn list_album_row(state: &State, entry: &ListAlbum, width: usize) -> Line<'static> {
    let album = &entry.album;
    let year = album
        .year
        .map(|year| format!("  {year}"))
        .unwrap_or_default();
    let on_shelf = state.on_shelf(&album.uri);
    let fixed = year.width() + if on_shelf { 2 } else { 0 };
    let room = width.saturating_sub(fixed);
    let album_start = album.name.width().min(6);
    let artist_room = (room / 2).max(room.saturating_sub(3 + album_start));
    let artist = truncate(&album.artist, artist_room);
    let mut spans = vec![artist.clone().into()];
    let rest = room.saturating_sub(artist.width() + 3);
    if rest > 0 {
        spans.push(" · ".fg(MUTED));
        spans.push(truncate(&album.name, rest).into());
    }
    spans.push(year.fg(MUTED));
    let mut line = Line::from(spans);
    shelf_mark(&mut line, state, album);
    line
}

/// Wraps text by words into at most `max_lines` lines. If text is left over, the
/// last line ends in "…". A word that is too long is truncated.
fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    let more = lines.len() > max_lines;
    lines.truncate(max_lines);
    if more && let Some(last) = lines.last_mut() {
        last.push('…');
    }
    lines.iter().map(|line| truncate(line, width)).collect()
}

fn artist(frame: &mut Frame, state: &State, view: &ArtistView, area: Rect) {
    if matches!(view.source, Source::Spotify(None)) {
        frame.render_widget(hint("Loading albums…"), area);
        return;
    }
    let albums = view.albums();
    if albums.is_empty() {
        frame.render_widget(hint("No albums"), area);
        return;
    }
    let from_shelf = matches!(view.source, Source::Shelf(_));
    let row = |index: usize| {
        let album = &albums[index];
        let year = album.year.map_or("    ".to_owned(), |y| y.to_string());
        let mut line = Line::from(vec![year.fg(MUTED), "  ".into(), album.name.clone().into()]);
        if !from_shelf {
            shelf_mark(&mut line, state, album);
        }
        line
    };
    list(
        frame,
        albums.len(),
        row,
        Some(view.selected),
        &view.scroll,
        area,
    );
}

/// Track list: the album, the artist and the year as the heading. For an album on the
/// Curated list, its reason is under the heading between empty rows, wherever the
/// album was opened from; the list scrolls below it. Tracks are numbered, with the
/// duration on the right edge, ♪ before the playing track and ♥ after a liked one.
fn album(frame: &mut Frame, state: &State, view: &AlbumView, area: Rect) {
    let reason = state
        .curated_reason(&view.album.uri)
        .map(|text| wrap(text, usize::from(area.width), ALBUM_REASON_LINES))
        .unwrap_or_default();
    let reason_height = if reason.is_empty() {
        0
    } else {
        reason.len() as u16 + 1
    };
    let [title, meta, _, reason_area, rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(reason_height),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(reason.into_iter().map(Line::from).collect::<Vec<_>>()).fg(MUTED),
        reason_area,
    );

    let album = &view.album;
    frame.render_widget(album.name.as_str().bold(), title);
    let mut line = Line::from(album.artist.as_str().fg(MUTED));
    if let Some(year) = album.year {
        line.push_span(format!(" · {year}").fg(MUTED));
    }
    shelf_mark(&mut line, state, album);
    frame.render_widget(line, meta);

    let Some(tracks) = &view.tracks else {
        frame.render_widget(hint("Loading tracks…"), rows);
        return;
    };
    if tracks.is_empty() {
        frame.render_widget(hint("No tracks"), rows);
        return;
    }
    let playing = state.playing_track(&album.uri);
    let digits = tracks.len().to_string().len();
    let width = usize::from(rows.width);
    let row = |index: usize| {
        let track = &tracks[index];
        let mark = if playing == Some(index) {
            "♪".fg(ACCENT)
        } else {
            " ".into()
        };
        // "♪ 12  " + name + ♥ + at least two spaces + duration.
        let number = format!(" {:>digits$}  ", index + 1);
        let duration = minutes(track.duration.as_secs());
        let liked = if state.liked(&track.uri) { LIKED } else { "" };
        let fixed = 1 + number.len() + liked.width() + duration.len();
        let name = truncate(&track.name, width.saturating_sub(fixed + 2));
        let gap = width.saturating_sub(fixed + name.width()).max(2);
        Line::from(vec![
            mark,
            number.fg(MUTED),
            name.into(),
            liked.fg(ACCENT),
            " ".repeat(gap).into(),
            duration.fg(MUTED),
        ])
    };
    list(
        frame,
        tracks.len(),
        row,
        Some(view.selected),
        &view.scroll,
        rows,
    );
}

/// The queue in playing order: heading, number of tracks and total duration, and the
/// tracks numbered (name, artist dimmed, duration on the right edge).
fn queue(frame: &mut Frame, state: &State, selected: usize, scroll: &Scroll, area: Rect) {
    let [title, meta, _, rows] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    frame.render_widget("Queue".bold(), title);
    let tracks = state.queue();
    if tracks.is_empty() {
        frame.render_widget(
            hint("The queue is empty – add tracks or albums with Ctrl+E"),
            rows,
        );
        return;
    }
    let total = tracks.iter().map(|t| t.duration.as_secs()).sum();
    frame.render_widget(
        Line::from(format!(
            "{} · {}",
            count(tracks.len(), "track"),
            minutes(total)
        ))
        .fg(MUTED),
        meta,
    );

    let digits = tracks.len().to_string().len();
    let width = usize::from(rows.width);
    let row = |index: usize| queue_row(&tracks[index], index, digits, width);
    list(frame, tracks.len(), row, Some(selected), scroll, rows);
}

/// "  3  Name  Artist      4:10": the name is truncated last.
fn queue_row(track: &QueuedTrack, index: usize, digits: usize, width: usize) -> Line<'static> {
    let number = format!("  {:>digits$}  ", index + 1);
    // A radio track's duration is unknown (zero).
    let duration = if track.duration.is_zero() {
        String::new()
    } else {
        minutes(track.duration.as_secs())
    };
    let room = width.saturating_sub(number.len() + duration.len() + 2);
    let name = truncate(&track.name, room);
    let artist = truncate(&track.artist, room.saturating_sub(name.width() + 2));
    let mut spans = vec![number.fg(MUTED), name.clone().into()];
    let mut used = name.width();
    if !artist.is_empty() {
        used += 2 + artist.width();
        spans.push("  ".into());
        spans.push(artist.fg(MUTED));
    }
    let gap = room.saturating_sub(used) + 2;
    spans.push(" ".repeat(gap).into());
    spans.push(duration.fg(MUTED));
    Line::from(spans)
}

/// Truncates text to at most `width` columns, ending truncated text with "…".
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut short = String::new();
    let mut used = 1; // "…"
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        short.push(c);
    }
    if width > 0 {
        short.push('…');
    }
    short
}

fn search(frame: &mut Frame, state: &State, view: &SearchView, area: Rect) {
    let [input, _, results] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    // The field is underlined in the accent colour up to the cursor, so even an empty
    // field is visible. The text shown while searching starts after a space.
    let field = Style::new().underlined().underline_color(ACCENT);
    let mut line = Line::from(vec![
        SEARCH_PROMPT.fg(MUTED),
        Span::styled(format!("{} ", view.query), field),
    ]);
    if view.searching() {
        line.push_span(" searching…".fg(MUTED));
    }
    frame.render_widget(line, input);
    let before_cursor: String = view.query.chars().take(view.cursor).collect();
    let x =
        input.x + Span::raw(SEARCH_PROMPT).width() as u16 + Span::raw(before_cursor).width() as u16;
    frame.set_cursor_position((x.min(input.right().saturating_sub(1)), input.y));

    let Some(found) = &view.results else {
        return;
    };
    if found.artists.is_empty() && found.albums.is_empty() {
        frame.render_widget(hint("No results"), results);
        return;
    }

    // Group headings and the gap are list rows that cannot be selected: the selection's
    // index within the results is converted to a row index.
    let mut rows = Vec::new();
    let mut selected_row = None;
    let mut group = None;
    for (index, item) in view.items().into_iter().enumerate() {
        let (name, line) = match item {
            SearchItem::Artist(artist) => ("Artists", Line::from(artist.name.as_str())),
            SearchItem::Album(album) => {
                let mut line = Line::from(vec![
                    album.name.as_str().into(),
                    "  ".into(),
                    album.artist.as_str().fg(MUTED),
                ]);
                if let Some(year) = album.year {
                    line.push_span(format!(" · {year}").fg(MUTED));
                }
                shelf_mark(&mut line, state, album);
                ("Albums", line)
            }
        };
        if group != Some(name) {
            if group.is_some() {
                rows.push(Line::default());
            }
            rows.push(Line::from(name).fg(HEADING).bold());
            group = Some(name);
        }
        if index == view.selected {
            selected_row = Some(rows.len());
        }
        rows.push(line);
    }
    lines(frame, rows, selected_row, &view.scroll, results);
}

/// A green ● after an album that is on the shelf.
fn shelf_mark(line: &mut Line, state: &State, album: &Album) {
    if state.on_shelf(&album.uri) {
        line.push_span(" ●".green());
    }
}

/// A list whose selected row is highlighted across the full width with a dim background
/// and bold text. The row's parts keep their own colours (the shelf ● green), so the
/// highlight uses the row's style and not the list's highlight_style, which would
/// cover them. Dim parts (year, artist) turn lighter on the selected row. The list
/// scrolls only when the selection would go past the visible area, and the new scroll
/// position is kept in the view state.
///
/// The list has `len` rows, and `row` builds a row from its index. Only the visible
/// rows are built, so drawing even a long list (e.g. radio) does not depend on its
/// length.
fn list<'a>(
    frame: &mut Frame,
    len: usize,
    mut row: impl FnMut(usize) -> Line<'a>,
    selected: Option<usize>,
    scroll: &Scroll,
    area: Rect,
) {
    let height = usize::from(area.height);
    if len == 0 || height == 0 || area.width == 0 {
        return;
    }
    // A selection past the end of the list lands on the last row, as in ratatui.
    let selected = selected.map(|selected| selected.min(len - 1));
    let first = first_visible(len, height, selected, scroll.get());
    scroll.set(first);
    let selected_style = Style::new().fg(SELECTED_FG).bg(SELECTED_BG).bold();
    let items = (first..len.min(first + height)).map(|index| {
        let line = row(index);
        if Some(index) == selected {
            ListItem::new(brighten_muted(line)).style(selected_style)
        } else {
            ListItem::new(line)
        }
    });
    frame.render_widget(List::new(items), area);
}

/// [`list`] from ready-made rows (shelf and search, which have heading and gap rows).
fn lines(
    frame: &mut Frame,
    mut rows: Vec<Line>,
    selected: Option<usize>,
    scroll: &Scroll,
    area: Rect,
) {
    let len = rows.len();
    list(
        frame,
        len,
        |index| std::mem::take(&mut rows[index]),
        selected,
        scroll,
        area,
    );
}

/// The top visible row, computed as ratatui's `List` does for single-line rows: the
/// previous position stays until the selection would go past the visible area.
/// `selected` is within the list.
fn first_visible(len: usize, height: usize, selected: Option<usize>, offset: usize) -> usize {
    let offset = offset.min(len - 1);
    let Some(selected) = selected else {
        return offset;
    };
    if selected < offset {
        selected
    } else if selected >= offset + height {
        selected + 1 - height
    } else {
        offset
    }
}

fn brighten_muted(mut line: Line) -> Line {
    for span in &mut line.spans {
        if span.style.fg == Some(MUTED) {
            span.style.fg = Some(SELECTED_MUTED);
        }
    }
    line
}

fn hint(text: &str) -> Paragraph<'_> {
    Paragraph::new(text).style(Style::new().fg(MUTED))
}

/// The bottom row: `[key]` in the accent colour, the label dimmed. ↑↓ and Enter (unless
/// it plays) are left out as self-evident. Outside search, Ctrl keys are shown as
/// plain letters (`[f] search`, for the queue `[u] show queue`, because q quits).
/// The Ctrl+A label tells whether it adds the selected album to the shelf (or the genre
/// to home) or removes it from there, and like shows only while something is playing:
/// `like playing` or `unlike playing`, because l applies to the playing track and not
/// to the selected row.
///
/// `[n] now playing` shows when n leads somewhere, and only if the row fits with it
/// even with the longest labels, at most by leaving out quit: otherwise it would
/// appear and disappear as the selection moves. If the row does not fit, quit is left
/// out (q, Ctrl+C in search); if that is not enough, Space and ←→, which work
/// everywhere, are left out instead, except on the shelf, whose help always shows
/// them; as a last resort both.
///
/// The shelf's `[d] remove from home` shows only while a genre row is selected. It
/// does not affect whether `[n]` shows, so that n does not disappear as the selection
/// moves; if the row does not fit with it, Space and ←→ may be left out on the shelf
/// too.
fn help_line(state: &State, view: &View, width: u16) -> Line<'static> {
    let entry_width = |(key, label): &(&str, &str)| key.width() + label.width() + 3;
    let total = |entries: &[(&str, &str)]| {
        entries.iter().map(entry_width).sum::<usize>() + 2 * entries.len().saturating_sub(1)
    };
    let mut entries = key_help(view).to_vec();
    let remove = if selected_on_shelf(state, view) {
        Some("remove from shelf")
    } else if selected_favorite(state, view) {
        Some("remove from home")
    } else {
        None
    };
    let playing = state
        .now_playing
        .as_ref()
        .filter(|playing| !playing.uri.is_empty());
    let like = playing.map(|playing| {
        if state.liked(&playing.uri) {
            "unlike playing"
        } else {
            "like playing"
        }
    });
    let now_playing = state.has_now_playing();
    let shelf = matches!(view, View::Shelf { .. });
    let home_row = selected_home_genre(state, view);
    entries.retain(|(key, _)| match *key {
        "Ctrl+L" => like.is_some(),
        "Ctrl+N" => now_playing,
        "Ctrl+D" if shelf => home_row,
        _ => true,
    });
    let search = matches!(view, View::Search(_));
    for entry in &mut entries {
        match entry.0 {
            "Ctrl+A" => entry.1 = remove.unwrap_or(entry.1),
            "Ctrl+L" => entry.1 = like.unwrap_or(entry.1),
            _ => {}
        }
        if !search {
            entry.0 = short_key(entry.0);
        }
    }
    let quit = |entry: &(&str, &str)| entry.1 == "quit";
    let home = |entry: &(&str, &str)| shelf && entry.1 == "remove from home";
    let longest: Vec<_> = entries
        .iter()
        .filter(|entry| !quit(entry) && !home(entry))
        .map(|&(key, label)| (key, longest_label(label)))
        .collect();
    if total(&longest) > usize::from(width) {
        entries.retain(|entry| entry.1 != "now playing");
    }
    if total(&entries) > usize::from(width) {
        let player =
            |entry: &(&str, &str)| (!shelf || home_row) && matches!(entry.0, "Space" | "←→");
        let without = |drop: &dyn Fn(&(&str, &str)) -> bool| -> Vec<(&'static str, &'static str)> {
            entries
                .iter()
                .copied()
                .filter(|entry| !drop(entry))
                .collect()
        };
        let both = |entry: &(&str, &str)| quit(entry) || player(entry);
        entries = [without(&quit), without(&player)]
            .into_iter()
            .find(|candidate| total(candidate) <= usize::from(width))
            .unwrap_or_else(|| without(&both));
    }
    let mut spans = Vec::new();
    for (index, (key, label)) in entries.iter().enumerate() {
        if index > 0 {
            spans.push("  ".into());
        }
        spans.push(format!("[{key}]").fg(ACCENT).bold());
        spans.push(format!(" {label}").fg(MUTED));
    }
    Line::from(spans)
}

/// Outside search, a Ctrl key also works as a plain letter.
fn short_key(key: &'static str) -> &'static str {
    match key {
        "Ctrl+F" => "f",
        "Ctrl+L" => "l",
        "Ctrl+N" => "n",
        "Ctrl+E" => "e",
        "Ctrl+A" => "a",
        "Ctrl+D" => "d",
        "Ctrl+R" => "r",
        "Ctrl+Q" => "u",
        key => key,
    }
}

/// Whether the album that Ctrl+A applies to is already on the shelf.
fn selected_on_shelf(state: &State, view: &View) -> bool {
    let uri = match view {
        View::Album(view) => Some(view.album.uri.as_str()),
        View::Artist(view) => view
            .albums()
            .get(view.selected)
            .map(|album| album.uri.as_str()),
        View::Search(view) => match view.items().get(view.selected) {
            Some(SearchItem::Album(album)) => Some(album.uri.as_str()),
            _ => None,
        },
        View::Curated { selected, .. } => state
            .curated()
            .and_then(|list| list.albums.get(*selected))
            .map(|entry| entry.album.uri.as_str()),
        View::Radio(view) => state
            .radio_tracks(&view.genre.id)
            .and_then(|tracks| tracks.get(view.selected))
            .map(|track| track.album.uri.as_str()),
        View::Shelf { .. } | View::Queue { .. } | View::Genres { .. } => None,
    };
    uri.is_some_and(|uri| state.on_shelf(uri))
}

/// Whether the selected shelf row is a genre on home (d removes it from home).
fn selected_home_genre(state: &State, view: &View) -> bool {
    match view {
        View::Shelf { selected, .. } => {
            matches!(state.shelf_lists().get(*selected), Some(ShelfRow::Genre(_)))
        }
        _ => false,
    }
}

/// Whether the genre selected in the Genres view is on home.
fn selected_favorite(state: &State, view: &View) -> bool {
    match view {
        View::Genres { selected, .. } => genres::all()
            .get(*selected)
            .is_some_and(|genre| state.favorite(&genre.id)),
        _ => false,
    }
}

fn key_help(view: &View) -> &'static [(&'static str, &'static str)] {
    match view {
        View::Shelf { .. } => &[
            ("Space", "pause"),
            ("←→", "track"),
            ("Ctrl+D", "remove from home"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Ctrl+F", "search"),
            ("Ctrl+Q", "show queue"),
            ("q", "quit"),
        ],
        View::Curated { .. } => &[
            ("Ctrl+E", "add to queue"),
            ("Ctrl+A", "add to shelf"),
            ("Ctrl+D", "not for me"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Esc", "back"),
        ],
        View::Artist(ArtistView {
            source: Source::Shelf(_),
            ..
        }) => &[
            ("Ctrl+E", "add to queue"),
            ("Ctrl+D", "remove from shelf"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Space", "pause"),
            ("←→", "track"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        View::Artist(_) => &[
            ("Ctrl+E", "add to queue"),
            ("Ctrl+A", "add to shelf"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Space", "pause"),
            ("←→", "track"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        View::Album(_) => &[
            ("Enter", "play from here"),
            ("Ctrl+E", "add to queue"),
            ("Ctrl+A", "add to shelf"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Space", "pause"),
            ("←→", "track"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
        View::Search(_) => &[
            ("Ctrl+E", "add to queue"),
            ("Ctrl+A", "add to shelf"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Esc", "back"),
            ("Ctrl+C", "quit"),
        ],
        View::Genres { .. } => &[
            ("Enter", "play radio"),
            ("Ctrl+A", "add to home"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Esc", "back"),
        ],
        // Enter's label is short so that the row fits in 100 columns even with the label
        // "remove from shelf".
        View::Radio(_) => &[
            ("Enter", "play"),
            ("Ctrl+E", "add to queue"),
            ("Ctrl+A", "add to shelf"),
            ("Ctrl+R", "new radio"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Esc", "back"),
        ],
        View::Queue { .. } => &[
            ("Ctrl+D", "remove"),
            ("Ctrl+N", "now playing"),
            ("Ctrl+L", "like playing"),
            ("Space", "pause"),
            ("←→", "track"),
            ("Esc", "back"),
            ("q", "quit"),
        ],
    }
}

/// The longest form of a label: remove for Ctrl+A and unlike for l.
fn longest_label(label: &'static str) -> &'static str {
    match label {
        "add to shelf" => "remove from shelf",
        "add to home" => "remove from home",
        "like playing" => "unlike playing",
        label => label,
    }
}

fn clock(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// A track's duration in a list: 4:10.
fn minutes(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Modifier};

    use super::*;
    use crate::{
        app::{Action, Event, Key},
        catalog::{Artist, SearchResults, Track},
        lists::{Curated, TEST_NOW},
        player,
    };

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

    fn colour() -> Album {
        album(
            "colour",
            "The Colour And The Shape",
            "Foo Fighters",
            Some(1997),
        )
    }

    fn shelf() -> Vec<Album> {
        vec![
            album("nevermind", "Nevermind", "Nirvana", Some(1991)),
            colour(),
            album("bleach", "Bleach", "Nirvana", Some(1989)),
        ]
    }

    fn render(state: &State, now: Instant, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state, now)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height).map(|y| row(buffer, y)).collect()
    }

    /// Selected row: dim background and bold text.
    fn is_selected(buffer: &Buffer, x: u16, y: u16) -> bool {
        let cell = &buffer[(x, y)];
        cell.bg == SELECTED_BG && cell.modifier.contains(Modifier::BOLD)
    }

    /// Plays Everlong (4:10), `position_ms` into the track.
    fn playing(state: &mut State, now: Instant, position_ms: u32) {
        state.update(
            Event::Player(player::Event::TrackChanged {
                uri: "spotify:track:everlong".to_owned(),
                artist: "Foo Fighters".to_owned(),
                album: "The Colour And The Shape".to_owned(),
                track: "Everlong".to_owned(),
                duration: Duration::from_secs(250),
                cover: None,
            }),
            now,
        );
        state.update(Event::Player(player::Event::Position(position_ms)), now);
    }

    #[test]
    fn shelf_view_lists_artists_with_header_and_help() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        state.update(Event::Key(Key::Down), now);
        let buffer = render(&state, now, 60, 11);

        assert_eq!(
            rows(&buffer),
            [
                "DECK",
                "—",
                format!("●{}", "─".repeat(59)).as_str(),
                "",
                format!("≋ Genres{}26 genres", " ".repeat(43)).as_str(),
                "",
                "Foo Fighters",
                "Nirvana",
                "",
                "─".repeat(60).as_str(),
                "[Space] pause  [←→] track  [f] search  [u] show queue",
            ]
        );
        // DECK is in the accent colour and bold.
        assert_eq!(buffer[(0, 0)].fg, ACCENT);
        assert!(buffer[(3, 0)].modifier.contains(Modifier::BOLD));
        // The selected row is highlighted across the full width with light text, the
        // others are not.
        assert!(is_selected(&buffer, 0, 6));
        assert!(is_selected(&buffer, 59, 6));
        assert_eq!(buffer[(0, 6)].fg, SELECTED_FG);
        // Genres is dim.
        assert_eq!(buffer[(2, 4)].fg, MUTED);
        assert_eq!(buffer[(59, 4)].fg, MUTED);
        assert!(!is_selected(&buffer, 0, 4));
        assert_eq!(buffer[(0, 4)].bg, Color::Reset);
        // When nothing is playing, row 2 and the bar are dim and ● is at the start.
        assert_eq!(buffer[(0, 1)].fg, MUTED);
        assert_eq!(buffer[(0, 2)].fg, ACCENT);
        assert_eq!(buffer[(1, 2)].fg, RULE);
        assert_eq!(buffer[(59, 2)].fg, RULE);
        // A dim full-width rule above the help row.
        assert_eq!(buffer[(0, 9)].fg, RULE);
        assert_eq!(buffer[(59, 9)].fg, RULE);
        // Help row: [key] in the accent colour and bold without a background, label dim.
        for x in [0, 1, 6] {
            assert_eq!(buffer[(x, 10)].fg, ACCENT, "x {x}");
            assert!(buffer[(x, 10)].modifier.contains(Modifier::BOLD), "x {x}");
            assert_eq!(buffer[(x, 10)].bg, Color::Reset, "x {x}");
        }
        assert_eq!(buffer[(8, 10)].fg, MUTED);
        assert!(!buffer[(8, 10)].modifier.contains(Modifier::BOLD));
    }

    const NO_OTHER_REASON: &str = "Cosmic country-rock that Teenage Fanclub and The Posies \
        grew up on, overlooked when it came out, and a record you return to for decades.";

    /// Curated list: three albums with their reasons, round 2026-W41.
    fn curated_list() -> AlbumList {
        let entry = |album: Album, reason: &str| ListAlbum {
            album,
            reason: Some(reason.to_owned()),
        };
        Curated {
            round: "2026-W41".to_owned(),
            created: "2026-10-05T07:00:00+03:00".to_owned(),
            albums: vec![
                entry(
                    album("noother", "No Other", "Gene Clark", Some(1974)),
                    NO_OTHER_REASON,
                ),
                entry(
                    album("number1", "#1 Record", "Big Star", Some(1972)),
                    "Power pop at its source.",
                ),
                entry(colour(), "Already on your shelf."),
            ],
        }
        .to_list(TEST_NOW)
    }

    #[test]
    fn shelf_shows_list_row_above_artists() {
        let now = Instant::now();
        let state = State::new(shelf()).with_curated(Ok(Some(curated_list())));
        let buffer = render(&state, now, 60, 12);
        assert_eq!(
            rows(&buffer)[3..],
            [
                "",
                format!("✦ Curated{}3 albums · week 41", " ".repeat(33)).as_str(),
                format!("≋ Genres{}26 genres", " ".repeat(43)).as_str(),
                "",
                "Foo Fighters",
                "Nirvana",
                "",
                "─".repeat(60).as_str(),
                "[Space] pause  [←→] track  [f] search  [u] show queue",
            ]
        );
        // The list row is selected across the full width, count and week dimmed (lighter
        // on the selected row). The empty row and the artists are not selected.
        assert!(is_selected(&buffer, 0, 4));
        assert!(is_selected(&buffer, 59, 4));
        assert_eq!(buffer[(0, 4)].fg, SELECTED_FG);
        assert_eq!(buffer[(42, 4)].fg, SELECTED_MUTED);
        assert!(!is_selected(&buffer, 0, 5));
        assert!(!is_selected(&buffer, 0, 6));
        assert!(!is_selected(&buffer, 0, 7));

        // Selection on an artist: the list row's count is dim.
        let mut state = state;
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Down), now);
        let buffer = render(&state, now, 60, 12);
        assert!(is_selected(&buffer, 0, 7));
        assert!(!is_selected(&buffer, 0, 4));
        assert_eq!(buffer[(42, 4)].fg, MUTED);
    }

    #[test]
    fn shelf_without_list_looks_as_before() {
        let now = Instant::now();
        let before = render(&State::new(shelf()), now, 60, 10);
        for list in [
            None,
            Some(
                Curated {
                    round: "2026-W41".to_owned(),
                    created: String::new(),
                    albums: Vec::new(),
                }
                .to_list(TEST_NOW),
            ),
        ] {
            let state = State::new(shelf()).with_curated(Ok(list));
            assert_eq!(render(&state, now, 60, 10), before);
        }
        // Broken file: an error on the top row, the shelf otherwise unchanged.
        let state = State::new(shelf()).with_curated(Err(
            "curated list file ~/.config/deck/curated.json is broken".into(),
        ));
        let buffer = render(&state, now, 60, 10);
        assert_eq!(
            row(&buffer, 1),
            "curated list file ~/.config/deck/curated.json is broken"
        );
        assert_eq!(buffer[(0, 1)].fg, Color::Red);
        assert_eq!(rows(&buffer)[3..], rows(&before)[3..]);
    }

    #[test]
    fn empty_shelf_shows_hint_below_list_row() {
        let now = Instant::now();
        let state = State::new(Vec::new()).with_curated(Ok(Some(curated_list())));
        let buffer = render(&state, now, 60, 11);
        assert!(row(&buffer, 4).starts_with("✦ Curated"));
        assert!(row(&buffer, 5).starts_with("≋ Genres"));
        assert_eq!(row(&buffer, 6), "");
        assert_eq!(
            row(&buffer, 7),
            "The shelf is empty – search for albums with Ctrl+F"
        );
        assert!(is_selected(&buffer, 0, 4));
    }

    #[test]
    fn stale_list_shows_its_age_in_warning_color() {
        let now = Instant::now();
        let mut list = curated_list();
        list.subtitle = "3 albums · week 41 · 9 days old".to_owned();
        list.stale = true;
        let mut state = State::new(shelf()).with_curated(Ok(Some(list)));
        state.update(Event::Key(Key::Down), now);

        // The shelf's list row: the age on the right edge in the warning colour.
        let buffer = render(&state, now, 60, 11);
        assert_eq!(
            row(&buffer, 4),
            format!("✦ Curated{}3 albums · week 41 · 9 days old", " ".repeat(20))
        );
        assert_eq!(buffer[(28, 4)].fg, Color::Reset);
        assert_eq!(buffer[(29, 4)].fg, WARNING);
        assert_eq!(buffer[(59, 4)].fg, WARNING);

        // List view: the subtitle in the warning colour, followed by a hint about the log.
        state.update(Event::Key(Key::Up), now);
        state.update(Event::Key(Key::Enter), now);
        let buffer = render(&state, now, 80, 16);
        assert_eq!(
            row(&buffer, 5),
            "3 albums · week 41 · 9 days old – see ~/.cache/deck/curate.log"
        );
        assert_eq!(buffer[(0, 5)].fg, WARNING);
        assert_eq!(buffer[(40, 5)].fg, MUTED);
    }

    /// Shelf list row › Curated, with album `index` selected.
    fn curated_view(index: usize, now: Instant) -> State {
        let mut state = State::new(shelf()).with_curated(Ok(Some(curated_list())));
        state.update(Event::Key(Key::Enter), now);
        for _ in 0..index {
            state.update(Event::Key(Key::Down), now);
        }
        state
    }

    #[test]
    fn curated_view_lists_albums_with_reason_below() {
        let now = Instant::now();
        let state = curated_view(0, now);
        let buffer = render(&state, now, 60, 17);
        assert_eq!(
            rows(&buffer)[3..],
            [
                "",
                "Curated",
                "3 albums · week 41",
                "",
                "Gene Clark · No Other  1974",
                "Big Star · #1 Record  1972",
                "Foo Fighters · The Colour And The Shape  1997 ●",
                "",
                "",
                "Cosmic country-rock that Teenage Fanclub and The Posies grew",
                "up on, overlooked when it came out, and a record you return…",
                "",
                "─".repeat(60).as_str(),
                "[e] add to queue  [a] add to shelf  [d] not for me  [Esc] ba",
            ]
        );
        // Heading bold, subtitle dimmed.
        assert!(buffer[(0, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        // Selected album highlighted; year dim, the ● of an album on the shelf green.
        assert!(is_selected(&buffer, 0, 7));
        assert!(!is_selected(&buffer, 0, 8));
        assert_eq!(buffer[(24, 7)].fg, SELECTED_MUTED);
        assert_eq!(buffer[(22, 8)].fg, MUTED);
        assert_eq!(buffer[(46, 9)].fg, Color::Green);
        // Reason dimmed.
        assert_eq!(buffer[(0, 12)].fg, MUTED);
        assert_eq!(buffer[(0, 13)].fg, MUTED);

        // The reason follows the selection. Ctrl+A on an album on the shelf removes it.
        let state = curated_view(2, now);
        let buffer = render(&state, now, 100, 17);
        assert_eq!(row(&buffer, 12), "Already on your shelf.");
        assert_eq!(row(&buffer, 13), "");
        assert_eq!(
            row(&buffer, 16),
            "[e] add to queue  [a] remove from shelf  [d] not for me  [Esc] back"
        );
        let state = curated_view(1, now);
        assert_eq!(
            row(&render(&state, now, 100, 17), 16),
            "[e] add to queue  [a] add to shelf  [d] not for me  [Esc] back"
        );
    }

    #[test]
    fn curated_view_scrolls_keeping_reason_in_place() {
        let now = Instant::now();
        let state = curated_view(2, now);
        // Two rows for the list: the list scrolls, the reason and the help row stay.
        let buffer = render(&state, now, 60, 15);
        assert_eq!(row(&buffer, 7), "Big Star · #1 Record  1972");
        assert!(row(&buffer, 8).starts_with("Foo Fighters · The Colour"));
        assert!(is_selected(&buffer, 0, 8));
        assert_eq!(row(&buffer, 9), "");
        assert_eq!(row(&buffer, 10), "Already on your shelf.");
        assert!(row(&buffer, 14).starts_with("[e] add to queue"));
    }

    #[test]
    fn long_curated_row_is_shortened_keeping_year() {
        let now = Instant::now();
        let state = curated_view(0, now);
        let buffer = render(&state, now, 30, 16);
        assert_eq!(row(&buffer, 7), "Gene Clark · No Other  1974");
        assert_eq!(row(&buffer, 9), "Foo Fighters · The Co…  1997 ●");
        let buffer = render(&state, now, 20, 16);
        assert_eq!(row(&buffer, 9), "Foo F… · Th…  1997 ●");
    }

    #[test]
    fn curated_view_without_albums_shows_hint() {
        let now = Instant::now();
        let mut state = curated_view(0, now);
        state.update(
            Event::CuratedChanged(Some(
                Curated {
                    round: "2026-W41".to_owned(),
                    created: String::new(),
                    albums: Vec::new(),
                }
                .to_list(TEST_NOW),
            )),
            now,
        );
        let buffer = render(&state, now, 60, 16);
        assert_eq!(row(&buffer, 4), "Curated");
        assert_eq!(row(&buffer, 5), "");
        assert_eq!(
            row(&buffer, 7),
            "Nothing left – a new list arrives with the next weekly run"
        );
    }

    #[test]
    fn wrap_breaks_words_and_marks_the_rest() {
        assert_eq!(wrap("one two three", 7, 2), ["one two", "three"]);
        assert_eq!(wrap("one two three four", 7, 2), ["one two", "three…"]);
        assert_eq!(wrap("one two three fours", 9, 1), ["one two…"]);
        assert_eq!(wrap("abcdefghij", 5, 2), ["abcd…"]);
        assert!(wrap("", 10, 2).is_empty());
    }

    #[test]
    fn empty_shelf_shows_hint() {
        let now = Instant::now();
        let buffer = render(&State::new(Vec::new()), now, 60, 10);
        assert!(row(&buffer, 4).starts_with("≋ Genres"));
        assert_eq!(
            row(&buffer, 6),
            "The shelf is empty – search for albums with Ctrl+F"
        );
    }

    #[test]
    fn artist_view_from_shelf_lists_albums_oldest_first() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        for key in [Key::Down, Key::Down, Key::Enter] {
            state.update(Event::Key(key), now);
        }
        let buffer = render(&state, now, 70, 9);

        let rows = rows(&buffer);
        assert_eq!(rows[0], "DECK");
        assert_eq!(rows[4], "1989  Bleach");
        assert_eq!(rows[5], "1991  Nevermind");
        assert_eq!(rows[7], "─".repeat(70));
        assert_eq!(
            rows[8],
            "[e] add to queue  [d] remove from shelf  [Esc] back  [q] quit"
        );
        // On the selected row the year is a grey that stands out from the background, the
        // name light.
        assert_eq!(buffer[(0, 4)].fg, SELECTED_MUTED);
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        assert!(is_selected(&buffer, 0, 4));
        assert!(is_selected(&buffer, 6, 4));
        assert_eq!(buffer[(6, 4)].fg, SELECTED_FG);
        assert!(!is_selected(&buffer, 6, 5));
    }

    #[test]
    fn search_view_groups_results_and_marks_shelf_albums() {
        let now = Instant::now();
        let mut state = State::new(vec![colour()]);
        state.update(Event::Key(Key::Ctrl('f')), now);
        for c in "foo".chars() {
            state.update(Event::Key(Key::Char(c)), now);
        }
        let later = now + Duration::from_millis(300);
        state.update(Event::Tick, later);
        state.update(
            Event::Searched {
                query: "foo".to_owned(),
                result: Ok(SearchResults {
                    artists: vec![Artist {
                        id: "id-foo-fighters".to_owned(),
                        name: "Foo Fighters".to_owned(),
                    }],
                    albums: vec![
                        colour(),
                        album(
                            "echoes",
                            "Echoes, Silence, Patience & Grace",
                            "Foo Fighters",
                            None,
                        ),
                    ],
                }),
            },
            later,
        );
        for key in [Key::Down, Key::Down] {
            state.update(Event::Key(key), later);
        }

        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| draw(frame, &state, later)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert_eq!(
            rows(&buffer)[..12],
            [
                "DECK",
                "—",
                format!("●{}", "─".repeat(69)).as_str(),
                "",
                "Search: foo",
                "",
                "Artists",
                "Foo Fighters",
                "",
                "Albums",
                "The Colour And The Shape  Foo Fighters · 1997 ●",
                "Echoes, Silence, Patience & Grace  Foo Fighters",
            ]
        );
        // The on-shelf mark is green, and the selection (second album) is highlighted.
        assert_eq!(buffer[(46, 10)].fg, Color::Green);
        assert!(is_selected(&buffer, 0, 11));
        assert!(!is_selected(&buffer, 0, 10));
        // The artist is dim; on the selected row a grey that stands out from the
        // background.
        assert_eq!(buffer[(26, 10)].fg, MUTED);
        assert_eq!(buffer[(35, 11)].fg, SELECTED_MUTED);
        // Group headings are highlighted in their own colour and bold.
        for y in [6, 9] {
            assert_eq!(buffer[(0, y)].fg, HEADING, "y {y}");
            assert!(buffer[(0, y)].modifier.contains(Modifier::BOLD), "y {y}");
        }
        // The search text is underlined in the accent colour up to the cursor; the prompt
        // is not.
        for x in 8..=11 {
            assert!(
                buffer[(x, 4)].modifier.contains(Modifier::UNDERLINED),
                "x {x}"
            );
            assert_eq!(buffer[(x, 4)].underline_color, ACCENT, "x {x}");
            assert_eq!(buffer[(x, 4)].fg, Color::Reset, "x {x}");
        }
        assert!(!buffer[(7, 4)].modifier.contains(Modifier::UNDERLINED));
        assert!(!buffer[(12, 4)].modifier.contains(Modifier::UNDERLINED));
        assert_eq!(buffer[(0, 4)].fg, MUTED);
        // The cursor is after the search text.
        terminal.backend_mut().assert_cursor_position((11, 4));
    }

    /// Search results with `albums` albums, the last one selected.
    fn long_search(albums: usize, now: Instant) -> State {
        let mut state = State::new(Vec::new());
        state.update(Event::Key(Key::Ctrl('f')), now);
        state.update(Event::Key(Key::Char('f')), now);
        state.update(Event::Tick, now + Duration::from_millis(300));
        state.update(
            Event::Searched {
                query: "f".to_owned(),
                result: Ok(SearchResults {
                    artists: vec![Artist {
                        id: "id-foo-fighters".to_owned(),
                        name: "Foo Fighters".to_owned(),
                    }],
                    albums: (1..=albums)
                        .map(|i| album(&i.to_string(), &format!("Album {i}"), "Foo Fighters", None))
                        .collect(),
                }),
            },
            now,
        );
        state
    }

    #[test]
    fn long_list_scrolls_and_leaves_a_blank_row_above_help() {
        let now = Instant::now();
        let mut state = long_search(20, now);
        // The help row is at the bottom with an empty row above it, even though there are
        // more results than fit.
        let buffer = render(&state, now, 60, 17);
        assert_eq!(row(&buffer, 13), "Album 4  Foo Fighters");
        assert_eq!(row(&buffer, 14), "");
        assert_eq!(row(&buffer, 15), "─".repeat(60));
        assert!(row(&buffer, 16).starts_with("[Ctrl+E] add to queue"));

        // Selection to the end of the list: the list scrolls, the help row and gap stay.
        for _ in 0..30 {
            state.update(Event::Key(Key::Down), now);
        }
        let buffer = render(&state, now, 60, 17);
        assert_eq!(row(&buffer, 13), "Album 20  Foo Fighters");
        assert!(is_selected(&buffer, 0, 13));
        assert_eq!(row(&buffer, 14), "");
        assert!(row(&buffer, 16).starts_with("[Ctrl+E] add to queue"));
    }

    #[test]
    fn blank_row_and_rule_above_help_in_every_view() {
        let now = Instant::now();
        let many: Vec<Album> = (1..=20)
            .map(|i| {
                album(
                    &i.to_string(),
                    &format!("Album {i}"),
                    &format!("Artist {i:02}"),
                    Some(2000),
                )
            })
            .collect();
        // Shelf.
        let mut state = State::new(many.clone());
        let buffer = render(&state, now, 60, 13);
        assert_eq!(row(&buffer, 9), "Artist 04");
        assert_eq!(row(&buffer, 10), "");
        assert_rule(&buffer, 11);
        assert!(row(&buffer, 12).starts_with("[Space] pause"));

        // Track list.
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Enter), now);
        state.update(Event::Key(Key::Enter), now);
        state.update(
            Event::AlbumTracks {
                uri: many[0].uri.clone(),
                result: Ok((1..=20)
                    .map(|i| Track {
                        uri: format!("spotify:track:{i}"),
                        name: format!("Track {i}"),
                        artist: "Nirvana".to_owned(),
                        duration: Duration::from_secs(200),
                    })
                    .collect()),
            },
            now,
        );
        let buffer = render(&state, now, 60, 13);
        assert!(row(&buffer, 9).starts_with("   3  Track 3"));
        assert_eq!(row(&buffer, 10), "");
        assert_rule(&buffer, 11);
        assert!(row(&buffer, 12).starts_with("[Enter] play from here"));
    }

    /// Row `y` is a dim full-width rule without the progress bar's ●.
    fn assert_rule(buffer: &Buffer, y: u16) {
        assert_eq!(row(buffer, y), "─".repeat(usize::from(buffer.area.width)));
        for x in 0..buffer.area.width {
            assert_eq!(buffer[(x, y)].fg, RULE, "x {x}");
        }
    }

    #[test]
    fn search_without_results_shows_hint() {
        let now = Instant::now();
        let mut state = State::new(Vec::new());
        state.update(Event::Key(Key::Ctrl('f')), now);
        state.update(Event::Key(Key::Char('x')), now);
        assert_eq!(
            row(&render(&state, now, 60, 10), 4),
            "Search: x  searching…"
        );

        let later = now + Duration::from_millis(300);
        state.update(Event::Tick, later);
        state.update(
            Event::Searched {
                query: "x".to_owned(),
                result: Ok(SearchResults::default()),
            },
            later,
        );
        let buffer = render(&state, later, 60, 10);
        assert_eq!(row(&buffer, 4), "Search: x");
        assert_eq!(row(&buffer, 6), "No results");
    }

    #[test]
    fn now_playing_shows_elapsed_time_and_track() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        playing(&mut state, now, 83_000);
        let buffer = render(&state, now, 70, 6);
        // Band / album / track on the left, time on the right edge.
        assert_eq!(
            row(&buffer, 1),
            format!(
                "{:<65}01:23",
                "Foo Fighters / The Colour And The Shape / Everlong"
            )
        );
        assert_eq!(buffer[(0, 1)].fg, Color::Reset);
        assert_eq!(buffer[(65, 1)].fg, Color::Reset);
        // The separators are dim and the track name bold.
        assert_eq!(buffer[(13, 1)].fg, MUTED);
        assert!(!buffer[(2, 1)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(45, 1)].modifier.contains(Modifier::BOLD));

        // Time moves between ticks; when paused it stops.
        let later = now + Duration::from_secs(2);
        assert!(row(&render(&state, later, 70, 6), 1).ends_with("  01:25"));
        state.update(Event::Player(player::Event::Paused(85_000)), later);
        let buffer = render(&state, later + Duration::from_secs(5), 70, 6);
        assert!(row(&buffer, 1).starts_with("Foo Fighters / "));
        assert!(row(&buffer, 1).ends_with("  01:25"));

        // In a narrow window a long row is cut before the time.
        let buffer = render(&state, later, 30, 6);
        assert!(row(&buffer, 1).starts_with("Foo Fighters / The Colo"));
        assert!(row(&buffer, 1).ends_with("  01:25"));
    }

    #[test]
    fn paused_shows_stopped_at_the_right_edge_of_the_top_row() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        playing(&mut state, now, 83_000);
        assert_eq!(row(&render(&state, now, 70, 6), 0), "DECK");

        state.update(Event::Player(player::Event::Paused(83_000)), now);
        let buffer = render(&state, now, 70, 6);
        assert_eq!(row(&buffer, 0), format!("DECK{}■ stopped", " ".repeat(57)));
        // ■ in the accent colour, text dimmed.
        assert_eq!(buffer[(61, 0)].fg, ACCENT);
        assert_eq!(buffer[(63, 0)].fg, MUTED);
        assert_eq!(buffer[(69, 0)].fg, MUTED);
        // In a wide window the mark is at the right edge of Deck's area.
        let buffer = render(&state, now, 120, 6);
        assert!(row(&buffer, 0).ends_with("■ stopped"));
        assert_eq!(row(&buffer, 0).chars().count(), 110);

        // Playback resumes: the mark goes away.
        state.update(Event::Player(player::Event::Position(84_000)), now);
        assert_eq!(row(&render(&state, now, 70, 6), 0), "DECK");
    }

    #[test]
    fn progress_bar_fills_the_deck_width() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        // Halfway: 125 s / 250 s.
        playing(&mut state, now, 125_000);

        // A narrow window is used across its full width; in a wide one Deck is 100 columns
        // in the middle of the window.
        for (width, margin) in [(40u16, 0u16), (120, 10)] {
            let buffer = render(&state, now, width, 6);
            let bar = row(&buffer, 2);
            let deck_width = width - 2 * margin;
            assert_eq!(
                bar,
                format!("{}{}", " ".repeat(margin.into()), bar.trim_start()),
                "leveys {width}"
            );
            let bar = bar.trim_start();
            assert_eq!(
                bar.chars().count(),
                usize::from(deck_width),
                "leveys {width}"
            );
            let head = usize::from(deck_width - 1) / 2;
            assert_eq!(
                bar.chars().position(|c| c == '●'),
                Some(head),
                "leveys {width}"
            );
            assert!(bar.chars().all(|c| c == '─' || c == '●'));
            // The played part and ● are in the accent colour, the remaining part dim.
            let head = margin + head as u16;
            assert_eq!(buffer[(margin, 2)].fg, ACCENT);
            assert_eq!(buffer[(head, 2)].fg, ACCENT);
            assert_eq!(buffer[(head + 1, 2)].fg, RULE);
            assert_eq!(buffer[(margin + deck_width - 1, 2)].fg, RULE);
        }
    }

    #[test]
    fn large_window_centres_a_deck_of_at_most_30_rows() {
        let now = Instant::now();
        let buffer = render(&State::new(shelf()), now, 120, 50);
        let rows = rows(&buffer);

        // 30 rows in the middle of a 50-row window: rows 10–39, empty above and below.
        let top = 10;
        let bottom = top + MAX_HEIGHT as usize - 1;
        for (y, line) in rows.iter().enumerate() {
            if y < top || y > bottom {
                assert_eq!(line, "", "rivi {y}");
            }
        }
        // 100 columns in the middle of a 120-column window: 10 empty on each side.
        let left = 10;
        let margin = " ".repeat(left.into());
        assert_eq!(rows[top], format!("{margin}DECK"));
        assert_eq!(
            rows[top + 2],
            format!("{margin}●{}", "─".repeat(usize::from(MAX_WIDTH) - 1))
        );
        assert!(rows[top + 4].starts_with(&format!("{margin}≋ Genres")));
        assert_eq!(rows[top + 6], format!("{margin}Foo Fighters"));
        assert!(rows[bottom].starts_with(&format!("{margin}[Space] pause")));
        // The selected row's highlight reaches the edges of Deck's area but not beyond.
        let right = left + MAX_WIDTH - 1;
        let y = top as u16 + 4;
        assert!(is_selected(&buffer, left, y));
        assert!(is_selected(&buffer, right, y));
        assert!(!is_selected(&buffer, left - 1, y));
        assert!(!is_selected(&buffer, right + 1, y));
    }

    #[test]
    fn small_window_is_filled_by_the_deck() {
        let now = Instant::now();
        let buffer = render(&State::new(shelf()), now, 60, 20);
        let rows = rows(&buffer);
        assert_eq!(rows[0], "DECK");
        assert_eq!(rows[2], format!("●{}", "─".repeat(59)));
        assert!(rows[19].starts_with("[Space] pause"));

        // Low but wide window: the height is filled, the width is limited to the middle.
        let buffer = render(&State::new(shelf()), now, 108, 12);
        assert_eq!(row(&buffer, 0), "    DECK");
        assert!(row(&buffer, 11).starts_with("    [Space] pause"));
    }

    fn nevermind() -> Album {
        album("nevermind", "Nevermind", "Nirvana", Some(1991))
    }

    fn nevermind_tracks() -> Vec<Track> {
        [
            ("Smells Like Teen Spirit", 301),
            ("In Bloom", 254),
            ("Come As You Are", 218),
            ("Breed", 183),
        ]
        .into_iter()
        .map(|(name, secs)| Track {
            uri: format!("spotify:track:{name}"),
            name: name.to_owned(),
            artist: "Nirvana".to_owned(),
            duration: Duration::from_secs(secs),
        })
        .collect()
    }

    /// Shelf › Nirvana › Nevermind: Enter opens the album's tracks without playing it.
    fn nevermind_state(now: Instant) -> State {
        let mut state = State::new(shelf());
        for key in [Key::Down, Key::Down, Key::Enter, Key::Down, Key::Enter] {
            state.update(Event::Key(key), now);
        }
        state
    }

    /// Plays the open Nevermind track list from track `index`. The selection returns
    /// to where it was.
    fn nevermind_playing(state: &mut State, now: Instant, index: usize) {
        let View::Album(view) = state.view() else {
            panic!("odotettiin kappalelistaa");
        };
        let selected = view.selected;
        let keys = [
            vec![Key::Up; selected],
            vec![Key::Down; index],
            vec![Key::Enter],
            vec![Key::Up; index],
            vec![Key::Down; selected],
        ];
        for key in keys.concat() {
            state.update(Event::Key(key), now);
        }
        let track = &nevermind_tracks()[index];
        state.update(
            Event::Player(player::Event::TrackChanged {
                uri: track.uri.clone(),
                artist: "Nirvana".to_owned(),
                album: "Nevermind".to_owned(),
                track: track.name.clone(),
                duration: track.duration,
                cover: None,
            }),
            now,
        );
    }

    /// A track row `width` columns wide: duration on the right edge.
    fn track_row(start: &str, duration: &str, width: usize) -> String {
        let room = width - start.chars().count() - duration.len();
        format!("{start}{}{duration}", " ".repeat(room))
    }

    #[test]
    fn album_view_lists_numbered_tracks_and_marks_the_playing_one() {
        let now = Instant::now();
        let mut state = nevermind_state(now);
        state.update(
            Event::AlbumTracks {
                uri: nevermind().uri,
                result: Ok(nevermind_tracks()),
            },
            now,
        );
        nevermind_playing(&mut state, now, 1);
        for key in [Key::Down, Key::Down] {
            state.update(Event::Key(key), now);
        }
        let buffer = render(&state, now, 50, 14);

        assert_eq!(
            rows(&buffer)[3..],
            [
                "",
                "Nevermind",
                "Nirvana · 1991 ●",
                "",
                track_row("  1  Smells Like Teen Spirit", "5:01", 50).as_str(),
                track_row("♪ 2  In Bloom", "4:14", 50).as_str(),
                track_row("  3  Come As You Are", "3:38", 50).as_str(),
                track_row("  4  Breed", "3:03", 50).as_str(),
                "",
                "─".repeat(50).as_str(),
                "[Enter] play from here  [e] add to queue  [a] remo",
            ]
        );
        // Ctrl+A on an album on the shelf removes it from the shelf.
        let help = row(&render(&state, now, 100, 14), 13);
        assert!(help.contains("[a] remove from shelf"), "{help}");
        assert!(row(&buffer, 1).starts_with("Nirvana / Nevermind / In Bloom  "));
        assert!(row(&buffer, 1).ends_with("00:00"));
        // Heading: album bold, artist and year dimmed, shelf ● green.
        assert!(buffer[(0, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        assert_eq!(buffer[(15, 5)].fg, Color::Green);
        // The playing track's ♪ in the accent colour, number and duration dimmed, name
        // normal.
        assert_eq!(buffer[(0, 8)].fg, ACCENT);
        assert_eq!(buffer[(2, 8)].fg, MUTED);
        assert_eq!(buffer[(5, 8)].fg, Color::Reset);
        assert_eq!(buffer[(49, 8)].fg, MUTED);
        // The selected track (3rd) highlighted across the full width, dim parts lighter.
        assert!(is_selected(&buffer, 0, 9));
        assert!(is_selected(&buffer, 49, 9));
        assert_eq!(buffer[(2, 9)].fg, SELECTED_MUTED);
        assert_eq!(buffer[(49, 9)].fg, SELECTED_MUTED);
        assert!(!is_selected(&buffer, 0, 8));

        // The track changes: the mark moves.
        nevermind_playing(&mut state, now, 2);
        let buffer = render(&state, now, 50, 13);
        assert!(row(&buffer, 8).starts_with("  2  In Bloom"));
        assert!(row(&buffer, 9).starts_with("♪ 3  Come As You Are"));
        assert_eq!(buffer[(0, 9)].fg, ACCENT);
    }

    #[test]
    fn album_view_shows_loading_until_tracks_arrive() {
        let now = Instant::now();
        let state = nevermind_state(now);
        let buffer = render(&state, now, 50, 13);
        assert_eq!(row(&buffer, 4), "Nevermind");
        assert_eq!(row(&buffer, 7), "Loading tracks…");
    }

    #[test]
    fn long_track_name_is_shortened_so_duration_stays_visible() {
        let now = Instant::now();
        let mut state = nevermind_state(now);
        let long = Track {
            uri: "spotify:track:long".to_owned(),
            artist: "Nirvana".to_owned(),
            name: "Endless, Nameless (Hidden Track Included After Silence)".to_owned(),
            duration: Duration::from_secs(403),
        };
        state.update(
            Event::AlbumTracks {
                uri: nevermind().uri,
                result: Ok(vec![long]),
            },
            now,
        );
        let buffer = render(&state, now, 40, 11);
        assert_eq!(row(&buffer, 7), "  1  Endless, Nameless (Hidden Tr…  6:43");
    }

    #[test]
    fn album_view_offers_ctrl_a_for_album_not_on_shelf() {
        let now = Instant::now();
        let mut state = State::new(Vec::new());
        state.update(Event::Key(Key::Ctrl('f')), now);
        state.update(Event::Key(Key::Char('n')), now);
        let later = now + Duration::from_millis(300);
        state.update(Event::Tick, later);
        state.update(
            Event::Searched {
                query: "n".to_owned(),
                result: Ok(SearchResults {
                    artists: vec![],
                    albums: vec![nevermind()],
                }),
            },
            later,
        );
        state.update(Event::Key(Key::Enter), later);
        let buffer = render(&state, later, 80, 10);
        // Not on the shelf: no ● in the heading.
        assert_eq!(row(&buffer, 5), "Nirvana · 1991");
        assert!(
            row(&buffer, 9)
                .starts_with("[Enter] play from here  [e] add to queue  [a] add to shelf")
        );
    }

    /// Curated › No Other open, with Nevermind's four tracks as its tracks.
    fn curated_album_view(now: Instant) -> State {
        let mut state = curated_view(0, now);
        state.update(Event::Key(Key::Enter), now);
        state.update(
            Event::AlbumTracks {
                uri: "spotify:album:noother".to_owned(),
                result: Ok(nevermind_tracks()),
            },
            now,
        );
        state
    }

    #[test]
    fn album_view_shows_curated_reason_below_title() {
        let now = Instant::now();
        let state = curated_album_view(now);
        let buffer = render(&state, now, 60, 18);
        assert_eq!(
            rows(&buffer)[3..],
            [
                "",
                "No Other",
                "Gene Clark · 1974",
                "",
                "Cosmic country-rock that Teenage Fanclub and The Posies grew",
                "up on, overlooked when it came out, and a record you return",
                "to for decades.",
                "",
                track_row("  1  Smells Like Teen Spirit", "5:01", 60).as_str(),
                track_row("  2  In Bloom", "4:14", 60).as_str(),
                track_row("  3  Come As You Are", "3:38", 60).as_str(),
                track_row("  4  Breed", "3:03", 60).as_str(),
                "",
                "─".repeat(60).as_str(),
                "[Enter] play from here  [e] add to queue  [a] add to shelf",
            ]
        );
        // Reason dimmed, the selected track highlighted below it.
        assert_eq!(buffer[(0, 7)].fg, MUTED);
        assert_eq!(buffer[(0, 9)].fg, MUTED);
        assert!(is_selected(&buffer, 0, 11));

        // When narrower, a longer reason is truncated on the third line.
        let buffer = render(&state, now, 40, 18);
        assert_eq!(
            rows(&buffer)[7..11],
            [
                "Cosmic country-rock that Teenage Fanclub",
                "and The Posies grew up on, overlooked",
                "when it came out, and a record you…",
                "",
            ]
        );
    }

    #[test]
    fn album_view_scrolls_tracks_keeping_curated_reason_in_place() {
        let now = Instant::now();
        let mut state = curated_album_view(now);
        for _ in 0..3 {
            state.update(Event::Key(Key::Down), now);
        }
        // Two rows for the tracks: the list scrolls, the reason stays under the heading.
        let buffer = render(&state, now, 60, 16);
        assert!(row(&buffer, 7).starts_with("Cosmic country-rock"));
        assert_eq!(row(&buffer, 10), "");
        assert!(row(&buffer, 11).starts_with("  3  Come As You Are"));
        assert!(row(&buffer, 12).starts_with("  4  Breed"));
        assert!(is_selected(&buffer, 0, 12));
        assert_eq!(row(&buffer, 13), "");
    }

    #[test]
    fn album_view_shows_curated_reason_wherever_album_was_opened() {
        let now = Instant::now();
        let search = |name: &str, found: Album| {
            let mut state = State::new(Vec::new()).with_curated(Ok(Some(curated_list())));
            state.update(Event::Key(Key::Ctrl('f')), now);
            state.update(Event::Key(Key::Char('n')), now);
            let later = now + Duration::from_millis(300);
            state.update(Event::Tick, later);
            state.update(
                Event::Searched {
                    query: "n".to_owned(),
                    result: Ok(SearchResults {
                        artists: vec![],
                        albums: vec![found],
                    }),
                },
                later,
            );
            state.update(Event::Key(Key::Enter), later);
            let buffer = render(&state, later, 60, 14);
            assert_eq!(row(&buffer, 4), name);
            buffer
        };
        // A Curated album opened via search: the reason under the heading.
        let buffer = search(
            "#1 Record",
            album("number1", "#1 Record", "Big Star", Some(1972)),
        );
        assert_eq!(row(&buffer, 6), "");
        assert_eq!(row(&buffer, 7), "Power pop at its source.");
        assert_eq!(row(&buffer, 8), "");
        assert_eq!(row(&buffer, 9), "Loading tracks…");
        // An album that is not on the list looks the same as before.
        let buffer = search("Nevermind", nevermind());
        assert_eq!(row(&buffer, 6), "");
        assert_eq!(row(&buffer, 7), "Loading tracks…");
    }

    /// WCAG contrast ratio between two RGB colours.
    fn contrast(a: Color, b: Color) -> f64 {
        fn luminance(color: Color) -> f64 {
            let Color::Rgb(r, g, b) = color else {
                panic!("{color:?} is not an RGB colour");
            };
            let channel = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.039_28 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
        }
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn selected_row_text_is_readable_on_its_background() {
        // The selected row sets its own background, so it is legible with any theme.
        assert!(contrast(SELECTED_FG, SELECTED_BG) >= 7.0);
        assert!(contrast(SELECTED_MUTED, SELECTED_BG) >= 4.5);
    }

    #[test]
    fn muted_text_is_readable_on_a_dark_theme() {
        // A fixed grey, not the theme's DarkGray: legible on a dark background, but clearly
        // dimmer than normal text and distinct from the selected row's dim background.
        let background = Color::Rgb(0x15, 0x15, 0x1C);
        let muted = contrast(MUTED, background);
        assert!(muted >= 4.5, "{muted}");
        assert!(contrast(SELECTED_FG, background) > 2.5 * muted);
        assert!(contrast(SELECTED_MUTED, background) > 1.4 * muted);
        // The rules stay darker than text.
        assert!(contrast(RULE, background) < muted);
    }

    #[test]
    fn progress_bar_ends() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        playing(&mut state, now, 0);
        assert!(row(&render(&state, now, 30, 6), 2).starts_with('●'));
        let end = now + Duration::from_secs(300);
        assert!(row(&render(&state, end, 30, 6), 2).ends_with('●'));
    }

    #[test]
    fn error_is_shown_in_red_instead_of_now_playing() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        playing(&mut state, now, 1_000);
        state.update(
            Event::Player(player::Event::Error(
                "connection to Spotify lost".to_owned(),
            )),
            now,
        );
        let buffer = render(&state, now, 60, 6);
        assert_eq!(row(&buffer, 1), "connection to Spotify lost");
        assert_eq!(buffer[(0, 1)].fg, Color::Red);
        // The bar still shows the playing track.
        assert!(row(&buffer, 2).contains('●'));
    }

    /// Nevermind plays from the first track, and In Bloom and Breed are in the queue.
    fn queued_state(now: Instant) -> State {
        let mut state = nevermind_state(now);
        state.update(
            Event::AlbumTracks {
                uri: nevermind().uri,
                result: Ok(nevermind_tracks()),
            },
            now,
        );
        nevermind_playing(&mut state, now, 0);
        let keys = [
            Key::Down,
            Key::Ctrl('e'),
            Key::Down,
            Key::Down,
            Key::Ctrl('e'),
        ];
        for key in keys {
            state.update(Event::Key(key), now);
        }
        state
    }

    #[test]
    fn top_row_shows_notice_then_queue_length_before_stopped() {
        let now = Instant::now();
        let mut state = queued_state(now);
        let buffer = render(&state, now, 50, 6);
        assert_eq!(
            row(&buffer, 0),
            format!("DECK{}+ queued: Breed", " ".repeat(31))
        );
        // + in the accent colour, text dimmed and the name normal.
        assert_eq!(buffer[(35, 0)].fg, ACCENT);
        assert_eq!(buffer[(37, 0)].fg, MUTED);
        assert_eq!(buffer[(45, 0)].fg, Color::Reset);

        // After the confirmation, the queue length.
        let later = now + crate::app::NOTICE_TIME;
        let buffer = render(&state, later, 50, 6);
        assert_eq!(row(&buffer, 0), format!("DECK{}+2 queued", " ".repeat(37)));
        assert_eq!(buffer[(41, 0)].fg, ACCENT);
        assert_eq!(buffer[(42, 0)].fg, ACCENT);
        assert_eq!(buffer[(44, 0)].fg, MUTED);

        // When paused, ■ stopped stays at the right edge.
        state.update(Event::Player(player::Event::Paused(0)), later);
        assert_eq!(
            row(&render(&state, later, 50, 6), 0),
            format!("DECK{}+2 queued   ■ stopped", " ".repeat(25))
        );
        // A long name is truncated so that DECK and ■ stopped fit.
        state.update(
            Event::AlbumQueued {
                album: album("long", "A Very Long Album Name Indeed", "X", None),
                result: Ok(nevermind_tracks()),
            },
            later,
        );
        assert_eq!(
            row(&render(&state, later, 50, 6), 0),
            "DECK  + queued: A Very Long Album Nam…   ■ stopped"
        );
    }

    #[test]
    fn empty_queue_shows_nothing_on_top_row() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        playing(&mut state, now, 0);
        assert_eq!(row(&render(&state, now, 50, 6), 0), "DECK");
    }

    #[test]
    fn queue_view_lists_tracks_with_artist_and_duration() {
        let now = Instant::now();
        let mut state = queued_state(now);
        state.update(Event::Key(Key::Ctrl('q')), now);
        state.update(Event::Key(Key::Down), now);
        let buffer = render(&state, now, 60, 12);

        assert_eq!(
            rows(&buffer)[4..],
            [
                "Queue",
                "2 tracks · 7:17",
                "",
                track_row("  1  In Bloom  Nirvana", "4:14", 60).as_str(),
                track_row("  2  Breed  Nirvana", "3:03", 60).as_str(),
                "",
                "─".repeat(60).as_str(),
                "[d] remove  [l] like playing  [Esc] back  [q] quit",
            ]
        );
        assert_eq!(buffer[(0, 4)].fg, Color::Reset);
        assert!(buffer[(0, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        // Artist and duration dimmed, the selected row highlighted.
        assert_eq!(buffer[(15, 7)].fg, MUTED);
        assert_eq!(buffer[(59, 7)].fg, MUTED);
        assert!(is_selected(&buffer, 0, 8));
        assert!(!is_selected(&buffer, 0, 7));

        // In a wider window n, Space and ←→ fit as well.
        let buffer = render(&state, now, 120, 12);
        assert_eq!(
            row(&buffer, 11).trim_start(),
            "[d] remove  [n] now playing  [l] like playing  [Space] pause  [←→] track  [Esc] back  [q] quit"
        );
    }

    #[test]
    fn queue_view_shortens_long_rows_keeping_duration() {
        let now = Instant::now();
        let mut state = queued_state(now);
        state.update(Event::Key(Key::Ctrl('q')), now);
        let buffer = render(&state, now, 24, 11);
        assert_eq!(row(&buffer, 7), "  1  In Bloom  Ni…  4:14");
        let buffer = render(&state, now, 16, 11);
        assert_eq!(row(&buffer, 7), "  1  In B…  4:14");
    }

    #[test]
    fn empty_queue_view_shows_hint() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        state.update(Event::Key(Key::Ctrl('q')), now);
        let buffer = render(&state, now, 70, 11);
        assert_eq!(row(&buffer, 4), "Queue");
        assert_eq!(row(&buffer, 5), "");
        assert_eq!(
            row(&buffer, 7),
            "The queue is empty – add tracks or albums with Ctrl+E"
        );
    }

    // --- Genres and radio ---

    fn radio_track(artist: &str, name: &str, album_id: &str, album: &str) -> radio::Track {
        radio::Track {
            uri: format!("spotify:track:{}", name.to_lowercase().replace(' ', "")),
            name: name.to_owned(),
            artist: artist.to_owned(),
            artist_uri: format!("spotify:artist:{}", artist.to_lowercase()),
            album: radio::Album {
                uri: format!("spotify:album:{album_id}"),
                name: album.to_owned(),
            },
        }
    }

    fn night_drive() -> Album {
        album("nightdrive", "Night Drive", "Timecop1983", Some(2018))
    }

    /// lofi and synthwave on home, synthwave's radio is playing and its view is open.
    /// Night Drive is on the shelf.
    fn synthwave_radio(now: Instant) -> State {
        let mut state =
            State::new(vec![night_drive()]).with_favorites(vec!["lofi".into(), "synthwave".into()]);
        state.update(Event::Key(Key::Down), now);
        let actions = state.update(Event::Key(Key::Enter), now);
        let [Action::LoadRadio(request)] = actions.as_slice() else {
            panic!("odotettiin radiohakua, oli {actions:?}");
        };
        let tracks = vec![
            radio_track("The Midnight", "Sunset", "endless", "Endless Summer"),
            radio_track("FM-84", "Running in the Night", "atlas", "Atlas"),
            radio_track("Timecop1983", "On the Run", "nightdrive", "Night Drive"),
            radio_track("Gunship", "Tech Noir", "gunship", "Gunship"),
        ];
        state.update(
            Event::RadioLoaded {
                request: request.clone(),
                result: Ok(tracks),
            },
            now,
        );
        state.update(
            Event::Player(player::Event::TrackChanged {
                uri: "spotify:track:sunset".to_owned(),
                artist: "The Midnight".to_owned(),
                album: "Endless Summer".to_owned(),
                track: "Sunset".to_owned(),
                duration: Duration::from_secs(300),
                cover: None,
            }),
            now,
        );
        state
    }

    #[test]
    fn shelf_shows_favorite_genres_and_genres_row() {
        let now = Instant::now();
        let mut state = synthwave_radio(now);
        state.update(Event::Key(Key::Esc), now);
        let buffer = render(&state, now, 70, 12);
        assert_eq!(
            rows(&buffer)[3..],
            [
                "",
                format!("≋ lofi{}radio", " ".repeat(59)).as_str(),
                format!("≋ synthwave{}♪ playing", " ".repeat(50)).as_str(),
                format!("≋ Genres{}26 genres", " ".repeat(53)).as_str(),
                "",
                "Timecop1983",
                "",
                "─".repeat(70).as_str(),
                "[d] remove from home  [l] like playing  [f] search  [u] show queue",
            ]
        );
        // Genre name normal, radio dimmed. On the playing row ♪ in the accent colour.
        assert_eq!(buffer[(2, 4)].fg, Color::Reset);
        assert_eq!(buffer[(65, 4)].fg, MUTED);
        assert!(is_selected(&buffer, 0, 5));
        assert_eq!(buffer[(61, 5)].fg, ACCENT);
        assert_eq!(buffer[(63, 5)].fg, SELECTED_MUTED);
        // Genres dimmed.
        assert_eq!(buffer[(2, 6)].fg, MUTED);
        assert_eq!(buffer[(61, 6)].fg, MUTED);
    }

    #[test]
    fn genres_view_lists_catalog_with_stars() {
        let now = Instant::now();
        let mut state =
            State::new(Vec::new()).with_favorites(vec!["lofi".into(), "synthwave".into()]);
        for key in [Key::Down, Key::Down, Key::Enter] {
            state.update(Event::Key(key), now);
        }
        let buffer = render(&state, now, 70, 30);
        let rows = rows(&buffer);
        assert_eq!(
            rows[4..10],
            [
                "Genres",
                "26 genres · 2 on home",
                "",
                "60s rock",
                "70s rock",
                "80s rock"
            ]
        );
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        let lofi = 7 + genres::all().iter().position(|g| g.id == "lofi").unwrap();
        assert_eq!(rows[lofi], "lofi ★");
        assert_eq!(buffer[(5, lofi as u16)].fg, STAR);
        assert_eq!(rows[lofi + 1], "metal");
        assert_eq!(rows[29], "[Enter] play radio  [a] add to home  [Esc] back");

        // On a favourite, Ctrl+A removes it from home.
        for _ in 7..lofi {
            state.update(Event::Key(Key::Down), now);
        }
        let buffer = render(&state, now, 70, 30);
        assert!(is_selected(&buffer, 0, lofi as u16));
        assert_eq!(
            row(&buffer, 29),
            "[Enter] play radio  [a] remove from home  [Esc] back"
        );
    }

    /// The list scrolls only when the selection goes past the visible area: going back
    /// up from the end, the list stays put until the selection is on the top row.
    #[test]
    fn list_scrolls_only_past_the_visible_edge() {
        let now = Instant::now();
        let mut state = State::new(Vec::new());
        state.update(Event::Key(Key::Enter), now);
        let names: Vec<&str> = genres::all().iter().map(|g| g.name.as_str()).collect();
        // Rows 7–14 are left for the list.
        let (top, bottom) = (7, 14);
        let press = |state: &mut State, key: Key, times: usize| {
            for _ in 0..times {
                state.update(Event::Key(key), now);
                render(state, now, 70, 18);
            }
            render(state, now, 70, 18)
        };

        // To the end: the last genre on the bottom row.
        let last = names.len() - 1;
        let buffer = press(&mut state, Key::Down, last);
        let first = last - (bottom - top);
        assert_eq!(row(&buffer, top as u16), names[first]);
        assert_eq!(row(&buffer, bottom as u16), names[last]);
        assert!(is_selected(&buffer, 0, bottom as u16));

        // ↑ moves the selection up, the list stays put until the top row.
        for up in 1..=bottom - top {
            let buffer = press(&mut state, Key::Up, 1);
            assert_eq!(row(&buffer, top as u16), names[first]);
            assert!(is_selected(&buffer, 0, (bottom - up) as u16));
        }
        // Only the next ↑ scrolls the list.
        let buffer = press(&mut state, Key::Up, 1);
        assert_eq!(row(&buffer, top as u16), names[first - 1]);
        assert!(is_selected(&buffer, 0, top as u16));

        // ↓ does not scroll before the bottom row.
        let buffer = press(&mut state, Key::Down, 1);
        assert_eq!(row(&buffer, top as u16), names[first - 1]);
        assert!(is_selected(&buffer, 0, top as u16 + 1));
    }

    /// `first_visible` scrolls like ratatui's `List` when given all the rows.
    #[test]
    fn first_visible_matches_ratatui_list() {
        use ratatui::widgets::{ListState, StatefulWidget};
        for len in 1..8 {
            for height in 1..6u16 {
                for offset in 0..10 {
                    for selected in [None, Some(0), Some(2), Some(4), Some(7), Some(9)] {
                        let area = Rect::new(0, 0, 10, height);
                        let mut buffer = Buffer::empty(area);
                        let mut list_state = ListState::default()
                            .with_offset(offset)
                            .with_selected(selected);
                        let items = (0..len).map(|i| ListItem::new(i.to_string()));
                        StatefulWidget::render(
                            List::new(items),
                            area,
                            &mut buffer,
                            &mut list_state,
                        );
                        let selected = selected.map(|s| s.min(len - 1));
                        assert_eq!(
                            first_visible(len, usize::from(height), selected, offset),
                            list_state.offset(),
                            "len {len}, height {height}, offset {offset}, selected {selected:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn radio_view_lists_tracks_with_playing_and_shelf_marks() {
        let now = Instant::now();
        let mut state = synthwave_radio(now);
        let buffer = render(&state, now, 70, 15);
        assert_eq!(
            rows(&buffer)[4..13],
            [
                "synthwave",
                "radio",
                "",
                "♪ The Midnight · Sunset  Endless Summer",
                "  FM-84 · Running in the Night  Atlas",
                "  Timecop1983 · On the Run  Night Drive ●",
                "  Gunship · Tech Noir  Gunship",
                "",
                "",
            ]
        );
        assert!(buffer[(0, 4)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(0, 5)].fg, MUTED);
        assert_eq!(buffer[(0, 7)].fg, ACCENT);
        assert!(is_selected(&buffer, 0, 7));
        // Album dimmed, shelf ● green.
        assert_eq!(buffer[(32, 8)].fg, MUTED);
        assert_eq!(buffer[(40, 9)].fg, Color::Green);

        // The help row fits in 100 columns even with the label "remove from shelf".
        let buffer = render(&state, now, 100, 15);
        assert_eq!(
            row(&buffer, 14),
            "[Enter] play  [e] add to queue  [a] add to shelf  [r] new radio  [l] like playing  [Esc] back"
        );
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Down), now);
        let buffer = render(&state, now, 100, 15);
        assert_eq!(
            row(&buffer, 14),
            "[Enter] play  [e] add to queue  [a] remove from shelf  [r] new radio  [l] like playing  [Esc] back"
        );
    }

    #[test]
    fn long_radio_row_is_shortened_album_first() {
        let now = Instant::now();
        let state = synthwave_radio(now);
        let buffer = render(&state, now, 36, 14);
        assert_eq!(row(&buffer, 8), "  FM-84 · Running in the Night  Atl…");
        assert_eq!(row(&buffer, 9), "  Timecop1983 · On the Run  Night… ●");
        // The album is not shown as just "A…".
        let buffer = render(&state, now, 34, 14);
        assert_eq!(row(&buffer, 8), "  FM-84 · Running in the Night");
        let buffer = render(&state, now, 24, 14);
        assert_eq!(row(&buffer, 8), "  FM-84 · Running in th…");
        assert_eq!(row(&buffer, 9), "  Timecop1983 · On th… ●");
    }

    #[test]
    fn stopped_radio_view_shows_hint() {
        let now = Instant::now();
        let mut state = synthwave_radio(now);
        state.update(Event::Player(player::Event::Stopped), now);
        let buffer = render(&state, now, 70, 14);
        assert_eq!(row(&buffer, 4), "synthwave");
        assert_eq!(
            row(&buffer, 7),
            "The radio has stopped – Ctrl+R starts it again"
        );
    }

    #[test]
    fn queued_radio_track_shows_no_duration() {
        let now = Instant::now();
        let mut state = synthwave_radio(now);
        for key in [Key::Down, Key::Ctrl('e'), Key::Ctrl('q')] {
            state.update(Event::Key(key), now);
        }
        let buffer = render(&state, now, 50, 11);
        assert_eq!(row(&buffer, 5), "1 track · 0:00");
        assert_eq!(row(&buffer, 7), "  1  Running in the Night  FM-84");
    }

    /// The tracks (URIs) as liked, as Spotify's response would report them.
    fn liked(state: &mut State, now: Instant, uris: &[&str]) {
        state.update(
            Event::LikedChecked {
                uris: uris.iter().map(|&uri| uri.to_owned()).collect(),
                result: Ok(vec![true; uris.len()]),
            },
            now,
        );
    }

    #[test]
    fn liked_tracks_have_heart_on_top_row_and_track_list() {
        let now = Instant::now();
        let mut state = nevermind_state(now);
        state.update(
            Event::AlbumTracks {
                uri: nevermind().uri,
                result: Ok(nevermind_tracks()),
            },
            now,
        );
        nevermind_playing(&mut state, now, 1);
        liked(
            &mut state,
            now,
            &["spotify:track:In Bloom", "spotify:track:Breed"],
        );
        let buffer = render(&state, now, 50, 14);

        assert_eq!(
            row(&buffer, 1),
            format!("{:<45}00:00", "Nirvana / Nevermind / In Bloom ♥")
        );
        assert_eq!(buffer[(31, 1)].fg, ACCENT);
        assert_eq!(
            rows(&buffer)[7..11],
            [
                track_row("  1  Smells Like Teen Spirit", "5:01", 50).as_str(),
                track_row("♪ 2  In Bloom ♥", "4:14", 50).as_str(),
                track_row("  3  Come As You Are", "3:38", 50).as_str(),
                track_row("  4  Breed ♥", "3:03", 50).as_str(),
            ]
        );
        assert_eq!(buffer[(14, 8)].fg, ACCENT);
        // A long name is truncated so that ♥ and the duration fit.
        let buffer = render(&state, now, 21, 14);
        assert_eq!(row(&buffer, 8), "♪ 2  In Bloom ♥  4:14");
        let buffer = render(&state, now, 20, 14);
        assert_eq!(row(&buffer, 8), "♪ 2  In Blo… ♥  4:14");
    }

    #[test]
    fn liked_radio_tracks_have_heart_after_name() {
        let now = Instant::now();
        let mut state = synthwave_radio(now);
        liked(
            &mut state,
            now,
            &["spotify:track:sunset", "spotify:track:ontherun"],
        );
        let buffer = render(&state, now, 70, 15);
        assert_eq!(
            rows(&buffer)[7..11],
            [
                "♪ The Midnight · Sunset ♥  Endless Summer",
                "  FM-84 · Running in the Night  Atlas",
                "  Timecop1983 · On the Run ♥  Night Drive ●",
                "  Gunship · Tech Noir  Gunship",
            ]
        );
        assert_eq!(buffer[(24, 7)].fg, ACCENT);
        assert!(row(&buffer, 1).starts_with("The Midnight / Endless Summer / Sunset ♥"));
    }

    #[test]
    fn top_row_confirms_like_and_unlike() {
        let now = Instant::now();
        let mut state = State::new(shelf()).with_likes(true);
        playing(&mut state, now, 0);
        let actions = state.update(Event::Key(Key::Ctrl('l')), now);
        assert_eq!(
            actions,
            [Action::SetLiked {
                uri: "spotify:track:everlong".to_owned(),
                liked: true,
            }]
        );
        state.update(
            Event::LikeChanged {
                uri: "spotify:track:everlong".to_owned(),
                liked: true,
                result: Ok(()),
            },
            now,
        );
        let buffer = render(&state, now, 50, 6);
        assert_eq!(
            row(&buffer, 0),
            format!("DECK{}♥ liked: Everlong", " ".repeat(29))
        );
        // ♥ in the accent colour, text dimmed and the name normal.
        assert_eq!(buffer[(33, 0)].fg, ACCENT);
        assert_eq!(buffer[(35, 0)].fg, MUTED);
        assert_eq!(buffer[(42, 0)].fg, Color::Reset);

        state.update(Event::Key(Key::Ctrl('l')), now);
        state.update(
            Event::LikeChanged {
                uri: "spotify:track:everlong".to_owned(),
                liked: false,
                result: Ok(()),
            },
            now,
        );
        assert_eq!(
            row(&render(&state, now, 50, 6), 0),
            format!("DECK{}unliked: Everlong", " ".repeat(29))
        );
        assert_eq!(
            row(&render(&state, now + crate::app::NOTICE_TIME, 50, 6), 0),
            "DECK"
        );
    }

    /// The help row (bottom row) in a 100-column Deck.
    fn help_row(state: &State, now: Instant) -> String {
        row(&render(state, now, 100, 20), 19)
    }

    /// Everlong is playing and is liked.
    fn liked_playing(state: &mut State, now: Instant) {
        playing(state, now, 0);
        liked(state, now, &["spotify:track:everlong"]);
    }

    /// Everlong is playing from the album The Colour And The Shape (restored playback
    /// state), so n leads to the album.
    fn playing_album(state: &mut State, now: Instant) {
        let saved = serde_json::from_value(serde_json::json!({
            "album": { "uri": colour().uri, "len": 13, "index": 3, "album": colour() },
            "current": "album",
            "now_playing": {
                "uri": "spotify:track:everlong",
                "artist": "Foo Fighters",
                "album": "The Colour And The Shape",
                "track": "Everlong",
                "duration_ms": 250_000,
            },
            "position_ms": 0,
        }))
        .unwrap();
        state.update(Event::Restore(Box::new(saved)), now);
    }

    fn liked_playing_album(state: &mut State, now: Instant) {
        playing_album(state, now);
        liked(state, now, &["spotify:track:everlong"]);
    }

    #[test]
    fn help_shows_now_playing_only_when_n_leads_somewhere() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        // The player reported the track, but Deck does not know the album: n leads nowhere.
        playing(&mut state, now, 0);
        assert!(!help_row(&state, now).contains("[n]"));

        playing_album(&mut state, now);
        assert_eq!(
            help_row(&state, now),
            "[Space] pause  [←→] track  [n] now playing  [l] like playing  [f] search  [u] show queue  [q] quit"
        );
        // In a narrower window quit goes first, then [n] now playing.
        assert_eq!(
            row(&render(&state, now, 92, 20), 19),
            "[Space] pause  [←→] track  [n] now playing  [l] like playing  [f] search  [u] show queue"
        );
        assert_eq!(
            row(&render(&state, now, 89, 20), 19),
            "[Space] pause  [←→] track  [l] like playing  [f] search  [u] show queue  [q] quit"
        );
        // In search, Ctrl+N.
        state.update(Event::Key(Key::Ctrl('f')), now);
        state.update(Event::Key(Key::Esc), now);
        state.update(Event::Key(Key::Ctrl('q')), now);
        assert_eq!(
            help_row(&state, now),
            "[d] remove  [n] now playing  [l] like playing  [Space] pause  [←→] track  [Esc] back  [q] quit"
        );
    }

    #[test]
    fn help_shows_like_or_unlike_only_while_something_plays() {
        let now = Instant::now();
        let mut state = nevermind_state(now);
        assert_eq!(
            help_row(&state, now),
            "[Enter] play from here  [e] add to queue  [a] remove from shelf  [Esc] back  [q] quit"
        );
        playing(&mut state, now, 0);
        assert_eq!(
            help_row(&state, now),
            "[Enter] play from here  [e] add to queue  [a] remove from shelf  [l] like playing  [Esc] back"
        );
        liked(&mut state, now, &["spotify:track:everlong"]);
        assert_eq!(
            help_row(&state, now),
            "[Enter] play from here  [e] add to queue  [a] remove from shelf  [l] unlike playing  [Esc] back"
        );
        // In search the Ctrl forms, because letters type into the search.
        state.update(Event::Key(Key::Ctrl('f')), now);
        assert_eq!(
            help_row(&state, now),
            "[Ctrl+E] add to queue  [Ctrl+A] add to shelf  [Ctrl+L] unlike playing  [Esc] back  [Ctrl+C] quit"
        );
    }

    #[test]
    fn shelf_help_shows_d_only_on_a_genre_row() {
        let now = Instant::now();
        let mut state = State::new(shelf()).with_favorites(vec!["lofi".into()]);
        assert_eq!(
            help_row(&state, now),
            "[Space] pause  [←→] track  [d] remove from home  [f] search  [u] show queue  [q] quit"
        );
        // On the Genres row and on an artist, d does nothing.
        state.update(Event::Key(Key::Down), now);
        assert!(!help_row(&state, now).contains("[d]"));
        state.update(Event::Key(Key::Down), now);
        assert!(!help_row(&state, now).contains("[d]"));
        // While something is playing, [n] does not disappear as the selection moves: on a
        // genre row Space and ←→ are left out.
        liked_playing_album(&mut state, now);
        assert_eq!(
            help_row(&state, now),
            "[Space] pause  [←→] track  [n] now playing  [l] unlike playing  [f] search  [u] show queue  [q] quit"
        );
        state.update(Event::Key(Key::Up), now);
        state.update(Event::Key(Key::Up), now);
        assert_eq!(
            help_row(&state, now),
            "[d] remove from home  [n] now playing  [l] unlike playing  [f] search  [u] show queue  [q] quit"
        );
    }

    #[test]
    fn shelf_help_shows_queue_as_u_before_quit() {
        let now = Instant::now();
        let mut state = State::new(shelf());
        assert_eq!(
            help_row(&state, now),
            "[Space] pause  [←→] track  [f] search  [u] show queue  [q] quit"
        );
        playing(&mut state, now, 0);
        assert_eq!(
            help_row(&state, now),
            "[Space] pause  [←→] track  [l] like playing  [f] search  [u] show queue  [q] quit"
        );
        // u opens the queue just like Ctrl+Q.
        state.update(Event::Key(Key::Char('u')), now);
        assert_eq!(row(&render(&state, now, 100, 20), 4), "Queue");
    }

    /// Every view's help row fits in 100 columns with the longest labels
    /// (`unlike playing`, `remove from shelf`) while something plays from an album.
    /// `[n] now playing` fits on the shelf, in the queue and in Genres. In search and an
    /// artist's albums quit is left out, in the track list quit, Space and ←→. No view
    /// has `[↑↓] select`.
    #[test]
    fn every_help_row_fits_100_columns_with_unlike() {
        let now = Instant::now();
        let mut rows = Vec::new();

        let mut state = State::new(shelf());
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Enter), now);
        rows.push(help_row(&state, now));

        // On the shelf with a home genre selected.
        let mut state = State::new(shelf()).with_favorites(vec!["lofi".into()]);
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));

        let mut state = curated_view(2, now);
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));

        let mut state = nevermind_state(now);
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));

        let mut state = queued_state(now);
        state.update(Event::Key(Key::Ctrl('q')), now);
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));

        // Search and an artist's albums from Spotify: The Colour And The Shape is on the
        // shelf.
        let mut state = State::new(vec![colour()]);
        state.update(Event::Key(Key::Ctrl('f')), now);
        state.update(Event::Key(Key::Char('f')), now);
        let later = now + Duration::from_millis(300);
        state.update(Event::Tick, later);
        state.update(
            Event::Searched {
                query: "f".to_owned(),
                result: Ok(SearchResults {
                    artists: vec![Artist {
                        id: "id-foo-fighters".to_owned(),
                        name: "Foo Fighters".to_owned(),
                    }],
                    albums: vec![colour()],
                }),
            },
            later,
        );
        liked_playing_album(&mut state, later);
        state.update(Event::Key(Key::Down), later);
        rows.push(help_row(&state, later));
        state.update(Event::Key(Key::Up), later);
        state.update(Event::Key(Key::Enter), later);
        state.update(
            Event::ArtistAlbums {
                artist_id: "id-foo-fighters".to_owned(),
                result: Ok(vec![colour()]),
            },
            later,
        );
        rows.push(help_row(&state, later));

        // In the Genres view with lofi (on home) selected.
        let mut state = State::new(Vec::new()).with_favorites(vec!["lofi".into()]);
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Enter), now);
        let lofi = genres::all().iter().position(|g| g.id == "lofi").unwrap();
        for _ in 0..lofi {
            state.update(Event::Key(Key::Down), now);
        }
        liked_playing_album(&mut state, now);
        rows.push(help_row(&state, now));

        // In radio with Night Drive (on the shelf) selected.
        let mut state = synthwave_radio(now);
        state.update(Event::Key(Key::Down), now);
        state.update(Event::Key(Key::Down), now);
        liked_playing(&mut state, now);
        rows.push(help_row(&state, now));

        assert_eq!(
            rows,
            [
                "[Space] pause  [←→] track  [n] now playing  [l] unlike playing  [f] search  [u] show queue  [q] quit",
                "[e] add to queue  [d] remove from shelf  [l] unlike playing  [Space] pause  [←→] track  [Esc] back",
                "[d] remove from home  [n] now playing  [l] unlike playing  [f] search  [u] show queue  [q] quit",
                "[e] add to queue  [a] remove from shelf  [d] not for me  [l] unlike playing  [Esc] back",
                "[Enter] play from here  [e] add to queue  [a] remove from shelf  [l] unlike playing  [Esc] back",
                "[d] remove  [n] now playing  [l] unlike playing  [Space] pause  [←→] track  [Esc] back  [q] quit",
                "[Ctrl+E] add to queue  [Ctrl+A] remove from shelf  [Ctrl+L] unlike playing  [Esc] back",
                "[e] add to queue  [a] remove from shelf  [l] unlike playing  [Space] pause  [←→] track  [Esc] back",
                "[Enter] play radio  [a] remove from home  [n] now playing  [l] unlike playing  [Esc] back",
                "[Enter] play  [e] add to queue  [a] remove from shelf  [r] new radio  [l] unlike playing  [Esc] back",
            ]
        );
        // No row is cut: even an undrawn row is at most 100 columns. Outside search the
        // queue is the letter u, not Ctrl+Q.
        for row in &rows {
            assert!(row.width() <= 100, "{row}");
            assert!(!row.contains("[Ctrl+Q]"), "{row}");
            assert!(!row.contains("[↑↓]"), "{row}");
        }
    }
}
