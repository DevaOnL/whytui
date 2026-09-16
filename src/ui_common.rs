use crate::api::{PlaylistDetails, SongDetails};
use crate::features::{LrcLine, fetch_synced_lyrics};
use crate::{LYRIC_OFFSET, Track, UI_MODE, player};
use colored::*;
use crossterm::execute;
use std::io::{Write, stdout};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub static SONG_MONITOR: RwLock<Option<Arc<AtomicBool>>> = RwLock::new(None);

struct LyricsState {
    generation: u64,
    track_key: Option<String>,
    lines: Vec<LrcLine>,
    enrichment_complete: bool,
}

impl LyricsState {
    const fn new() -> Self {
        Self {
            generation: 0,
            track_key: None,
            lines: Vec::new(),
            enrichment_complete: false,
        }
    }

    fn begin_track(&mut self, track_key: String) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.track_key = Some(track_key);
        self.lines.clear();
        self.enrichment_complete = false;
        self.generation
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.track_key = None;
        self.lines.clear();
        self.enrichment_complete = false;
    }

    fn owns(&self, generation: u64, track_key: &str) -> bool {
        self.generation == generation && self.track_key.as_deref() == Some(track_key)
    }
}

static LYRICS_STATE: RwLock<LyricsState> = RwLock::new(LyricsState::new());

/// Abort handle for the in-flight lyric fetch+translate task.
///
/// The generation checks in `spawn_lyrics_fetcher` stop a superseded task from *publishing* stale
/// lyrics, but on their own they let it run every LRCLIB and Google-translate request to its timeout —
/// so rapid track changes piled up dozens of wasted in-flight HTTP requests. Keeping the current
/// task's handle here lets a track change, `stop_lyrics`, or shutdown actually *cancel* that work;
/// dropping the outer task also drops its inner `JoinSet`, cancelling the child requests.
static LYRIC_TASK: std::sync::Mutex<Option<tokio::task::AbortHandle>> = std::sync::Mutex::new(None);

/// Cancel the current lyric task, if any.
///
/// Holds only `LYRIC_TASK`, briefly, and never while another lock is held — `abort()` itself takes no
/// lock — so it introduces no lock-ordering hazard against `LYRICS_STATE`/`SONG_MONITOR`.
fn abort_lyric_task() {
    let handle = LYRIC_TASK.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(handle) = handle {
        handle.abort();
    }
}

/// Install the new lyric task's handle, aborting whatever it replaces.
fn set_lyric_task(handle: tokio::task::AbortHandle) {
    let previous = LYRIC_TASK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(handle);
    if let Some(previous) = previous {
        previous.abort();
    }
}

pub static TITLE_SCROLL: RwLock<usize> = RwLock::new(0);
pub static LAST_SCROLL: RwLock<Option<Instant>> = RwLock::new(None);
/// A second, independent marquee cursor. ui3 scrolls the title AND the artist in the same frame; with
/// only the one shared cursor the artist was advanced modulo the *title's* length, clamping it to the
/// title's cycle so a long artist under a shorter title never scrolled far enough to reveal its tail.
/// Modes that scroll a single field (ui1/ui2) keep using TITLE_SCROLL.
static ARTIST_SCROLL: RwLock<usize> = RwLock::new(0);
static ARTIST_LAST_SCROLL: RwLock<Option<Instant>> = RwLock::new(None);

pub static LYRIC_DISPLAY_MODE: AtomicU8 = AtomicU8::new(0);
pub static STATUS_LINE: RwLock<String> = RwLock::new(String::new());

pub static BASE_STATUS: RwLock<Option<String>> = RwLock::new(None);
pub static STATUS_TIMEOUT: RwLock<Option<Instant>> = RwLock::new(None);
/// Whatever the status line is currently showing, so it can be restored after a repaint covers it.
static CURRENT_STATUS_TEXT: RwLock<Option<String>> = RwLock::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LyricVariantAvailability {
    Loading,
    Available,
    Unavailable,
}

pub struct LyricFrame<'a> {
    pub lines: &'a [LrcLine],
    pub current_idx: Option<usize>,
    pub mode: u8,
    pub notice: Option<&'static str>,
}

fn variant_availability(
    lines: &[LrcLine],
    enrichment_complete: bool,
    mode: u8,
) -> LyricVariantAvailability {
    if mode == 0 {
        return LyricVariantAvailability::Available;
    }
    if !enrichment_complete {
        return LyricVariantAvailability::Loading;
    }

    let mut saw_lyric = false;
    let complete = lines
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .all(|line| {
            saw_lyric = true;
            line.text_for_mode(mode)
                .map(|text| !text.trim().is_empty())
                .unwrap_or(false)
        });

    if saw_lyric && complete {
        LyricVariantAvailability::Available
    } else {
        LyricVariantAvailability::Unavailable
    }
}

fn selected_lyric_notice(mode: u8, availability: LyricVariantAvailability) -> Option<&'static str> {
    match (mode, availability) {
        (1, LyricVariantAvailability::Loading) => Some("Fetching romanization..."),
        (1, LyricVariantAvailability::Unavailable) => Some("No romanization found"),
        (2, LyricVariantAvailability::Loading) => Some("Fetching translation..."),
        (2, LyricVariantAvailability::Unavailable) => Some("No translation found"),
        _ => None,
    }
}

pub fn cycle_lyric_display_mode() -> (u8, LyricVariantAvailability) {
    let previous = LYRIC_DISPLAY_MODE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |mode| {
            Some((mode + 1) % 3)
        })
        .unwrap_or(0);
    let mode = (previous + 1) % 3;
    let state = LYRICS_STATE.read().unwrap_or_else(|e| e.into_inner());
    (
        mode,
        variant_availability(&state.lines, state.enrichment_complete, mode),
    )
}

pub fn stop_lyrics() {
    // Cancel any in-flight fetch first (standalone lock, nothing else held), then invalidate state.
    abort_lyric_task();
    // Same order as ensure_monitor_for_track (lyrics state, then monitor).
    let mut lyrics = LYRICS_STATE.write().unwrap_or_else(|e| e.into_inner());
    let mut monitor_guard = SONG_MONITOR.write().unwrap_or_else(|e| e.into_inner());

    if let Some(stop_signal) = monitor_guard.take() {
        stop_signal.store(true, Ordering::Relaxed);
    }
    lyrics.invalidate();
}

pub fn clear_lyrics() {
    LYRICS_STATE
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .invalidate();
}

use std::cmp::min;

