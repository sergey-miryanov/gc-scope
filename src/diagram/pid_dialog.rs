use std::collections::HashMap;
use std::io::Stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui::Terminal;

use crate::list_pids::{build_flat_rows, FlatRow, ProcessInfo};

pub fn show_pid_dialog(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    processes: &[ProcessInfo],
    pid_info_map: &HashMap<u32, (String, u32)>,
) -> Result<u32> {
    if processes.is_empty() {
        anyhow::bail!("No Python processes found");
    }

    let flat_rows = build_flat_rows(processes, pid_info_map);

    if flat_rows.is_empty() {
        anyhow::bail!("No Python processes found");
    }

    fn is_supported(r: &FlatRow) -> bool {
        r.is_python && r.runtime_found && r.offsets_known
    }

    let mut selected = {
        let mut i = 0;
        while i < flat_rows.len() && !is_supported(&flat_rows[i]) { i += 1; }
        if i >= flat_rows.len() { anyhow::bail!("No supported Python processes found"); }
        i
    };
    let mut cmdline_scroll = 0u16;

    loop {
        terminal.draw(|f| {
            render_dialog(f, &flat_rows, selected, cmdline_scroll);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            let mut n = selected.saturating_sub(1);
                            while n > 0 && !is_supported(&flat_rows[n]) {
                                n = n.saturating_sub(1);
                            }
                            if is_supported(&flat_rows[n]) {
                                selected = n;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let mut n = selected.saturating_add(1);
                            while n < flat_rows.len() && !is_supported(&flat_rows[n]) {
                                n = n.saturating_add(1);
                            }
                            if n < flat_rows.len() && is_supported(&flat_rows[n]) {
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
                                return Ok(flat_rows[selected].pid);
                            }
                        }
                        KeyCode::Char('q') | KeyCode::Esc => {
                            anyhow::bail!("Cancelled by user");
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn v_char(row: &FlatRow) -> &'static str {
    match row.check_ok {
        Some(true) => "Y",
        Some(false) => "N",
        None => "-",
    }
}

fn render_dialog(
    frame: &mut Frame,
    flat_rows: &[FlatRow],
    selected: usize,
    cmdline_scroll: u16,
) {
    let area = frame.size();

    let popup_w = (area.width as f64 * 0.85) as u16;
    let popup_h = ((flat_rows.len() as u16 + 4).min(area.height - 4).min(30)).max(12);
    let popup_x = (area.width - popup_w) / 2;
    let popup_y = (area.height - popup_h) / 2;
    let popup_rect = Rect::new(popup_x, popup_y, popup_w, popup_h);

    frame.render_widget(Clear, popup_rect);

    fn prefix_depth(prefix: &str) -> usize {
        (prefix.len().saturating_sub(4)) / 4
    }

    let inner_w = popup_w.saturating_sub(4) as usize;
    let cmd_w = inner_w.saturating_sub(8 + 2 + 1 + 2 + 1 + 2 + 1 + 2 + 22 + 4);

    let mut lines = Vec::new();

    let header = format!(
        "{:>8}  {}  {}  {}  {:<22}    {}",
        "PID", "R", "S", "V", "Version/Name", "Command Line"
    );
    lines.push(Line::from(Span::raw(header)));
    lines.push(Line::from(Span::raw("-".repeat(inner_w))));

    for (i, row) in flat_rows.iter().enumerate() {
        let display_name = if row.is_python {
            row.version.as_deref().unwrap_or("-").to_string()
        } else {
            row.name.clone()
        };
        let r_char = if row.is_python && row.runtime_found { "Y" } else if row.is_python { "N" } else { "-" };
        let s_char = if row.is_python && row.offsets_known { "Y" } else if row.is_python { "N" } else { "-" };
        let v = v_char(row);
        let indent = "  ".repeat(prefix_depth(&row.prefix));
        let full_name = format!("{}{}", indent, display_name);

        let scroll = cmdline_scroll as usize;
        let cmd_display: String = if scroll < row.cmdline.len() {
            row.cmdline.chars().skip(scroll).take(cmd_w).collect()
        } else {
            String::new()
        };

        let row_str = format!(
            "{:>8}  {}  {}  {}  {:<22}    {}",
            row.pid, r_char, s_char, v, full_name, cmd_display
        );

        let supported = row.is_python && row.runtime_found && row.offsets_known;
        let style = if i == selected {
            Style::new().bg(Color::DarkGray).fg(Color::White)
        } else if !supported {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::default()
        };

        lines.push(Line::from(Span::styled(row_str, style)));
    }

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .title(" Select Python PID ")
            .title_alignment(Alignment::Center),
    );

    frame.render_widget(paragraph, popup_rect);
}
