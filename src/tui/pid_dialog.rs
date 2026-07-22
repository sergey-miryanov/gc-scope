use std::io::Stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::block::{Position, Title};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::list_pids::{FlatRow, PidInfoMap, ProcessInfo, build_flat_rows};

fn is_supported(r: &FlatRow) -> bool {
    r.is_python && r.runtime_found && r.supports_stats
}

/// First supported row at or after `from`, if any.
fn supported_at_or_after(rows: &[FlatRow], from: usize) -> Option<usize> {
    (from..rows.len()).find(|&n| is_supported(&rows[n]))
}

/// First supported row at or before `from`, if any.
fn supported_at_or_before(rows: &[FlatRow], from: usize) -> Option<usize> {
    (0..=from.min(rows.len().saturating_sub(1)))
        .rev()
        .find(|&n| is_supported(&rows[n]))
}

/// Popup width/height for the given terminal size and row count.
fn popup_dims(area_w: u16, area_h: u16, num_rows: usize) -> (u16, u16) {
    let popup_w = (area_w as f64 * 0.85) as u16;
    // Height wants one row per entry plus 4 chrome rows, shrunk to fit the terminal
    // (`area_h - 4`), then clamped to [12, 30]. The clamp bounds are constants and
    // correctly ordered, so it cannot panic; on a terminal shorter than 16 rows the
    // result floors at 12 and ratatui clips the overflow.
    let popup_h = (num_rows as u16 + 4)
        .min(area_h.saturating_sub(4))
        .clamp(12, 30);
    (popup_w, popup_h)
}

/// Number of data rows that fit inside a popup of the given height
/// (borders + header + separator consume 4 lines).
fn capacity_of(popup_h: u16) -> usize {
    popup_h.saturating_sub(4).max(1) as usize
}