fn _draw_status_line_locked(status: Option<String>) {
    if crate::shutdown_started() {
        return;
    }

    // Remember what is on screen so a full repaint can put it back; see repaint_status_line_locked.
    *CURRENT_STATUS_TEXT
        .write()
        .unwrap_or_else(|e| e.into_inner()) = status.clone();

    if UI_MODE.load(Ordering::Relaxed) == 2 {
        return;
    }

    let mut line = STATUS_LINE.write().unwrap_or_else(|e| e.into_inner());

    let global_indent = get_padding(50);
    let inner_width: usize = 31;

    // Clip and measure by display width. Byte length over-counts multi-byte characters, so a
    // status containing any of them lost its padding, and nothing clipped an over-long message —
    // it just shoved the frame's right-hand art off the end of the line.
    // Leave a blank cell either side of the text: a message that filled all 31 columns ran straight
    // into the frame's ░ edges and made the art look damaged.
    let raw_text = truncate_safe(&status.unwrap_or_default(), inner_width.saturating_sub(2));
    let styled_text = raw_text.blue().dimmed().bold();

    let visible_len = min(get_visual_width(&raw_text), inner_width);
    let total_padding = inner_width - visible_len;
    let pad_l_len = total_padding / 2;
    let pad_r_len = total_padding - pad_l_len;

    let pad_l = " ".repeat(pad_l_len);
    let pad_r = " ".repeat(pad_r_len);

    let bottom_spacer = " ".repeat(inner_width);

    let left_art = "      ░   ░".blue().dimmed();
    let right_art = " ░   ░".blue().dimmed();
    let _ = execute!(
        std::io::stdout(),
        crossterm::cursor::SavePosition,
        crossterm::cursor::MoveTo(0, 9),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine),
    );

    *line = format!(
        "\r{indent}{l_art}{pl}{text}{pr}{r_art}\r\n{indent}{l_art}{spacer}{r_art}",
        indent = global_indent,
        l_art = left_art,
        r_art = right_art,
        pl = pad_l,
        pr = pad_r,
        text = styled_text,
        spacer = bottom_spacer
    );

    print!("{}", *line);
    let _ = std::io::stdout().flush();

    let _ = execute!(std::io::stdout(), crossterm::cursor::RestorePosition);
}

fn _draw_status_line(status: Option<String>) {
    let _guard = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    _draw_status_line_locked(status);
}

/// Re-emit the status line after something has drawn over it.
///
/// The banner art spans the status row, so every `refresh_ui` erased whatever message was showing —
/// and since only the 1s timeout ever redraws the line, the message was simply lost. Anything set
/// before a repaint (a search failure, a lyric-mode change, a library error) was never seen.
///
/// The caller must already hold `OUTPUT_LOCK`, hence `_locked`.
pub fn repaint_status_line_locked() {
    let text = CURRENT_STATUS_TEXT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    _draw_status_line_locked(text);
}

/// How long a transient message stays on screen before the quality/base status returns.
///
/// The status frame sits inside the banner's lower art, so a message parked there for too long reads
/// as the art being broken rather than as a notification. Four seconds was too intrusive for that;
/// the original one second was too short to actually read. This is the compromise.
const STATUS_MESSAGE_LINGER: Duration = Duration::from_millis(1800);

// Public wrapper for temporary status updates
pub fn set_status_line(status: Option<String>) {
    let _guard = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_status_line_locked(status);
}

fn set_status_line_locked(status: Option<String>) {
    if crate::shutdown_started() {
        return;
    }

    *STATUS_TIMEOUT.write().unwrap_or_else(|e| e.into_inner()) =
        Some(Instant::now() + STATUS_MESSAGE_LINGER);
    _draw_status_line_locked(status);
}

fn get_padding(content_width: usize) -> String {
    let (cols, _) = crossterm::terminal::size().unwrap_or((80, 24));
    let term_width = cols as usize;
    let padding = term_width.saturating_sub(content_width) / 2;
    " ".repeat(padding)
}

