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
use std::sync::RwLock;

static UP_NEXT_TEXT: RwLock<String> = RwLock::new(String::new());

pub fn load_banner(track_opt: Option<&Track>, queue: &[String], _toggle: &str) {
    let mut stdout = stdout();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let next_text = if let Some(s) = queue.first() {
        let (title, _) = split_title_artist(s);
        format!("Up Next: {}", title)
    } else {
        "Up Next: ~".to_string()
    };
    *UP_NEXT_TEXT.write().unwrap_or_else(|e| e.into_inner()) = next_text.clone();

    // Every visible pixel of this mode comes from draw_minimal_ui on the monitor thread. This used to
    // Clear(All) and draw nothing, so each refresh blanked the whole screen for up to 300ms until the
    // next tick — and with no monitor running (nothing playing) it stayed blank indefinitely. Mode
    // switches already clear the screen for us, so paint the one line we own instead of wiping.
    let up_next_row = rows.saturating_sub(2);
    let content_width = (cols as usize).saturating_sub(4);
    let _ = queue!(
        stdout,
        cursor::Hide,
        cursor::MoveTo(2, up_next_row),
        terminal::Clear(ClearType::CurrentLine),
        Print(truncate_safe(&next_text, content_width).dimmed().italic()),
    );

    let prompt_row = rows.saturating_sub(1);
    let _ = queue!(stdout, cursor::MoveTo(0, prompt_row), cursor::Hide);
    let _ = stdout.flush();

    if let Some(track) = track_opt {
        ensure_monitor_for_track(track, draw_minimal_ui);
    }
}

/// Re-arm the progress/lyrics monitor for `track` without repainting the banner. Used while a chooser
/// owns the screen so the now-playing line follows an auto-advance rather than freezing on the old song.
pub fn rearm_monitor(track: &Track) {
    ensure_monitor_for_track(track, draw_minimal_ui);
}

pub fn restart_monitor_for_view(track: &Track) {
    crate::ui_common::restart_monitor_for_view(track, draw_minimal_ui);
}

fn draw_minimal_ui(
    title: &str,
    artist: &str,
    _album: &str,
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

    let lyric_start_row = 4;
    let up_next_row = rows.saturating_sub(2);
    let lyric_end_row = up_next_row.saturating_sub(1);

    let available_lyric_height = lyric_end_row.saturating_sub(lyric_start_row);
    let content_width = width_usize.saturating_sub(4);

    let _ = queue!(stdout, cursor::Hide, cursor::SavePosition);

    let title_scroll = get_scrolling_text(title, content_width);
    let artist_scroll = crate::ui_common::get_scrolling_text_secondary(artist, content_width);

    let fmt_time = |s: f64| format!("{:02}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64);
    let bar_width = width_usize.saturating_sub(16);
    let ratio = if tot > 0.0 {
        (curr / tot).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (ratio * bar_width as f64).round() as usize;
    let empty = bar_width.saturating_sub(filled);
    let bar_str = format!(
        "{}{}",
        "━".repeat(filled).cyan(),
        "─".repeat(empty).dimmed()
    );

    let _ = queue!(
        stdout,
        cursor::MoveTo(2, 0),
        terminal::Clear(ClearType::CurrentLine),
        Print(title_scroll.white().bold()),
        cursor::MoveTo(2, 1),
        terminal::Clear(ClearType::CurrentLine),
        Print(artist_scroll.dimmed()),
        cursor::MoveTo(2, 2),
        terminal::Clear(ClearType::CurrentLine),
        Print(format!(
            "{} {} {}",
            fmt_time(curr).cyan(),
            bar_str,
            fmt_time(tot).cyan()
        ))
    );

    for r in lyric_start_row..lyric_end_row {
        let _ = queue!(
            stdout,
            cursor::MoveTo(0, r),
            terminal::Clear(ClearType::CurrentLine)
        );
    }

    if (!lyrics.is_empty() || lyric_notice.is_some()) && available_lyric_height > 0 {
        let center_row = lyric_start_row + (available_lyric_height / 2);

        // `None` means the first line's timestamp has not been reached: nothing is highlighted, and
        // the whole lyric sheet sits below as upcoming text.
        let active_text = lyric_notice.unwrap_or_else(|| {
            current_idx
                .and_then(|i| lyrics.get(i))
                .and_then(|line| line.text_for_mode(lyric_mode))
                .unwrap_or("")
        });
        let active_lines = if active_text.is_empty() {
            Vec::new()
        } else {
            word_wrap_cjk(active_text, content_width)
        };
        let active_block_start = center_row.saturating_sub((active_lines.len() / 2) as u16);

        for (i, line) in active_lines.iter().enumerate() {
            let r = active_block_start + i as u16;
            if r >= lyric_start_row && r < lyric_end_row {
                let prefix = if i == 0 { "→ ".cyan() } else { "  ".into() };
                let _ = queue!(
                    stdout,
                    cursor::MoveTo(2, r),
                    Print(format!("{}{}", prefix, line.white().bold()))
                );
            }
        }

        let mut cursor_row = active_block_start;
        for i in (0..current_idx.unwrap_or(0)).rev() {
            if lyric_notice.is_some() {
                break;
            }
            if cursor_row <= lyric_start_row {
                break;
            }
            let lines = word_wrap_cjk(
                lyrics[i].text_for_mode(lyric_mode).unwrap_or(""),
                content_width,
            );
            let count = lines.len() as u16;
            if cursor_row < count {
                break;
            }
            let start_draw_row = cursor_row - count;

            for (j, line) in lines.iter().enumerate() {
                let r = start_draw_row + j as u16;
                if r >= lyric_start_row && r < active_block_start {
                    let _ = queue!(
                        stdout,
                        cursor::MoveTo(4, r),
                        Print(line.truecolor(95, 95, 95))
                    );
                }
            }
            cursor_row = start_draw_row;
        }

        let mut cursor_row = active_block_start + (active_lines.len() as u16);
        for lyric in lyrics.iter().skip(current_idx.map(|i| i + 1).unwrap_or(0)) {
            if lyric_notice.is_some() {
                break;
            }
            if cursor_row >= lyric_end_row {
                break;
            }
            let lines = word_wrap_cjk(lyric.text_for_mode(lyric_mode).unwrap_or(""), content_width);
            for line in lines {
                if cursor_row < lyric_end_row {
                    let _ = queue!(
                        stdout,
                        cursor::MoveTo(4, cursor_row),
                        Print(line.truecolor(95, 95, 95))
                    );
                    cursor_row += 1;
                }
            }
        }
    }

    let up_next_raw = UP_NEXT_TEXT.read().unwrap_or_else(|e| e.into_inner());
    let safe_up_next = crate::ui_common::truncate_safe(&up_next_raw, content_width);

    let _ = queue!(
        stdout,
        cursor::MoveTo(2, up_next_row),
        terminal::Clear(ClearType::CurrentLine),
        Print(safe_up_next.dimmed().italic()),
        cursor::RestorePosition,
        cursor::Hide
    );

    let _ = stdout.flush();
}
