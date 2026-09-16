use crate::Track;
use crate::api::split_title_artist;
use crate::ui_common::*;
use colored::*;
use crossterm::{
    cursor, queue,
    style::Print,
    terminal::{self, ClearType},
};
use std::io::{Write, stdout};

const PROGRESS_ROW: u16 = 12;
const CONTENT_START_ROW: u16 = 17;
const QUEUE_SIZE: usize = 6;
const PROMPT_ROW: u16 = CONTENT_START_ROW + (QUEUE_SIZE as u16) + 1;

pub fn load_banner(track_opt: Option<&Track>, queue: &[String], toggle: &str) {
    let mut stdout = stdout();
    let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));

    // Absolute rows: writing past the last line scrolls the terminal and permanently shifts every
    // other element. PROMPT_ROW is 24, which is off-screen on the recommended 80x24.
    let last_row = term_rows.saturating_sub(1);
    let queue_rows = if CONTENT_START_ROW > last_row {
        0
    } else {
        std::cmp::min(QUEUE_SIZE, (last_row - CONTENT_START_ROW + 1) as usize)
    };

    let split_col = term_cols / 2;

    let left_center_x = split_col / 2;
    let right_width = term_cols - split_col;
    let right_center_x = split_col + (right_width / 2);

    let _ = queue!(stdout, cursor::Hide, cursor::MoveTo(0, 0));
    let _ = queue!(stdout, Print(get_banner_art()));

    let queue_header_txt = if toggle == "recent" {
        "recent"
    } else {
        "queue"
    };
    let q_header_str = format!("¨˜ˆ”°⍣~•{}•~⍣°”ˆ˜¨", queue_header_txt);
    let l_header_str = " ¨˜ˆ”°⍣~•lyrics•~⍣°”ˆ˜¨";

    let l_len = get_visual_width(l_header_str) as u16;
    let l_pos = left_center_x.saturating_sub(l_len / 2);
    let _ = queue!(
        stdout,
        cursor::MoveTo(l_pos, CONTENT_START_ROW - 1),
        Print(l_header_str.cyan().bold().dimmed())
    );

    let q_len = get_visual_width(&q_header_str) as u16;
    let q_pos = right_center_x.saturating_sub(q_len / 2);
    let _ = queue!(
        stdout,
        cursor::MoveTo(q_pos, CONTENT_START_ROW - 1),
        Print(q_header_str.bright_cyan().bold().dimmed())
    );

    for i in 0..queue_rows {
        let _ = queue!(
            stdout,
            cursor::MoveTo(split_col, CONTENT_START_ROW + (i as u16)),
            terminal::Clear(ClearType::UntilNewLine)
        );

        if i < queue.len() {
            let (clean_name, _) = split_title_artist(&queue[i]);

            let max_len = (right_width as usize).saturating_sub(2);

            let clean_name = blindly_trim(&clean_name);
            let safe_name = truncate_safe(clean_name, max_len);

            let display_str = safe_name.to_string();

            let display_len = get_visual_width(&display_str) as u16;

            let final_x = right_center_x.saturating_sub(display_len / 2);

            let styled = match i {
                0 => display_str.truecolor(255, 255, 255).bold(),
                1 => display_str.truecolor(180, 180, 180),
                2 => display_str.truecolor(160, 160, 160),
                3 => display_str.truecolor(140, 140, 140),
                4 => display_str.truecolor(120, 120, 120),
                _ => display_str.truecolor(100, 100, 100),
            };

            let _ = queue!(
                stdout,
                cursor::MoveTo(final_x, CONTENT_START_ROW + (i as u16)),
                Print(styled)
            );
        }
    }

    // clamped to the last row, and without the trailing newline that forced a scroll there
    let prompt_row = std::cmp::min(PROMPT_ROW, last_row);
    let _ = queue!(
        stdout,
        cursor::MoveTo(0, prompt_row),
        terminal::Clear(ClearType::CurrentLine),
        terminal::Clear(ClearType::FromCursorDown),
        // Print("> ".bright_blue().bold()),
        cursor::Hide
    );
    let _ = stdout.flush();

    if let Some(track) = track_opt {
        ensure_monitor_for_track(track, draw_ui2_status);
    }
}

/// Re-arm the progress/lyrics monitor for `track` without repainting the banner. Used while a chooser
/// owns the screen so the now-playing line follows an auto-advance rather than freezing on the old song.
pub fn rearm_monitor(track: &Track) {
    ensure_monitor_for_track(track, draw_ui2_status);
}