pub fn get_banner_art() -> String {
    let art = r#"
   █     █░ ██░ ██▓ ██   ██▓ ▄███████▓ █    ██  ██▓
  ▓█░ █ ░█░▓██░ ██▒ ▒██  ██▒▓   ██▒ ▓▒ ██  ▓██ ▒▓██▒
  ▒█░ █ ░█ ▒██▀▀██░  ▒██ ██░▒  ▓██░ ▒░▓██  ▒██ ░▒██▒
  ░█░ █ ░█ ░▓█ ░██   ░ ▐██▓░░  ▓██▓ ░ ▓▓█  ░██ ░░██░
  ░░██▒██▓ ░▓█▒░██▓  ░ ██▒▓░   ▒██▒ ░ ▒▒█████▓  ░██░
  ░ ▓░▒ ▒   ▒ ░░▒░▒   ██▒▒▒    ▒ ░░   ░▒▓▒ ▒ ▒  ░▓
    ▒ ░ ░   ▒ ░▒░ ░ ▓██ ░▒░      ░    ░░▒░ ░ ░   ▒ ░
    ░   ░   ░  ░░ ░ ▒ ▒ ░░      ░      ░░░   ░   ▒ ░
        ░   ░                                ░   ░
        ░   ░                                ░   ░
"#;

    let pad = get_padding(54);
    let output = art
        .lines()
        .map(|l| format!("\r{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n");

    output.blue().dimmed().to_string()
}

pub fn dur_to_secs(d: Duration) -> f64 {
    d.as_millis() as f64 / 1000.0
}

pub fn get_scrolling_text(text: &str, width: usize) -> String {
    scroll_text(text, width, &TITLE_SCROLL, &LAST_SCROLL)
}

/// Like [`get_scrolling_text`] but on an independent cursor, for a second field scrolled in the same
/// frame (ui3's artist line, drawn alongside its title). Sharing one cursor bounded the longer field
/// to the shorter field's cycle length, so its end never scrolled into view.
pub fn get_scrolling_text_secondary(text: &str, width: usize) -> String {
    scroll_text(text, width, &ARTIST_SCROLL, &ARTIST_LAST_SCROLL)
}

fn scroll_text(
    text: &str,
    width: usize,
    scroll_state: &RwLock<usize>,
    last_state: &RwLock<Option<Instant>>,
) -> String {
    // Sanitise before anything else: this path never touches truncate_safe, so it is a second
    // untrusted-text boundary (the artist field) that would otherwise leak control bytes.
    let text = sanitize_display(text);
    let text = text.as_str();
    // `width` is a number of terminal cells, so compare against the display width. Counting chars
    // meant a CJK title that already overflowed its slot was reported as fitting.
    if get_visual_width(text) <= width {
        return text.to_string();
    }

    let mut scroll = scroll_state.write().unwrap_or_else(|e| e.into_inner());
    let mut last_lock = last_state.write().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();

    if last_lock.is_none() {
        *last_lock = Some(now);
    }

    let last = last_lock.get_or_insert(now);
    if now.duration_since(*last) >= Duration::from_millis(300) {
        *scroll = (*scroll + 1) % (text.chars().count() + 2);
        *last = now;
    }

    let padded = format!("{}  {}", text, text);
    let chars: Vec<char> = padded.chars().collect();
    let start = *scroll % chars.len();

    // Fill by display width rather than by char count: taking `width` wide chars produced a string
    // twice as wide as the slot, which then ran over whatever was drawn to its right. `.take` bounds
    // this to one rotation so a zero-width char cannot spin the cycle forever.
    let mut out = String::new();
    let mut used = 0;
    for c in chars.iter().cycle().skip(start).take(chars.len()) {
        let w = char_width(*c);
        if used + w > width {
            break;
        }
        out.push(*c);
        used += w;
    }
    out
}

/// First row a chooser may use, so it never covers the now-playing title or the progress bar.
const CHOOSER_FIRST_ROW: u16 = 16;

/// Trim a chooser's lines to `budget` rows, keeping the prompt (the last line) and, when present, the
/// header (the first line), and dropping the middle entries behind a "..." marker.
///
/// The prompt is what tells the user how to choose ("Select (1-N)"), so it must survive. The previous
/// version simply truncated the tail, which discarded the prompt itself — on the recommended 80x24 a
/// library of 8+ playlists showed a list with no visible way to pick from it.
fn trim_chooser_lines(mut lines: Vec<String>, budget: usize, has_header: bool) -> Vec<String> {
    if budget == 0 {
        return Vec::new();
    }
    if lines.len() <= budget {
        return lines;
    }

    // The prompt is always the last line; keep it however tight the budget is.
    let prompt = lines
        .pop()
        .expect("chooser lines always include the prompt");
    if budget == 1 {
        return vec![prompt];
    }

    let header = if has_header && !lines.is_empty() {
        Some(lines.remove(0))
    } else {
        None
    };
    // `lines` now holds only the middle entries.
    let reserved = header.is_some() as usize + 1; // header? + prompt
    let room = budget - reserved; // budget >= 2 and reserved <= 2, so this cannot underflow

    let mut out = Vec::with_capacity(budget);
    if let Some(h) = header {
        out.push(h);
    }
    if room >= 2 {
        // one row spent on the "..." marker, the rest on entries
        out.extend(lines.into_iter().take(room - 1));
        out.push("...".to_string());
    } else if room == 1 {
        out.push("...".to_string());
    }
    out.push(prompt);
    out
}

/// Draw a chooser anchored to the bottom of the screen.
///
/// Absolute positioning, one `Clear` per row, and never a trailing newline on the last line. Printing
/// a listing at the cursor — which is where the banner parks it, on the very last row — scrolled the
/// terminal once per line on anything as short as the recommended 80x24. That pushed the first entries
/// off the top (a "Select (1-5)" prompt with only 3, 4 and 5 still visible) and permanently shifted
/// every absolutely-positioned element. Anchoring here keeps the banner, the now-playing line and the
/// progress bar visible above the list.
fn draw_chooser(header: Option<String>, entries: Vec<String>, prompt: String) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = cols as usize;
    let last_row = rows.saturating_sub(1);

    let has_header = header.is_some();
    let mut lines: Vec<String> = Vec::new();
    if let Some(h) = header {
        lines.push(h);
    }
    lines.extend(entries);
    lines.push(prompt);

    // Trim to the vertical budget while ALWAYS keeping the prompt (and the header, if any); only the
    // middle entries are dropped, behind a "..." marker.
    let budget = (last_row + 1).saturating_sub(CHOOSER_FIRST_ROW) as usize;
    let lines = trim_chooser_lines(lines, budget, has_header);

    // Sit directly under the queue, where listings used to appear, rather than pinned to the bottom of
    // the terminal — bottom-anchoring pushed them several rows lower than they had always been on a
    // tall terminal. Only pulled up when the list would not otherwise fit.
    let preferred_start = match UI_MODE.load(Ordering::Relaxed) {
        0 => 26, // ui1: queue occupies rows 20..24
        1 => 24, // ui2: queue occupies rows 17..22
        _ => CHOOSER_FIRST_ROW,
    };
    let highest_that_fits = (last_row + 1).saturating_sub(lines.len() as u16);
    let start_row = preferred_start
        .min(highest_that_fits)
        .max(CHOOSER_FIRST_ROW.min(last_row));

    for (i, line) in lines.iter().enumerate() {
        let row = start_row + i as u16;
        if row > last_row {
            break;
        }
        let _ = execute!(
            stdout(),
            crossterm::cursor::MoveTo(0, row),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine),
        );
        // width-clipped so a long title cannot wrap and push the rest of the list down a row
        print!("\r{}", truncate_safe(line, width.saturating_sub(1)));
    }
    let _ = stdout().flush();
}

pub fn show_songs(list: &[SongDetails]) {
    let entries = list
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "{}. {} [{}] [{}]",
                i + 1,
                s.title,
                s.artists.join(", "),
                s.duration
            )
        })
        .collect();
    draw_chooser(None, entries, format!("~ Select (1-{}): ", list.len()));
}

pub fn show_playlists(list: &[PlaylistDetails]) {
    let entries = list
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if p.count.is_empty() {
                format!("{}. {}", i + 1, p.title)
            } else {
                format!("{}. {} [{}]", i + 1, p.title, p.count)
            }
        })
        .collect();
    draw_chooser(
        Some("--- YOUR LIBRARY ---".to_string()),
        entries,
        format!("~ Select (1-{}): ", list.len()),
    );
}

/// Draw one page of the library browser, keeping the banner and player visible above it.
pub fn show_pager_page(header: String, songs: &[SongDetails], end: bool) {
    let entries = if end {
        vec![" --- End ---".to_string()]
    } else {
        songs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    "{}. {} [{}] [{}]",
                    i + 1,
                    s.title,
                    s.artists.join(", "),
                    s.duration
                )
            })
            .collect()
    };
    let prompt = if songs.is_empty() {
        // Nothing on this page, so don't offer a selection range that cannot be satisfied.
        "~ [p]rev to go back: ".to_string()
    } else {
        format!("~ Select (1-{}): ", songs.len())
    };
    draw_chooser(Some(header), entries, prompt);
}