/// Shows the interactive PID picker. Returns `Ok(Some(pid))` when the user selects a
/// process, `Ok(None)` when the user cancels (q/Esc), and `Err` only for genuine errors
/// (no processes available).
pub fn show_pid_dialog(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    processes: &[ProcessInfo],
    pid_info_map: &PidInfoMap,
) -> Result<Option<u32>> {
    if processes.is_empty() {
        anyhow::bail!("No Python processes found");
    }

    let flat_rows = build_flat_rows(processes, pid_info_map);

    if flat_rows.is_empty() {
        anyhow::bail!("No Python processes found");
    }

    let mut selected = match supported_at_or_after(&flat_rows, 0) {
        Some(i) => i,
        None => anyhow::bail!("No supported Python processes found"),
    };
    let mut cmdline_scroll = 0u16;
    let mut scroll_offset = 0usize;

    // Wipe any text (e.g. runtime-probe warnings) printed to the screen before the
    // dialog opened; the popup only paints its own rect, so stray text would otherwise
    // remain visible around it.
    terminal.clear()?;

    loop {
        terminal.draw(|f| {
            render_dialog(f, &flat_rows, selected, &mut scroll_offset, cmdline_scroll);
        })?;

        // Page size for PageUp/PageDown, derived from the current terminal height.
        let page = {
            let size = terminal.size()?;
            capacity_of(popup_dims(size.width, size.height, flat_rows.len()).1)
        };

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if selected > 0
                        && let Some(n) = supported_at_or_before(&flat_rows, selected - 1)
                    {
                        selected = n;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(n) = supported_at_or_after(&flat_rows, selected + 1) {
                        selected = n;
                    }
                }
                KeyCode::PageUp => {
                    let target = selected.saturating_sub(page);
                    selected = supported_at_or_before(&flat_rows, target)
                        .or_else(|| supported_at_or_after(&flat_rows, target))
                        .unwrap_or(selected);
                }
                KeyCode::PageDown => {
                    let target = (selected + page).min(flat_rows.len() - 1);
                    selected = supported_at_or_after(&flat_rows, target)
                        .or_else(|| supported_at_or_before(&flat_rows, target))
                        .unwrap_or(selected);
                }
                KeyCode::Home => {
                    if let Some(n) = supported_at_or_after(&flat_rows, 0) {
                        selected = n;
                    }
                }
                KeyCode::End => {
                    if let Some(n) = supported_at_or_before(&flat_rows, flat_rows.len() - 1) {
                        selected = n;
                    }
                }
                KeyCode::Left => {
                    cmdline_scroll = cmdline_scroll.saturating_sub(4);
                }
                KeyCode::Right => {
                    cmdline_scroll = cmdline_scroll.saturating_add(4);
                }
                KeyCode::Enter => {
                    if is_supported(&flat_rows[selected]) {
                        return Ok(Some(flat_rows[selected].pid));
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

fn render_dialog(
    frame: &mut Frame,
    flat_rows: &[FlatRow],
    selected: usize,
    scroll_offset: &mut usize,
    cmdline_scroll: u16,
) {
    let area = frame.size();

    let (popup_w, popup_h) = popup_dims(area.width, area.height, flat_rows.len());
    let capacity = capacity_of(popup_h);
    let popup_x = (area.width - popup_w) / 2;
    let popup_y = (area.height - popup_h) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

    // Keep the selected row within the visible window.
    if selected < *scroll_offset {
        *scroll_offset = selected;
    } else if selected >= *scroll_offset + capacity {
        *scroll_offset = selected + 1 - capacity;
    }
    // Clamp so we never scroll past the end (e.g. after a terminal resize).
    let max_offset = flat_rows.len().saturating_sub(capacity);
    if *scroll_offset > max_offset {
        *scroll_offset = max_offset;
    }
    let top = *scroll_offset;

    frame.render_widget(Clear, popup_rect);

    fn prefix_depth(prefix: &str) -> usize {
        (prefix.len().saturating_sub(4)) / 4
    }

    let inner_w = popup_w.saturating_sub(4) as usize;
    let cmd_w = inner_w.saturating_sub(8 + 2 + 1 + 2 + 1 + 2 + 22 + 4);

    let mut lines = Vec::new();

    let header = format!(
        "{:>8}  {}  {}  {:<22}    {}",
        "PID", "R", "S", "Version/Name", "Command Line"
    );
    lines.push(Line::from(Span::raw(header)));
    lines.push(Line::from(Span::raw("-".repeat(inner_w))));

    let end = (top + capacity).min(flat_rows.len());
    for (i, row) in flat_rows.iter().enumerate().take(end).skip(top) {
        let display_name = if row.is_python {
            row.version.as_deref().unwrap_or("-").to_string()
        } else {
            row.name.clone()
        };
        let r_char = if row.is_python && row.runtime_found {
            "Y"
        } else if row.is_python {
            "N"
        } else {
            "-"
        };
        let s_char = if row.is_python && row.supports_stats {
            "Y"
        } else if row.is_python {
            "N"
        } else {
            "-"
        };
        let indent = "  ".repeat(prefix_depth(&row.prefix));
        let full_name = format!("{}{}", indent, display_name);

        let scroll = cmdline_scroll as usize;
        let cmd_display: String = if scroll < row.cmdline.len() {
            row.cmdline.chars().skip(scroll).take(cmd_w).collect()
        } else {
            String::new()
        };

        let row_str = format!(
            "{:>8}  {}  {}  {:<22}    {}",
            row.pid, r_char, s_char, full_name, cmd_display
        );

        let supported = row.is_python && row.runtime_found && row.supports_stats;
        let style = if i == selected {
            Style::new().bg(Color::DarkGray).fg(Color::White)
        } else if !supported {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        lines.push(Line::from(Span::styled(row_str, style)));
    }

    let total = flat_rows.len();
    let up_marker = if top > 0 { "▲ " } else { "" };
    let down_marker = if end < total { " ▼" } else { "" };
    let status = format!(
        " {}{}-{} of {}{} ",
        up_marker,
        top + 1,
        end,
        total,
        down_marker
    );

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title(" Select Python PID ")
            .title_alignment(Alignment::Center)
            .title(
                Title::from(status)
                    .alignment(Alignment::Center)
                    .position(Position::Bottom),
            ),
    );

    frame.render_widget(paragraph, popup_rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, is_python: bool, runtime_found: bool, supports_stats: bool) -> FlatRow {
        FlatRow {
            pid,
            name: String::new(),
            prefix: String::new(),
            cmdline: String::new(),
            is_python,
            version: None,
            runtime_found,
            supports_stats,
        }
    }

    /// A row is a selectable target only when it is python AND its runtime was found
    /// AND it decodes GC stats — the three gates the picker skips over.
    #[test]
    fn is_supported_requires_all_three_gates() {
        assert!(is_supported(&row(1, true, true, true)));
        assert!(!is_supported(&row(1, false, true, true)));
        assert!(!is_supported(&row(1, true, false, true)));
        assert!(!is_supported(&row(1, true, true, false)));
    }

    /// Down-navigation lands on the next supported row, skipping unsupported ones, and
    /// reports `None` once there are none left (so the cursor can't run off the end).
    #[test]
    fn supported_at_or_after_skips_unsupported_rows() {
        let rows = [
            row(10, true, true, true),    // 0 supported
            row(11, true, false, false),  // 1
            row(12, false, false, false), // 2
            row(13, true, true, true),    // 3 supported
        ];
        assert_eq!(supported_at_or_after(&rows, 0), Some(0));
        assert_eq!(supported_at_or_after(&rows, 1), Some(3), "skips 1 and 2");
        assert_eq!(supported_at_or_after(&rows, 4), None, "past the end");
        assert_eq!(
            supported_at_or_after(&[row(1, false, false, false)], 0),
            None
        );
    }

    /// Up-navigation is the mirror: the nearest supported row at or before the cursor.
    #[test]
    fn supported_at_or_before_scans_backwards() {
        let rows = [
            row(10, true, true, true),    // 0 supported
            row(11, true, false, false),  // 1
            row(12, false, false, false), // 2
            row(13, true, true, true),    // 3 supported
        ];
        assert_eq!(supported_at_or_before(&rows, 3), Some(3));
        assert_eq!(
            supported_at_or_before(&rows, 2),
            Some(0),
            "skips 2 and 1 back to 0"
        );
        assert_eq!(supported_at_or_before(&rows, 0), Some(0));
        assert_eq!(
            supported_at_or_before(&[row(1, false, false, false)], 0),
            None
        );
    }

    /// The popup is 85% of terminal width, and its height is one row per entry plus 4
    /// chrome lines, shrunk to fit the terminal and clamped to [12, 30] — including the
    /// documented floor at 12 on a terminal too short to honor it.
    #[test]
    fn popup_dims_clamp_height_between_12_and_30() {
        assert_eq!(popup_dims(100, 40, 5), (85, 12), "few rows floor at 12");
        assert_eq!(popup_dims(100, 40, 50), (85, 30), "many rows cap at 30");
        assert_eq!(popup_dims(100, 20, 50), (85, 16), "shrinks to area_h - 4");
        assert_eq!(
            popup_dims(100, 10, 50),
            (85, 12),
            "floors at 12 even when taller than the terminal"
        );
    }

    /// The visible-row capacity is the popup height minus 4 chrome lines, never below 1.
    #[test]
    fn capacity_of_subtracts_chrome_and_never_reaches_zero() {
        assert_eq!(capacity_of(30), 26);
        assert_eq!(capacity_of(12), 8);
        assert_eq!(capacity_of(4), 1, "floors at 1");
        assert_eq!(capacity_of(0), 1);
    }
}