pub fn restart_monitor_for_view(track: &Track) {
    crate::ui_common::restart_monitor_for_view(track, draw_ui2_status);
}

fn draw_ui2_status(
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
    let (term_cols, rows) = terminal::size().unwrap_or((80, 24));
    let width_usize = term_cols as usize;

    let split_col = term_cols / 2;
    let left_center_x = split_col / 2;

    let lyric_area_width = split_col as usize;

    let max_lyric_width = lyric_area_width.saturating_sub(1);

    let artist_scroll = get_scrolling_text(artist, 25);
    let title = blindly_trim(title);
    let display_title = truncate_safe(title, 35);

    let fmt_time = |s: f64| format!("{:02}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64);
    let max_bar_width = 42;
    let available_width = width_usize.saturating_sub(16);
    let bar_width = std::cmp::min(available_width, max_bar_width);

    let total_bar_len = 12 + bar_width;
    let bar_pad = (width_usize.saturating_sub(total_bar_len)) / 2;

    let title_visual_len = get_visual_width(&display_title) + get_visual_width(&artist_scroll) + 6;
    let title_pad = (width_usize.saturating_sub(title_visual_len)) / 2;

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

    // load_banner clamps to the terminal height; this repaint runs three times a second and used not
    // to, so on a short terminal every absolute row below collapsed onto the bottom line and each
    // Clear/cleaner erased whatever the previous row had just drawn there.
    let last_row = rows.saturating_sub(1);
    let fits = |row: u16| row <= last_row;

    let _ = queue!(stdout, cursor::Hide, cursor::SavePosition);

    if fits(PROGRESS_ROW) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(title_pad as u16, PROGRESS_ROW),
            terminal::Clear(ClearType::CurrentLine),
            Print(format!(
                "{} {} [{}]",
                "▶︎".cyan(),
                display_title.white().bold(),
                artist_scroll.dimmed().italic()
            )),
        );
    }

    if fits(PROGRESS_ROW + 1) {
        let _ = queue!(
            stdout,
            cursor::MoveTo(bar_pad as u16, PROGRESS_ROW + 1),
            terminal::Clear(ClearType::CurrentLine),
            Print(format!(
                "{} {} {}",
                fmt_time(curr).cyan(),
                bar_str,
                fmt_time(tot).cyan()
            )),
        );
    }

    let cleaner = " ".repeat(lyric_area_width);

    if max_lyric_width > 9 {
        for offset in 0..6 {
            let row = CONTENT_START_ROW + offset as u16;
            if !fits(row) {
                break;
            }
            // `None` means nothing has been sung yet: leave the highlighted slot empty and let the
            // upcoming lines queue up beneath it, rather than highlighting line 0 from second zero.
            let target_idx = match current_idx {
                Some(i) => Some(i + offset),
                None if offset == 0 => None,
                None => Some(offset - 1),
            };
            let text = if offset == 0 {
                lyric_notice.unwrap_or_else(|| {
                    target_idx
                        .and_then(|i| lyrics.get(i))
                        .and_then(|line| line.text_for_mode(lyric_mode))
                        .unwrap_or("")
                })
            } else if lyric_notice.is_some() {
                ""
            } else {
                target_idx
                    .and_then(|i| lyrics.get(i))
                    .and_then(|line| line.text_for_mode(lyric_mode))
                    .unwrap_or("")
            };

            let safe_text = truncate_safe(text, max_lyric_width);

            let len = get_visual_width(&safe_text) as u16;

            let final_x = left_center_x.saturating_sub(len / 2);

            let styled = match offset {
                0 => safe_text.truecolor(255, 255, 255).bold(),
                1 => safe_text.truecolor(180, 180, 180),
                2 => safe_text.truecolor(160, 160, 160),
                3 => safe_text.truecolor(140, 140, 140),
                4 => safe_text.truecolor(120, 120, 120),
                _ => safe_text.truecolor(100, 100, 100),
            };

            let _ = queue!(
                stdout,
                cursor::MoveTo(0, row),
                Print(&cleaner),
                cursor::MoveTo(final_x, row),
                Print(styled)
            );
        }
    }

    let _ = queue!(stdout, cursor::RestorePosition, cursor::Hide);
    let _ = stdout.flush();
}