/// Restart the progress/lyrics monitor thread if `track` is not the one already being monitored.
///
/// This is the single owner of `CURRENT_LYRIC_TRACK_KEY` + `SONG_MONITOR` on the render path. Each
/// `uiN::load_banner` used to inline its own copy of this logic, which is how a callee ended up
/// re-locking `CURRENT_LYRIC_TRACK_KEY` on the same thread and deadlocking; keeping both
/// acquisitions in one place is what prevents that from coming back.
pub fn ensure_monitor_for_track<F>(track: &Track, draw_callback: F)
where
    F: for<'a> Fn(&str, &str, &str, f64, f64, LyricFrame<'a>) + Send + 'static,
{
    if crate::shutdown_started() || track.title.is_empty() {
        return;
    }

    let key = crate::track_identity::track_key(track);
    let generation = {
        let mut state = LYRICS_STATE.write().unwrap_or_else(|e| e.into_inner());
        if state.track_key.as_ref() == Some(&key) {
            return;
        }
        let generation = state.begin_track(key);
        if track.url.is_empty() {
            state.enrichment_complete = true;
        }
        generation
    };

    // New track: cancel the previous track's still-running lyric fetch so it stops hitting LRCLIB and
    // Google translate. If this track has a URL, the replacement spawned below registers its own
    // handle; a URL-less track (an offline file) leaves no task, which is correct. Called with no lock
    // held, so it cannot invert the SONG_MONITOR ordering taken below.
    abort_lyric_task();

    // New track, so any manual lyric sync the user dialled in for the previous one no longer applies.
    LYRIC_OFFSET.store(0, Ordering::Relaxed);

    // Reset the marquee too, or a new track with a long title picks up mid-scroll at the previous
    // track's offset and appears to start part-way through its own name.
    *TITLE_SCROLL.write().unwrap_or_else(|e| e.into_inner()) = 0;
    *LAST_SCROLL.write().unwrap_or_else(|e| e.into_inner()) = None;
    *ARTIST_SCROLL.write().unwrap_or_else(|e| e.into_inner()) = 0;
    *ARTIST_LAST_SCROLL
        .write()
        .unwrap_or_else(|e| e.into_inner()) = None;

    let mut monitor_guard = SONG_MONITOR.write().unwrap_or_else(|e| e.into_inner());
    if let Some(stop_signal) = monitor_guard.take() {
        stop_signal.store(true, Ordering::Relaxed);
    }
    *monitor_guard = Some(start_monitor_thread_with_fetch(
        track.clone(),
        draw_callback,
        Some(generation),
    ));
}

/// Replace only the renderer monitor after a layout switch. The lyric generation and enriched lines
/// remain untouched, so changing views cannot refetch or temporarily revert them to originals.
pub fn restart_monitor_for_view<F>(track: &Track, draw_callback: F)
where
    F: for<'a> Fn(&str, &str, &str, f64, f64, LyricFrame<'a>) + Send + 'static,
{
    if crate::shutdown_started() {
        return;
    }

    let key = crate::track_identity::track_key(track);
    let state = LYRICS_STATE.read().unwrap_or_else(|e| e.into_inner());
    if state.track_key.as_ref() != Some(&key) {
        drop(state);
        ensure_monitor_for_track(track, draw_callback);
        return;
    }

    let mut monitor = SONG_MONITOR.write().unwrap_or_else(|e| e.into_inner());
    if let Some(stop) = monitor.take() {
        stop.store(true, Ordering::Relaxed);
    }
    *monitor = Some(start_monitor_thread_with_fetch(
        track.clone(),
        draw_callback,
        None,
    ));
}

fn start_monitor_thread_with_fetch<F>(
    track: Track,
    draw_callback: F,
    fetch_generation: Option<u64>,
) -> Arc<AtomicBool>
where
    F: for<'a> Fn(&str, &str, &str, f64, f64, LyricFrame<'a>) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));

    if !track.url.is_empty()
        && let Some(generation) = fetch_generation
    {
        spawn_lyrics_fetcher(track.clone(), generation);
    }

    // 3. Start the UI Loop
    let stop_clone = stop.clone();
    let track_title = track.title.clone();
    let artist_str = track.artists.join(", ");
    let _track_album = track.album.clone();
    // fallback only: mpv's own duration is authoritative once it has opened the stream
    let metadata_tot: f64 = duration_to_seconds(&track.duration);

    thread::spawn(move || {
        if crate::shutdown_started() {
            return;
        }

        // Show quality status in the spawned thread, not in refresh_ui() lock context
        update_quality_status();

        while !stop_clone.load(Ordering::Relaxed) && !crate::shutdown_started() {
            // get current progress from playertitle
            let (playback_generation, curr, player_tot) =
                player::get_time_info().unwrap_or((0, 0.0, 0.0));
            // This is also the only place playback position is observed, so it doubles as the input
            // for "was this track actually listened to?" when deciding whether to keep a cached copy.
            player::note_progress(playback_generation, curr, player_tot);
            // Prefer what mpv reports; the metadata duration is missing ("0:00") for untagged
            // offline files and for search results the API did not give a length for, which
            // otherwise leaves the progress bar stuck empty for the whole track.
            let tot = if player_tot > 0.0 {
                player_tot
            } else {
                metadata_tot
            };
            //draw screen
            //
            // LOCK ORDER: OUTPUT_LOCK is the outermost lock in this program — take it *before*
            // LYRICS, never after. refresh_ui() holds OUTPUT_LOCK and can reach LYRICS through
            // start_monitor_thread(), so reading LYRICS first here (as this loop used to) is a
            // lock-order inversion that deadlocks the whole app every 300ms of exposure.
            if !crate::PROMPT_ACTIVE.load(Ordering::SeqCst) {
                check_status_timeout();
                let _guard = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

                // Re-check after taking the lock: we may have waited here while the track changed,
                // and painting now would stamp this dead track's frame over the new UI.
                if stop_clone.load(Ordering::Relaxed) || crate::shutdown_started() {
                    break;
                } else if !crate::PROMPT_ACTIVE.load(Ordering::SeqCst) {
                    let lyrics = LYRICS_STATE.read().unwrap_or_else(|e| e.into_inner());
                    let mode = LYRIC_DISPLAY_MODE.load(Ordering::Relaxed) % 3;
                    let availability =
                        variant_availability(&lyrics.lines, lyrics.enrichment_complete, mode);
                    let notice = selected_lyric_notice(mode, availability);

                    let current_idx = get_current_lyric_index(&lyrics.lines, curr);

                    draw_callback(
                        &track_title,
                        &artist_str,
                        &track_title,
                        curr,
                        tot,
                        LyricFrame {
                            lines: &lyrics.lines,
                            current_idx,
                            mode,
                            notice,
                        },
                    );
                }
            }

            thread::sleep(Duration::from_millis(300));
        }
    });

    stop
}

