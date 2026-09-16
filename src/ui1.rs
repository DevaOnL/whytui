use crate::Track;
use crate::ui_common::*;
use colored::*;
use crossterm::{
    cursor, queue,
    style::Print,
    terminal::{self, ClearType},
};
use std::io::{Write, stdout};

const STATUS_LINE_ROW: u16 = 12;
const QUEUE_SIZE: usize = 5;

pub use crate::ui_common::{show_playlists, show_songs};

const HEADER_ROW: u16 = 18;
const QUEUE_START_ROW: u16 = 20;

pub fn load_banner(track_opt: Option<&Track>, queue: &[String], toggle: &str) {
    let mut stdout = stdout();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let width_usize = cols as usize;

    // Every row here is absolute, and writing past the last line makes the terminal scroll — which
    // shifts the banner, the progress bar and the lyrics all out of position for the rest of the
    // session. The queue used to reach row 24 and park the cursor on row 26, so on the 80x24
    // terminal the README recommends the whole layout tore itself apart. Draw only what fits.
    let last_row = rows.saturating_sub(1);
    let queue_rows = if QUEUE_START_ROW > last_row {
        0
    } else {
        std::cmp::min(QUEUE_SIZE, (last_row - QUEUE_START_ROW + 1) as usize)
    };

    // draw art first
    let _ = queue!(stdout, cursor::Hide, cursor::MoveTo(0, 0));
    let _ = queue!(stdout, Print(get_banner_art()), Print("\n\n"));

    // recent/ queue (default)
    let header_text = if toggle == "recent" {
        "recent"
    } else {
        "queue"
    };

    let raw_header = format!("¨˜ˆ”°⍣~•{}•~⍣°”ˆ˜¨", header_text);
    let header_len = get_visual_width(&raw_header);
    let header_pad = (width_usize.saturating_sub(header_len)) / 2;

    if HEADER_ROW <= last_row {
        let _ = queue!(
            stdout,
            cursor::MoveTo(0, HEADER_ROW),
            terminal::Clear(ClearType::FromCursorDown)
        );

        let _ = queue!(
            stdout,
            cursor::MoveTo(header_pad as u16, HEADER_ROW),
            Print(raw_header.cyan())
        );
    }

    // print songs in queue
    if !queue.is_empty() {
        for (i, name) in queue.iter().enumerate().take(queue_rows) {
            // compress the name, then clip it to the terminal width. Unclipped, a title wider than
            // the terminal wrapped onto the next row and, on the bottom queue row, scrolled the whole
            // absolutely-positioned layout permanently out of place — the very failure the top-of-file
            // comment says was fixed. ui2 already truncates its queue names for the same reason.
            let clean_name = blindly_trim(name);
            let display_str = truncate_safe(clean_name, width_usize.saturating_sub(1));
            let len = get_visual_width(&display_str);
            let pad = (width_usize.saturating_sub(len)) / 2;

            let _ = queue!(
                stdout,
                cursor::MoveTo(pad as u16, QUEUE_START_ROW + i as u16),
                Print(display_str.dimmed())
            );
        }
    } else if queue_rows > 0 {
        let msg = "~";
        let pad = (width_usize.saturating_sub(1)) / 2;
        let _ = queue!(
            stdout,
            cursor::MoveTo(pad as u16, QUEUE_START_ROW),
            Print(msg)
        );
    }

    // Park the cursor just below the queue for whatever prints next (search results, prompts),
    // clamped to the last row and without the trailing newline that used to force a scroll.
    let park_row = std::cmp::min(QUEUE_START_ROW + queue_rows as u16 + 1, last_row);
    let _ = queue!(stdout, cursor::MoveTo(0, park_row), cursor::Hide);
    let _ = stdout.flush();

    if let Some(track) = track_opt {
        ensure_monitor_for_track(track, draw_ui1_status);
    }
}

/// Re-arm the progress/lyrics monitor for `track` without repainting the banner. Used while a chooser
/// owns the screen so the now-playing line follows an auto-advance rather than freezing on the old song.
pub fn rearm_monitor(track: &Track) {
    ensure_monitor_for_track(track, draw_ui1_status);
}

pub fn restart_monitor_for_view(track: &Track) {
    crate::ui_common::restart_monitor_for_view(track, draw_ui1_status);
}