fn spawn_lyrics_fetcher(track: Track, generation: u64) {
    let handle = tokio::spawn(async move {
        let expected_key = crate::track_identity::track_key(&track);
        let result = fetch_synced_lyrics(&track).await;
        // NOTE: LYRIC_OFFSET is reset by ensure_monitor_for_track when the track changes, not here.
        // Zeroing it after the await threw away a sync adjustment the user made with '['/']' while
        // the fetch was still in flight — and a stale fetcher reset it for the *next* track too.

        let mut lines = match result {
            Ok(parsed) if !parsed.is_empty() => parsed,
            _ => {
                {
                    let mut state = LYRICS_STATE.write().unwrap_or_else(|e| e.into_inner());
                    if !state.owns(generation, &expected_key) {
                        return;
                    }
                    state.enrichment_complete = true;
                }
                set_status_line_if_current(
                    generation,
                    &expected_key,
                    "No lyrics found >_<".to_string(),
                );
                return;
            }
        };

        // Show the original lyrics right away, then fetch the English translation and romaji in the
        // background and swap the enriched copy in. A non-English song is therefore readable at once
        // and the [t] toggle (original -> romaji -> English) starts working a moment later, instead
        // of the lyrics not appearing at all until every line has been round-tripped to Google.
        {
            let mut state = LYRICS_STATE.write().unwrap_or_else(|e| e.into_inner());
            if !state.owns(generation, &expected_key) {
                return;
            }
            state.lines = lines.clone();
        }

        // A long song plus a throttled service must not leave the mode in "loading" indefinitely.
        let _ = tokio::time::timeout(
            Duration::from_secs(45),
            crate::features::translate_lines(&mut lines),
        )
        .await;

        // The generation comparison and publication share one write lock. This prevents both the
        // check/write race and A -> B -> A ABA writes from an older request.
        let mut state = LYRICS_STATE.write().unwrap_or_else(|e| e.into_inner());
        if state.owns(generation, &expected_key) {
            state.lines = lines;
            state.enrichment_complete = true;
        }
    });
    // Register the handle so the next track change / stop / shutdown can cancel this task's remaining
    // network work, aborting any prior task still in flight.
    set_lyric_task(handle.abort_handle());
}

fn set_status_line_if_current(generation: u64, track_key: &str, message: String) {
    let _output = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let state = LYRICS_STATE.read().unwrap_or_else(|e| e.into_inner());
    if state.owns(generation, track_key) {
        set_status_line_locked(Some(message));
    }
}

fn update_quality_status() {
    let is_lossless = crate::PLAYING_LOSSLESS.load(Ordering::SeqCst);
    let game_mode = crate::config().game_mode;

    // Plain text on purpose: _draw_status_line_locked centres and styles it, and it can only
    // measure the width correctly if there are no ANSI escapes embedded in the string.
    let text = if !game_mode {
        if is_lossless {
            Some("FLAC • LOSSLESS AUDIO".to_string())
        } else {
            Some("OPUS • STANDARD AUDIO".to_string())
        }
    } else {
        None
    };

    // scoped so the BASE_STATUS guard is released before _draw_status_line takes OUTPUT_LOCK,
    // keeping OUTPUT_LOCK outermost
    {
        *BASE_STATUS.write().unwrap_or_else(|e| e.into_inner()) = text.clone();
    }

    // Don't paint over a message that is still within its linger window. This runs as the first act
    // of every new monitor thread, so it raced the message the main thread had just set and usually
    // won — the startup greeting ("Wassup <name>") was overwritten within milliseconds and never
    // appeared at all, and any status set alongside a track change went the same way.
    let _output = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if transient_message_showing() {
        return;
    }
    _draw_status_line_locked(text);
}

/// Whether a temporary message is still inside its display window.
fn transient_message_showing() -> bool {
    STATUS_TIMEOUT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .map(|t| Instant::now() < t)
        .unwrap_or(false)
}

fn check_status_timeout() {
    let _output = crate::OUTPUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if crate::shutdown_started() || crate::PROMPT_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let mut timeout = STATUS_TIMEOUT.write().unwrap_or_else(|e| e.into_inner());
    let expired = timeout.map(|t| Instant::now() > t).unwrap_or(false);
    if expired {
        *timeout = None;
        drop(timeout);
        let base = BASE_STATUS
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        _draw_status_line_locked(base);
    }
}

/// Index of the lyric line that should be highlighted, or `None` if none has started yet.
///
/// The `None` case is why this returns an Option: it used to collapse "the first line has not been
/// reached" and "the first line is the current one" both to `0`, so a song with an instrumental intro
/// highlighted its opening line from the very first second, then sat on it until the vocals caught up.
fn get_current_lyric_index(lyrics: &[LrcLine], curr_time: f64) -> Option<usize> {
    if lyrics.is_empty() {
        return None;
    }
    let offset = LYRIC_OFFSET.load(Ordering::Relaxed) as f64 / 1000.0;
    let cutoff = curr_time + 0.149 + offset;

    match lyrics
        .iter()
        .position(|l| dur_to_secs(l.timestamp) > cutoff)
    {
        Some(0) => None,                // still before the first line's timestamp
        Some(next) => Some(next - 1),   // the line before the next one is current
        None => Some(lyrics.len() - 1), // past every timestamp: the last line stands
    }
}

/// Display width of `s` in terminal cells.
///
/// This used to guess "any multi-byte char is 2 cells wide", which over-counts every accented
/// Latin letter (é, ü, ñ ...) and so mis-centred any title containing one. It also disagreed with
/// the `unicode_width`-based copies that ui1/ui2 kept privately, meaning the same string measured
/// differently depending on which module asked.
pub fn get_visual_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Strip terminal-control and layout-breaking characters from an untrusted string before it is
/// measured, truncated, centred, coloured, or printed.
///
/// Remote track titles, artists, playlist names and lyrics are attacker-influenced text. Left raw they
/// can carry ESC (`\x1b`) and other control bytes that clear the screen, rewrite the window title, emit
/// OSC hyperlinks, move the cursor, or make later shell output deceptive — and `unicode_width` reports
/// all of those as zero cells, so they slipped through every width calculation invisibly. This is the
/// single sanitiser every display path funnels through (via [`truncate_safe`], [`get_scrolling_text`]
/// and [`word_wrap_cjk`]); it must run on the raw field, never on an already-styled string, so the
/// application's own ANSI styling is applied afterwards and left intact.
pub fn sanitize_display(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            // Fold every line/whitespace break to one space so single-line fields stay single-line.
            '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => Some(' '),
            // Bidi override/isolate controls reorder displayed text for visual spoofing.
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => None,
            // ESC, the rest of the C0 controls, DEL, and the C1 controls (all general category Cc).
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

pub fn truncate_safe(s: &str, max_width: usize) -> String {
    // Sanitise first, then measure/truncate the clean text: control bytes count as zero cells to
    // unicode_width, so truncating raw input both mis-measured it and copied the control bytes through.
    let s = sanitize_display(s);
    let s = s.as_str();
    if get_visual_width(s) <= max_width {
        return s.to_string();
    }

    // Only spend cells on an ellipsis if there is room for one. Reserving 3 cells unconditionally
    // meant a slot narrower than 3 got back "..." — itself 3 cells wide, i.e. wider than the slot it
    // was supposed to fit, so it overflowed into whatever was drawn to its right.
    const ELLIPSIS: &str = "...";
    let (ellipsis, budget) = if max_width >= ELLIPSIS.len() {
        (ELLIPSIS, max_width - ELLIPSIS.len())
    } else {
        ("", max_width)
    };

    let mut result = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = char_width(c);
        if width + w > budget {
            break;
        }
        result.push(c);
        width += w;
    }
    result.push_str(ellipsis);
    result
}

pub fn blindly_trim(text: &str) -> &str {
    let separators = ['-', '(', '[', '_', '|'];

    let mut cut = text.len();

    for sep in separators {
        let pattern = format!(" {}", sep);
        if let Some(idx) = text.find(&pattern) {
            cut = cut.min(idx);
        }
    }
    &text[..cut]
}