fn draw_ui1_status(
    title: &str,
    artist: &str,
    _full_name: &str,
    curr: f64,
    tot: f64,
    lyric_frame: LyricFrame<'_>,
) {
    let LyricFrame {
        lines: lyrics,
        current_idx,
        mode: lyric_mode,
        notice: lyric_notice,
    } = lyric_frame;
    let mut stdout = stdout();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let width_usize = cols as usize;

    // load_banner clamps its rows to the terminal height, but this repaint runs three times a second
    // and used to write its absolute rows unconditionally. A MoveTo past the last line is clamped by
    // the terminal, so on a short terminal every one of these rows collapsed onto the bottom line and
    // each Clear(CurrentLine) wiped what the previous one had just drawn — including the prompt row
    // load_banner had carefully placed there.
    let last_row = rows.saturating_sub(1);
    let fits = |row: u16| row <= last_row;

    let fmt_time = |s: f64| format!("{:02}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64);

    // progress bar
    let max_bar_width = 42;
    let available_width = width_usize.saturating_sub(16);
    let bar_width = std::cmp::min(available_width, max_bar_width);

    let ratio = if tot > 0.0 {
        (curr / tot).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled_len = (ratio * bar_width as f64).round() as usize;
    let empty_len = bar_width.saturating_sub(filled_len);

    let bar_str = format!(
        "{}{}",
        "━".repeat(filled_len).cyan(),
        "─".repeat(empty_len).dimmed()
    );

    let artist_scroll = get_scrolling_text(artist, 25);
    let trimmed_title = blindly_trim(title);
    let truncated_title = truncate_safe(trimmed_title, 35);
    let final_title = format!("{} [{}]", truncated_title, artist_scroll.dimmed());

    let title_visual_len =
        get_visual_width(&truncated_title) + get_visual_width(&artist_scroll) + 5;
    let title_pad = (width_usize.saturating_sub(title_visual_len)) / 2;

    let total_bar_len = 12 + bar_width;
    let bar_pad = (width_usize.saturating_sub(total_bar_len)) / 2;

    // Truncated to the terminal width: a long lyric line used to wrap onto the row below, which the
    // next frame then cleared, leaving the display flickering between one and two lines.
    let lyric_width = width_usize.saturating_sub(2);
    let line_text = |idx: usize| {
        lyrics
            .get(idx)
            .and_then(|line| line.text_for_mode(lyric_mode))
            .unwrap_or("")
    };
    // `None` means the first line's timestamp has not been reached, so there is nothing to highlight
    // yet — show the opening line as what is coming up instead of pretending it is being sung.
    let (current_raw, next_raw) = match lyric_notice {
        Some(notice) => (notice, ""),
        None => match current_idx {
            Some(i) => (line_text(i), line_text(i + 1)),
            None => ("", line_text(0)),
        },
    };
    let current_text = truncate_safe(current_raw, lyric_width);
    let next_text = truncate_safe(next_raw, lyric_width);

    let curr_lyric_len = get_visual_width(&current_text);
    let next_lyric_len = get_visual_width(&next_text);

    let curr_lyric_pad = (width_usize.saturating_sub(curr_lyric_len)) / 2;
    let next_lyric_pad = (width_usize.saturating_sub(next_lyric_len)) / 2;

    let current_display = if current_text.trim().is_empty() {
        "♪".white().bold().blink().to_string()
    } else {
        current_text
            .truecolor(255, 255, 255)
            .bold()
            .italic()
            .to_string()
    };

    // draw status
    let _ = queue!(stdout, cursor::Hide, cursor::SavePosition);

    if fits(STATUS_LINE_ROW) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(title_pad as u16, STATUS_LINE_ROW),
            terminal::Clear(ClearType::CurrentLine),
            Print(format!("{} {}", "▶︎".cyan(), final_title.white().bold())),
        );
    }

    if fits(STATUS_LINE_ROW + 1) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(bar_pad as u16, STATUS_LINE_ROW + 1),
            terminal::Clear(ClearType::CurrentLine),
            Print(format!(
                "{} {} {}",
                fmt_time(curr).cyan(),
                bar_str,
                fmt_time(tot).cyan()
            )),
        );
    }

    if fits(STATUS_LINE_ROW + 3) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(curr_lyric_pad as u16, STATUS_LINE_ROW + 3),
            terminal::Clear(ClearType::CurrentLine),
            Print(current_display),
        );
    }

    if fits(STATUS_LINE_ROW + 4) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(next_lyric_pad as u16, STATUS_LINE_ROW + 4),
            terminal::Clear(ClearType::CurrentLine),
            Print(next_text.dimmed().italic()),
        );
    }

    if fits(STATUS_LINE_ROW + 5) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(next_lyric_pad as u16, STATUS_LINE_ROW + 5),
            terminal::Clear(ClearType::CurrentLine),
        );
    }

    let _ = queue!(stdout, cursor::RestorePosition, cursor::Hide);
    let _ = stdout.flush();
}