pub fn word_wrap_cjk(text: &str, max_width: usize) -> Vec<String> {
    // Sanitise first: lyric lines are untrusted, and a folded newline/control byte here would break
    // the wrapper's single-line-per-entry assumption as well as corrupt the terminal.
    let text = sanitize_display(text);
    let text = text.as_str();
    // max_width 0 has no valid wrapping; without this the per-char fallback below emitted one line
    // per character (each still too wide for the slot) for the whole lyric.
    if text.trim().is_empty() || max_width == 0 {
        return vec!["".to_string()];
    }
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    let word_visual_width = |w: &str| -> usize { get_visual_width(w) };

    for word in text.split_whitespace() {
        let w_len = word_visual_width(word);
        if current_width + w_len + (if current_width > 0 { 1 } else { 0 }) <= max_width {
            if current_width > 0 {
                current_line.push(' ');
                current_width += 1;
            }
            current_line.push_str(word);
            current_width += w_len;
        } else {
            if !current_line.is_empty() {
                lines.push(current_line);
            }
            if w_len > max_width {
                // Word longer than the slot: break it across lines, character by character.
                current_line = String::new();
                current_width = 0;
                for c in word.chars() {
                    let c_width = char_width(c);
                    // A single char wider than the whole slot can never be placed; emitting it
                    // anyway produced a line wider than max_width.
                    if c_width > max_width {
                        continue;
                    }
                    if current_width + c_width > max_width {
                        // don't emit a blank line when the very first char already fills the slot
                        if !current_line.is_empty() {
                            lines.push(std::mem::take(&mut current_line));
                        }
                        current_width = 0;
                    }
                    current_line.push(c);
                    current_width += c_width;
                }
            } else {
                current_line = word.to_string();
                current_width = w_len;
            }
        }
    }
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    // Callers index and measure the result, so never hand back an empty Vec (possible when every
    // character was too wide for the slot).
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub fn duration_to_seconds(duration: &str) -> f64 {
    let parts: Vec<f64> = duration
        .split(':')
        .map(|p| p.parse::<f64>().unwrap_or(0.0))
        .collect();

    match parts.as_slice() {
        [m, s] => m * 60.0 + s,
        [h, m, s] => h * 3600.0 + m * 60.0 + s,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Placement rule for a chooser: sit just under the queue (where listings always used to appear),
    /// and only ride up when the list would not otherwise fit on the terminal.
    fn chooser_start_row(ui_mode: usize, rows: u16, n_lines: usize) -> u16 {
        let last_row = rows.saturating_sub(1);
        let preferred = match ui_mode {
            0 => 26,
            1 => 24,
            _ => CHOOSER_FIRST_ROW,
        };
        let highest_that_fits = (last_row + 1).saturating_sub(n_lines as u16);
        preferred
            .min(highest_that_fits)
            .max(CHOOSER_FIRST_ROW.min(last_row))
    }

    #[test]
    fn trim_chooser_keeps_the_prompt_when_the_list_overflows() {
        // 10 results + prompt on an 80x24 budget of 8: the old code truncated the tail and lost the
        // "Select" prompt entirely. It must survive as the last line.
        let mut lines: Vec<String> = (1..=10).map(|i| format!("{i}. song")).collect();
        lines.push("~ Select (1-10): ".to_string());
        let out = trim_chooser_lines(lines, 8, false);
        assert_eq!(out.len(), 8);
        assert_eq!(out.last().unwrap(), "~ Select (1-10): ");
        assert!(
            out.iter().any(|l| l == "..."),
            "a trim marker should be shown"
        );
    }

    #[test]
    fn trim_chooser_keeps_both_header_and_prompt() {
        // A big library: header + 15 playlists + prompt, budget 8. Both bookends must remain.
        let mut lines = vec!["--- YOUR LIBRARY ---".to_string()];
        lines.extend((1..=15).map(|i| format!("{i}. playlist")));
        lines.push("~ Select (1-15): ".to_string());
        let out = trim_chooser_lines(lines, 8, true);
        assert_eq!(out.len(), 8);
        assert_eq!(out.first().unwrap(), "--- YOUR LIBRARY ---");
        assert_eq!(out.last().unwrap(), "~ Select (1-15): ");
        assert!(out.iter().any(|l| l == "..."));
    }

    #[test]
    fn trim_chooser_leaves_a_fitting_list_untouched() {
        let lines = vec![
            "1. a".to_string(),
            "2. b".to_string(),
            "~ Select (1-2): ".to_string(),
        ];
        assert_eq!(trim_chooser_lines(lines.clone(), 8, false), lines);
    }

    #[test]
    fn trim_chooser_survives_a_tiny_budget() {
        let mut lines: Vec<String> = (1..=5).map(|i| format!("{i}. x")).collect();
        lines.push("PROMPT".to_string());
        // budget 1: only the prompt fits
        assert_eq!(trim_chooser_lines(lines.clone(), 1, false), vec!["PROMPT"]);
        // budget 2, no header: marker + prompt, still within budget
        let out = trim_chooser_lines(lines, 2, false);
        assert_eq!(out.len(), 2);
        assert_eq!(out.last().unwrap(), "PROMPT");
    }

    #[test]
    fn chooser_sits_under_the_queue_on_a_tall_terminal() {
        // 6 lines (5 results + prompt) on a 40-row terminal: right below ui1's queue, not at row 34
        assert_eq!(chooser_start_row(0, 40, 6), 26);
        assert_eq!(chooser_start_row(1, 40, 6), 24);
    }

    #[test]
    fn chooser_rides_up_only_when_it_would_not_fit() {
        // 80x24 has no room at row 26, so it moves up just enough to fit
        assert_eq!(chooser_start_row(0, 24, 6), 18);
        assert_eq!(chooser_start_row(0, 24, 8), 16);
    }

    #[test]
    fn chooser_never_climbs_over_the_player() {
        // even a huge list must not start above the now-playing/progress rows
        for rows in [10u16, 20, 24, 30, 50] {
            for n in 1..30usize {
                let r = chooser_start_row(0, rows, n);
                let floor = CHOOSER_FIRST_ROW.min(rows.saturating_sub(1));
                assert!(r >= floor, "rows={rows} n={n} start={r} floor={floor}");
                assert!(r <= rows.saturating_sub(1), "rows={rows} n={n} start={r}");
            }
        }
    }

    fn lrc(secs: f64, text: &str) -> LrcLine {
        LrcLine {
            timestamp: Duration::from_millis((secs * 1000.0) as u64),
            text: text.to_string(),
            translation: None,
            romanized: None,
        }
    }

    #[test]
    fn partial_variants_do_not_expose_an_incomplete_sheet() {
        let mut lines = [lrc(1.0, "one"), lrc(2.0, "two")];
        lines[0].translation = Some("uno".to_string());
        lines[0].romanized = Some("ichi".to_string());

        assert_eq!(
            variant_availability(&lines, true, 1),
            LyricVariantAvailability::Unavailable
        );
        assert_eq!(
            variant_availability(&lines, true, 2),
            LyricVariantAvailability::Unavailable
        );
    }

    #[test]
    fn complete_variants_preserve_blank_timed_lines() {
        let mut lines = [lrc(1.0, "one"), lrc(2.0, ""), lrc(3.0, "two")];
        lines[0].translation = Some("uno".to_string());
        lines[2].translation = Some("dos".to_string());

        assert_eq!(
            variant_availability(&lines, true, 2),
            LyricVariantAvailability::Available
        );
    }

    #[test]
    fn variant_loading_is_distinct_from_unavailable() {
        let lines = [lrc(1.0, "one")];
        assert_eq!(
            variant_availability(&lines, false, 2),
            LyricVariantAvailability::Loading
        );
        assert_eq!(
            selected_lyric_notice(2, LyricVariantAvailability::Loading),
            Some("Fetching translation...")
        );
    }

    #[tokio::test]
    async fn a_superseded_lyric_task_is_cancelled_not_just_ignored() {
        // Prove cancellation actually drops the task's future (and thus its in-flight requests), using
        // a drop sentinel — generation checks alone would let it run to its timeout.
        abort_lyric_task(); // clear any leftover from the process

        struct Sentinel(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Sentinel {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let _sentinel = Sentinel(Some(dropped_tx));
            let _ = started_tx.send(());
            // Never completes on its own, so only an abort can end it.
            std::future::pending::<()>().await;
        });
        set_lyric_task(handle.abort_handle());
        // Let the task actually start (construct the sentinel) before we cancel it.
        started_rx.await.expect("lyric task should start");

        // A track change / stop supersedes it; the future must be dropped promptly.
        abort_lyric_task();
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .expect("superseded lyric task was not cancelled")
            .expect("sentinel drop signal");
    }

    #[test]
    fn lyric_generation_rejects_stale_and_aba_publications() {
        let mut state = LyricsState::new();
        let first_a = state.begin_track("a".to_string());
        let _b = state.begin_track("b".to_string());
        let second_a = state.begin_track("a".to_string());

        assert!(!state.owns(first_a, "a"));
        assert!(state.owns(second_a, "a"));
        state.invalidate();
        assert!(!state.owns(second_a, "a"));
    }

    #[test]
    fn no_lyric_is_active_before_the_first_timestamp() {
        // A song with an instrumental intro used to highlight its opening line from second zero,
        // because "not reached yet" and "line 0 is current" both came back as 0.
        let lines = [lrc(15.0, "first"), lrc(20.0, "second")];
        LYRIC_OFFSET.store(0, Ordering::Relaxed);

        assert_eq!(get_current_lyric_index(&lines, 0.0), None);
        assert_eq!(get_current_lyric_index(&lines, 14.0), None);
        // once the first line's timestamp passes it becomes current, and stays current until the next
        assert_eq!(get_current_lyric_index(&lines, 15.0), Some(0));
        assert_eq!(get_current_lyric_index(&lines, 19.0), Some(0));
        assert_eq!(get_current_lyric_index(&lines, 21.0), Some(1));
        // past every timestamp the last line stands
        assert_eq!(get_current_lyric_index(&lines, 999.0), Some(1));
    }

    #[test]
    fn lyric_index_is_none_when_there_are_no_lyrics() {
        LYRIC_OFFSET.store(0, Ordering::Relaxed);
        assert_eq!(get_current_lyric_index(&[], 0.0), None);
        assert_eq!(get_current_lyric_index(&[], 42.0), None);
    }

    #[test]
    fn a_lyric_starting_at_zero_is_active_immediately() {
        LYRIC_OFFSET.store(0, Ordering::Relaxed);
        let lines = [lrc(0.0, "right away"), lrc(5.0, "later")];
        assert_eq!(get_current_lyric_index(&lines, 0.0), Some(0));
    }

    #[test]
    fn accented_latin_is_one_cell_wide() {
        // the old byte-length heuristic charged 2 cells per accented letter, mis-centring titles
        assert_eq!(get_visual_width("cafe"), 4);
        assert_eq!(get_visual_width("café"), 4);
        assert_eq!(get_visual_width("Björk"), 5);
    }

    #[test]
    fn cjk_is_two_cells_wide() {
        assert_eq!(get_visual_width("日本語"), 6);
    }

    #[test]
    fn truncate_safe_never_exceeds_max_width() {
        assert_eq!(truncate_safe("hello", 10), "hello");
        let t = truncate_safe("abcdefghijklmnop", 8);
        assert!(get_visual_width(&t) <= 8, "got {:?}", t);
        assert!(t.ends_with("..."));
    }

    #[test]
    fn truncate_safe_fits_slots_too_narrow_for_an_ellipsis() {
        // reserving 3 cells for "..." unconditionally returned a 3-cell string for a 0/1/2-cell slot
        for w in 0..=6 {
            for s in ["hello world", "日本語のテキスト", "café", "🎵🎶🎵🎶"] {
                let out = truncate_safe(s, w);
                assert!(
                    get_visual_width(&out) <= w,
                    "truncate_safe({s:?}, {w}) = {out:?} is {} cells",
                    get_visual_width(&out)
                );
            }
        }
    }

    #[test]
    fn word_wrap_cjk_is_safe_at_tiny_widths() {
        for w in 0..=4 {
            let lines = word_wrap_cjk("日本語のテキストはここで折り返される and some ascii", w);
            assert!(!lines.is_empty(), "width {w} produced no lines at all");
            for line in &lines {
                assert!(
                    get_visual_width(line) <= w.max(1),
                    "width {w}: line {line:?} is {} cells",
                    get_visual_width(line)
                );
            }
        }
    }

    #[test]
    fn truncate_safe_keeps_accented_text_that_fits() {
        assert_eq!(truncate_safe("café", 4), "café");
    }

    #[test]
    fn word_wrap_cjk_lines_fit_max_width() {
        for line in word_wrap_cjk("this is a fairly long sentence that must wrap", 10) {
            assert!(get_visual_width(&line) <= 10, "line too wide: {:?}", line);
        }
        for line in word_wrap_cjk("日本語のテキストはここで折り返される", 8) {
            assert!(get_visual_width(&line) <= 8, "line too wide: {:?}", line);
        }
    }

    #[test]
    fn get_scrolling_text_never_exceeds_the_slot_width() {
        // each CJK char is 2 cells, so taking `width` *chars* produced twice the requested width
        for w in [1, 5, 10, 17] {
            let out = get_scrolling_text("日本語のとてもながいきょくめい", w);
            assert!(
                get_visual_width(&out) <= w,
                "width {w}: got {out:?} ({} cells)",
                get_visual_width(&out)
            );
        }
    }

    #[test]
    fn get_scrolling_text_returns_text_that_already_fits() {
        assert_eq!(get_scrolling_text("short", 20), "short");
        // 3 CJK chars == 6 cells, so this fits a 6-cell slot exactly and must not scroll
        assert_eq!(get_scrolling_text("日本語", 6), "日本語");
    }

    #[test]
    fn scroll_cursors_advance_independently() {
        // Regression: the title and artist lines used to share one cursor, so the longer field was
        // bound to the shorter field's cycle and its tail never scrolled into view. ui3 now scrolls
        // the artist on its own cursor via get_scrolling_text_secondary; scroll_text takes the cursor
        // state explicitly, so drive two independent states and confirm one tick moves only its own.
        let text = "a marquee field far too long to fit its slot";
        let width = 10;

        // The "title" cursor is overdue (last tick 400ms ago) and must advance on this call.
        let title_scroll = RwLock::new(0usize);
        let title_last = RwLock::new(Some(Instant::now() - Duration::from_millis(400)));
        // The "artist" cursor was just touched and is still inside the 300ms gate: it must hold.
        let artist_scroll = RwLock::new(0usize);
        let artist_last = RwLock::new(Some(Instant::now()));

        let _ = scroll_text(text, width, &title_scroll, &title_last);
        let _ = scroll_text(text, width, &artist_scroll, &artist_last);

        assert_eq!(
            *title_scroll.read().unwrap(),
            1,
            "the overdue cursor should have advanced one step"
        );
        assert_eq!(
            *artist_scroll.read().unwrap(),
            0,
            "the second cursor must not move on the first cursor's tick"
        );
    }

    #[test]
    fn sanitize_display_strips_terminal_control_sequences() {
        // clear-screen, an OSC window-title sequence, and a cursor move — none of the control bytes
        // may survive, but the visible text between them must.
        let evil = "\x1b[2Jhi\x1b]0;pwned\x07 there\x1b[1;1H";
        let clean = sanitize_display(evil);
        assert!(!clean.contains('\x1b'), "ESC survived: {clean:?}");
        assert!(!clean.contains('\x07'), "BEL survived: {clean:?}");
        assert!(clean.contains("hi") && clean.contains(" there"));
    }

    #[test]
    fn sanitize_display_flattens_breaks_and_strips_c1_and_bidi() {
        // CR, LF and tab each collapse to a single space so a one-line field stays one line.
        assert_eq!(sanitize_display("a\r\nb\tc"), "a  b c");
        // NEL is a C1 control (category Cc) and must go.
        assert!(!sanitize_display("x\u{0085}y").contains('\u{0085}'));
        // bidi override + isolate used for visual spoofing are removed outright.
        assert_eq!(sanitize_display("a\u{202e}b\u{2066}c"), "abc");
    }

    #[test]
    fn sanitize_display_preserves_legitimate_text() {
        // Accents, CJK, emoji and a zero-width joiner (emoji sequences) are all normal text.
        for s in [
            "plain ascii",
            "日本語",
            "café",
            "Björk",
            "🎵🎶",
            "he\u{200d}llo",
        ] {
            assert_eq!(sanitize_display(s), s, "mangled {s:?}");
        }
    }

    #[test]
    fn width_helpers_sanitise_before_measuring() {
        // truncate_safe drops the ESC bytes but keeps the visible characters, and never returns a
        // control byte.
        let out = truncate_safe("\x1b[31mred\x1b[0m", 100);
        assert!(!out.contains('\x1b'));
        assert_eq!(out, "[31mred[0m");
        // the artist scroller and the lyric wrapper are the other two untrusted boundaries.
        assert!(!get_scrolling_text("a\x1bb", 100).contains('\x1b'));
        for line in word_wrap_cjk("one\x1btwo\nthree", 100) {
            assert!(
                !line.contains('\x1b') && !line.contains('\n'),
                "leaked control: {line:?}"
            );
        }
    }

    #[test]
    fn duration_to_seconds_handles_both_formats() {
        assert_eq!(duration_to_seconds("3:21"), 201.0);
        assert_eq!(duration_to_seconds("1:02:03"), 3723.0);
        assert_eq!(duration_to_seconds("garbage"), 0.0);
    }
}
