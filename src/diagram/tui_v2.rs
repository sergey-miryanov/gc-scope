use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Terminal;

use super::collect::{avg_collection_time_per_gen, collections_rate_from_slots, CollectedData};
use super::render::{debug_offsets_tree, gen_stats_layout, tree_prefixes};

// ── Layout constants ──────────────────────────────────────────────
const OUTER_W: usize = 158;
const PL: usize = 65;
const PR: usize = 90;
const INNER_W: usize = PL - 4;      // 61
const INNER_TW: usize = INNER_W - 2; // 59


/// Restores the terminal (raw mode, alternate screen, cursor) on drop, so it is cleaned
/// up on every exit path — including an early `?` return from the PID dialog or setup —
/// not just when the main loop breaks normally.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = out.execute(crossterm::cursor::Show);
        let _ = disable_raw_mode();
        let _ = out.execute(LeaveAlternateScreen);
    }
}

// ── Entry point ───────────────────────────────────────────────────
pub fn run_tui(pid: Option<u32>, mut rate_ms: u64, duration_secs: Option<u64>, mut glitch_enabled: bool) -> Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    stdout().execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // PID selection dialog if no PID given
    let mut pid = match pid {
        Some(p) => p,
        None => {
            let (processes, pid_info_map) = crate::list_pids::list_python_processes(true)?;
            match super::pid_dialog::show_pid_dialog(&mut terminal, &processes, &pid_info_map)? {
                Some(p) => p,
                None => return Ok(()), // user cancelled the picker — exit cleanly
            }
        }
    };

    let mut ver = crate::remote_debugging::version::detect(pid)?;
    let mut start = Instant::now();
    let mut frame: u64 = 0;
    let mut scroll: u16 = 0;
    let mut selected_slot: usize = 0;
    let mut debug_offsets_show_tree: bool = true;
    let mut debug_offsets_show_hex: bool = true;
    let mut show_runtime_hex: bool = false;

    // Glitch state
    let mut rng_state: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(12345);
    let mut glitch_active = false;
    let mut next_glitch_at = Instant::now();
    let mut glitch_end = Instant::now();

    // Connection-lost sequence state
    let mut cl_active = false;
    let mut cl_phase: u8 = 0;        // 0=inactive, 1=build-up, 2=message
    let mut cl_phase_start = Instant::now();
    let mut cl_end = Instant::now();
    let mut next_cl_show = Instant::now() + Duration::from_secs(30);
    let mut cl_jx: i32 = 0;
    let mut cl_jy: i32 = 0;
    let mut cl_last_jitter = Instant::now();

    let result = loop {
        if event::poll(Duration::from_millis(rate_ms))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Up | KeyCode::Char('k') => {
                            selected_slot = selected_slot.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            selected_slot = selected_slot.saturating_add(1);
                        }
                        KeyCode::Char('t') => {
                            debug_offsets_show_tree = !debug_offsets_show_tree;
                        }
                        KeyCode::Char('h') => {
                            debug_offsets_show_hex = !debug_offsets_show_hex;
                        }
                        KeyCode::Char('o') => {
                            if debug_offsets_show_tree || debug_offsets_show_hex {
                                debug_offsets_show_tree = false;
                                debug_offsets_show_hex = false;
                            } else {
                                debug_offsets_show_tree = true;
                                debug_offsets_show_hex = true;
                            }
                        }
                        KeyCode::Char('d') => {
                            show_runtime_hex = !show_runtime_hex;
                        }
                        KeyCode::Char('r') => {
                            rate_ms = rate_ms.saturating_sub(10).max(10);
                        }
                        KeyCode::Char('R') => {
                            rate_ms = rate_ms.saturating_add(10);
                        }
                        KeyCode::Char('g') => {
                            glitch_enabled = !glitch_enabled;
                        }
                        KeyCode::Char('p') => {
                            if let Ok((processes, pid_info_map)) = crate::list_pids::list_python_processes(true) {
                                if let Ok(Some(new_pid)) = super::pid_dialog::show_pid_dialog(&mut terminal, &processes, &pid_info_map) {
                                    pid = new_pid;
                                    if let Ok(new_ver) = crate::remote_debugging::version::detect(pid) {
                                        ver = new_ver;
                                    }
                                    start = Instant::now();
                                    scroll = 0;
                                    selected_slot = 0;
                                    frame = 0;
                                    debug_offsets_show_tree = true;
                                    debug_offsets_show_hex = true;
                                    show_runtime_hex = false;
                                }
                            }
                        }
                        KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                        KeyCode::PageDown => scroll = scroll.saturating_add(10),
                        KeyCode::Home => scroll = 0,
                        KeyCode::End => scroll = u16::MAX,
                        _ => {}
                    }
                }
            }
        }

        let data = match crate::diagram::collect::collect_data(pid, &ver) {
            Ok(d) => d,
            Err(e) => {
                terminal.draw(|f| {
                    let area = f.size();
                    let msg =
                        Paragraph::new(format!("Error: {}", e)).block(Block::bordered().title(" Error "));
                    f.render_widget(msg, area);
                })?;
                std::thread::sleep(Duration::from_secs(2));
                break Err(e);
            }
        };

        // Clamp selected_slot to valid range based on new data
        let slot_count = data.interpreter.gc.generation_stats.slots.len();
        let max_slot = slot_count.saturating_sub(1);
        if selected_slot > max_slot {
            selected_slot = max_slot;
        }

        let elapsed = start.elapsed();
        frame += 1;

        // Auto-exit if duration exceeded
        if let Some(max_dur) = duration_secs {
            if elapsed.as_secs() >= max_dur {
                break Ok(());
            }
        }

        // Glitch + connection-lost timer logic (wall-clock based)
        if glitch_enabled {
            let now = Instant::now();
            if cl_active {
                if cl_phase == 1 {
                    // Build-up phase lasts 1 second
                    if now >= cl_phase_start + Duration::from_secs(1) {
                        cl_phase = 2;
                        cl_phase_start = now;
                        let msg_dur = rand_range(&mut rng_state, 4000, 8000);
                        cl_end = now + Duration::from_millis(msg_dur as u64);
                    }
                } else if cl_phase == 2 {
                    if now >= cl_end {
                        cl_active = false;
                        cl_phase = 0;
                        // Double next normal glitch cooldown
                        let delay = rand_range(&mut rng_state, 1000, 8000) * 2;
                        next_glitch_at = now + Duration::from_millis(delay as u64);
                        // Schedule next sequence in ~30 s
                        let interval = rand_range(&mut rng_state, 25000, 35000);
                        next_cl_show = now + Duration::from_millis(interval as u64);
                    }
                }
            } else if now >= next_cl_show {
                cl_active = true;
                cl_phase = 1;
                cl_phase_start = now;
            } else if glitch_active {
                if now >= glitch_end {
                    glitch_active = false;
                    let delay = rand_range(&mut rng_state, 1000, 8000);
                    next_glitch_at = now + Duration::from_millis(delay as u64);
                }
            } else if now >= next_glitch_at {
                glitch_active = true;
                let dur = rand_range(&mut rng_state, 200, 600);
                glitch_end = now + Duration::from_millis(dur as u64);
            }
        }

        let slots = &data.interpreter.gc.generation_stats.slots;
        let (rate_per_gen, avg_coll_time_per_gen) = (
            collections_rate_from_slots(slots),
            avg_collection_time_per_gen(slots),
        );
        let (styled_lines, _slot_line) = build_lines(&data, rate_per_gen, avg_coll_time_per_gen, selected_slot, debug_offsets_show_tree, debug_offsets_show_hex, show_runtime_hex);

        // Pre-compute glitch draw conditions
        let should_glitch = glitch_enabled && !cl_active && glitch_active;
        let should_buildup = glitch_enabled && cl_active && cl_phase == 1;
        let should_msg = glitch_enabled && cl_active && cl_phase == 2;
        let buildup_progress = if should_buildup {
            cl_phase_start.elapsed().as_secs_f64().min(1.0)
        } else {
            0.0
        };
        let glitch_badge_active = glitch_active || cl_active;

        // Update CL jitter at most every 200ms
        if cl_active && cl_phase == 2 {
            let now = Instant::now();
            if now >= cl_last_jitter + Duration::from_millis(200) {
                cl_jx = ((rand_range(&mut rng_state, 0, 2) as i32) - 1).clamp(-1, 1);
                cl_jy = ((rand_range(&mut rng_state, 0, 2) as i32) - 1).clamp(-1, 1);
                cl_last_jitter = now;
            }
        }

        terminal.draw(|f| {
            let area = f.size();
            let chunks = ratatui::layout::Layout::vertical([
                ratatui::layout::Constraint::Min(1),
                ratatui::layout::Constraint::Length(1),
            ])
            .split(area);

            let line_count = styled_lines.len() as u16;
            let max_scroll = line_count.saturating_sub(chunks[0].height.saturating_sub(2));
            if scroll > max_scroll {
                scroll = max_scroll;
            }

            let title = format!(
                " gcscope tui — PID {} — Frame {} @ {:.1}s — Rate {}ms{} ",
                pid,
                frame,
                elapsed.as_secs_f64(),
                rate_ms,
                duration_secs.map_or(String::new(), |d| format!(" — Dur {d}s"))
            );
            let content = Paragraph::new(Text::from(styled_lines))
                .block(Block::bordered().border_type(BorderType::Plain).title(title))
                .scroll((scroll, 0));

            let status = status_bar(scroll, max_scroll, selected_slot, slot_count, rate_ms, glitch_badge_active, cl_active, glitch_enabled, data.collect_duration);
            f.render_widget(content, chunks[0]);
            f.render_widget(status, chunks[1]);
            if should_buildup {
                apply_connection_lost_buildup(f.buffer_mut(), &mut rng_state, buildup_progress);
            } else if should_msg {
                draw_connection_lost_box(f.buffer_mut(), cl_jx, cl_jy);
                for _ in 0..rand_range(&mut rng_state, 3, 6) {
                    apply_one_glitch(f.buffer_mut(), &mut rng_state);
                }
            } else if should_glitch {
                apply_glitch(f.buffer_mut(), &mut rng_state);
            }
        })?;
    };

    // Terminal teardown is handled by `_guard` on drop, covering every exit path.
    result
}

fn status_bar(scroll: u16, max_scroll: u16, slot: usize, slot_count: usize, rate_ms: u64, glitch_active: bool, cl_active: bool, glitch_enabled: bool, collect_dur: Duration) -> Paragraph<'static> {
    let style = Style::new().bg(Color::Blue).fg(Color::White);
    let scroll_pct = if max_scroll > 0 {
        format!(" {:>3}%", scroll * 100 / max_scroll)
    } else {
        " 100%".to_string()
    };
    let slot_text = if slot_count > 0 {
        format!(" slot {}/{} ", slot + 1, slot_count)
    } else {
        " no slots ".to_string()
    };
    let rate_text = format!(" {}ms ", rate_ms);
    let badge = if cl_active {
        Span::styled(" [CL] ", style.bg(Color::Red).fg(Color::White).add_modifier(ratatui::style::Modifier::BOLD))
    } else if glitch_active {
        Span::styled(" [G] ", style.bg(Color::Red).fg(Color::White).add_modifier(ratatui::style::Modifier::BOLD))
    } else {
        Span::raw("")
    };
    let glitch_label = if glitch_enabled { "on" } else { "off" };
    let glitch_style = if glitch_enabled { style } else { style.bg(Color::DarkGray) };
    let text = Line::from(vec![
        Span::styled(" [q] quit ", style.bg(Color::DarkGray)),
        Span::styled(" [p] pick pid ", style),
        Span::styled(" [t] tree [h] hex [o] collapse [d] Dbg/Rt", style),
        Span::styled(" [r/R] rate", style),
        Span::styled(format!(" [g] {}", glitch_label), glitch_style),
        Span::styled(format!(" {} ", fmt_duration_ns(collect_dur)), style.bg(Color::DarkGray)),
        Span::styled(rate_text, style.bg(Color::DarkGray)),
        badge,
        Span::styled(" [\u{2191}\u{2193}/jk] ", style),
        Span::styled(slot_text, style.bg(Color::DarkGray)),
        Span::styled(format!(" [PgUp/PgDn] scroll{} ", scroll_pct), style),
    ]);
    Paragraph::new(text)
}

// ── Main line builder ─────────────────────────────────────────────
fn build_lines(data: &CollectedData, rate_per_gen: [f64; 3], avg_coll_time_per_gen: [f64; 3], selected_slot: usize, debug_offsets_show_tree: bool, debug_offsets_show_hex: bool, show_runtime_hex: bool) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let s1 = section_debug_offsets(data, debug_offsets_show_tree, debug_offsets_show_hex, show_runtime_hex);
    let s2 = section_interpreter(data);
    let s1_len = s1.len();
    let s2_len = s2.len();
    lines.extend(s1);
    lines.push(Line::from(""));
    lines.extend(s2);
    lines.push(Line::from(""));
    lines.extend(section_gc_stats(data, rate_per_gen, avg_coll_time_per_gen, selected_slot));
    // Slot row in section_gc_stats starts at index 3 (top/buffer/top) + 7 header lines in the interleave
    let slot_line_idx = s1_len + 1 + s2_len + 1 + 3 + 7 + selected_slot;
    (lines, slot_line_idx)
}

// ── Box helpers ───────────────────────────────────────────────────
fn top() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}

fn l(content: &str) -> String {
    format!("| {:<w$} |", content, w = OUTER_W)
}

fn plain_line(left: &str, right: &str) -> Line<'static> {
    Line::from(Span::raw(format!(
        "|{:<pl$} | {:<pr$}|",
        left, right, pl = PL, pr = PR
    )))
}

fn full_line(left: &str, right_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw(format!("|{:<pl$} | ", left, pl = PL))];
    let rw: usize = right_spans.iter().map(|s| s.content.len()).sum();
    spans.extend(right_spans);
    if rw < PR {
        spans.push(Span::raw(" ".repeat(PR - rw)));
    }
    spans.push(Span::raw("|"));
    Line::from(spans)
}

fn span_line(left_spans: Vec<Span<'static>>, right_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw("|")];
    let lw: usize = left_spans.iter().map(|s| s.content.len()).sum();
    spans.extend(left_spans);
    if lw < PL {
        spans.push(Span::raw(" ".repeat(PL - lw)));
    }
    spans.push(Span::raw(" | "));
    let rw: usize = right_spans.iter().map(|s| s.content.len()).sum();
    spans.extend(right_spans);
    if rw < PR {
        spans.push(Span::raw(" ".repeat(PR - rw)));
    }
    spans.push(Span::raw("|"));
    Line::from(spans)
}

fn styled_left_inner_box(content: &str, color: Option<Color>) -> Vec<Span<'static>> {
    let s = format!("  | {:<tw$} |", content, tw = INNER_TW);
    if let Some(c) = color {
        let style = Style::new().bg(c).fg(Color::Black);
        vec![
            Span::raw(s[..4].to_string()),
            Span::styled(s[4..4 + INNER_TW].to_string(), style),
            Span::raw(s[4 + INNER_TW..].to_string()),
        ]
    } else {
        vec![Span::raw(s)]
    }
}

fn padding_hex_right(hex_spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let rw: usize = hex_spans.iter().map(|s| s.content.len()).sum();
    let mut spans = hex_spans;
    if rw < PR {
        spans.push(Span::raw(" ".repeat(PR - rw)));
    }
    spans
}

// ── Hex dump renderer ─────────────────────────────────────────────
fn hex_dump_rows(
    bytes: &[u8],
    limit: usize,
    highlights: &[(usize, u8, Color)],
    base_offset: usize,
) -> Vec<Vec<Span<'static>>> {
    let display = bytes.len().min(limit);
    let mut rows = Vec::new();
    for chunk in bytes[..display].chunks(16) {
        let base = chunk.as_ptr() as usize - bytes.as_ptr() as usize + base_offset;
        let mut spans = Vec::new();
        spans.push(Span::raw(format!("  {:08x}  ", base)));
        for (i, &b) in chunk.iter().enumerate() {
            let global_off = base + i;
            let hl = highlights.iter().find(|&&(off, len, _)| {
                global_off >= off && global_off < off + len as usize
            });
            let hl_color = hl.map(|&(_, _, c)| c);
            let next_in_same = hl.map_or(false, |&(off, len, _)| {
                global_off + 1 < off + len as usize
            });

            // Emit the two hex digits
            if let Some(c) = hl_color {
                spans.push(Span::styled(
                    format!("{:02x}", b),
                    Style::new().bg(c).fg(Color::Black),
                ));
            } else {
                spans.push(Span::raw(format!("{:02x}", b)));
            }

            // Emit interspace to next byte
            if i < 15 {
                let space = if i == 7 { "  " } else { " " };
                if let Some(c) = hl_color.filter(|_| next_in_same) {
                    spans.push(Span::styled(
                        space.to_string(),
                        Style::new().bg(c).fg(Color::Black),
                    ));
                } else {
                    spans.push(Span::raw(space.to_string()));
                }
            }
        }
        // Pad the hex column to the full-row width so the ascii column stays aligned on a
        // short final row (region length not a multiple of 16).
        if chunk.len() < 16 {
            let pad = hex_col_emitted(16) - hex_col_emitted(chunk.len());
            spans.push(Span::raw(" ".repeat(pad)));
        }
        let ascii: String = chunk
            .iter()
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
            .collect();
        spans.push(Span::raw(format!(" |{}", ascii)));
        rows.push(spans);
    }
    rows
}

/// Rendered width of the hex-bytes column emitted by `hex_dump_rows` for `n` bytes
/// (`n <= 16`): 2 chars per byte plus one interspace per byte where `i < 15` (2 at the
/// mid-row gap `i == 7`, else 1). A full 16-byte row is 48 chars wide.
fn hex_col_emitted(n: usize) -> usize {
    (0..n)
        .map(|i| 2 + if i < 15 { if i == 7 { 2 } else { 1 } } else { 0 })
        .sum()
}

// ── Section 1: _Py_DebugOffsets ───────────────────────────────────
fn section_debug_offsets(data: &CollectedData, show_tree: bool, show_hex: bool, show_runtime_hex: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let debug_size = data.debug_offsets_size as usize;

    lines.push(Line::from(Span::raw(top())));
    if !show_tree && !show_hex {
        lines.push(Line::from(Span::raw(l(&format!(
            "_Py_DebugOffsets (embedded in _PyRuntime) @ {:#x}  (size: {} bytes)  [hidden - press t/h to show]",
            data.runtime_addr, debug_size
        )))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }
    lines.push(Line::from(Span::raw(l(&format!(
        "_Py_DebugOffsets (embedded in _PyRuntime) @ {:#x}  (size: {} bytes)",
        data.runtime_addr, debug_size
    )))));
    lines.push(Line::from(Span::raw(top())));

    let bytes = &data.runtime_raw_bytes;

    let (hex_start, hex_label) = if show_runtime_hex {
        (debug_size, "Runtime")
    } else {
        (0, "DebugOffsets")
    };
    let hex_range_end = hex_start + debug_size;
    let hex_slice = if hex_range_end <= bytes.len() {
        &bytes[hex_start..hex_range_end]
    } else if hex_start < bytes.len() {
        &bytes[hex_start..]
    } else {
        &[]
    };

    // `gc.generation_stats_size` is read from the target's `_Py_DebugOffsets`, so the
    // accessor already holds the process-published value (0 on builds without the field).
    let gen_stats_size = data.offsets.gc_generation_stats_size();
    let gs = gen_stats_layout(gen_stats_size);

    // Drive the GC-state subtree from actual, version-correct layout: the `gc`
    // sub-struct fields and the resolved per-slot field layout (which reflects the
    // clean-vs-`+inc` selection).
    let gc_fields = data.offsets.gc_debug_fields();
    let offset_table = data.offsets.to_offset_table(data.pid, data.runtime_addr);
    let slot_fields = offset_table.gc_layout.map(|l| l.fields);
    let tree = debug_offsets_tree(&gc_fields, slot_fields);
    let prefixes = tree_prefixes(&tree);

    let debug_highlights = if !show_runtime_hex {
        data.offsets.debug_offsets_highlight_regions()
    } else {
        vec![]
    };
    let hex_highlights: Vec<(usize, u8, Color)> = debug_highlights.iter()
        .filter(|(off, len, _, _)| hex_slice.len() >= off + *len as usize)
        .map(|(off, len, label, _)| {
            let color = match *label {
                "cookie[8]" => Color::Green,
                "interpreters_head" => Color::Cyan,
                "next" => Color::Yellow,
                "gc" => Color::Magenta,
                _ => Color::White,
            };
            (*off, *len, color)
        })
        .collect();

    let hex_rows = if show_hex {
        hex_dump_rows(hex_slice, hex_slice.len(), &hex_highlights, hex_start)
    } else {
        vec![]
    };

    let read_u64 = |off: usize| -> u64 {
        if off + 8 <= bytes.len() && off + 8 <= debug_size {
            u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
        } else {
            0
        }
    };

    let fmt_val = |val: u64, name: &str| -> String {
        if name.contains("cookie") {
            let b = val.to_le_bytes();
            let sv = String::from_utf8_lossy(&b);
            format!("\"{}\"", sv.trim_end_matches('\0'))
        } else if name.contains("version") {
            format!("0x{:08x}", val)
        } else if name.contains("size") {
            format!("{}", val)
        } else if val > 0xFFFF_FFFF {
            format!("{:#x}", val)
        } else if val > 0x10000 {
            format!("{} ({:#x})", val, val)
        } else {
            format!("{}", val)
        }
    };

    let derived_val = |label: &str| -> String {
        let (item_size, young_bytes, _old_bytes, i0, i1, i2, _o0) = gs;
        match label {
            "item_size" => format!("{}", item_size),
            "young_slots (11)" => format!("11 x {} = {} bytes", item_size, young_bytes),
            "index0" => format!("+{}", i0),
            "old0_slots (3)" => format!("3 x {} bytes", item_size),
            "index1" => format!("+{}", i1),
            "old1_slots (3)" => format!("3 x {} bytes", item_size),
            "index2" => format!("+{}", i2),
            _ => String::new(),
        }
    };

    let format_tree_line = |prefix: &str, offset_str: &str, name: &str, value_str: &str| -> String {
        let before = format!("{}{}{}", prefix, offset_str, name);
        let pad = PL.saturating_sub(before.len() + value_str.len());
        format!("{}{}{}", before, " ".repeat(pad), value_str)
    };

    use std::collections::HashMap;
    let tree_row_colors: HashMap<&str, Color> = [
        ("cookie[8]", Color::Green),
        ("interpreters_head", Color::Cyan),
        ("next", Color::Yellow),
        ("gc", Color::Magenta),
    ].into_iter().collect();
    let mut tree_highlight_rows: Vec<(usize, Color)> = Vec::new();

    let mut left_owned: Vec<String> = Vec::new();
    if show_tree {
        left_owned.push(format!("{:<pl$}", "Fields:", pl = PL));
        for (i, entry) in tree.iter().enumerate() {
            let pfx = &prefixes[i];
            let line = match entry.kind {
                super::render::TreeEntryKind::RawValue { offset } => {
                    let val = read_u64(offset);
                    let f = fmt_val(val, entry.label);
                    format_tree_line(pfx, &format!("0x{:04x}  ", offset), entry.label, &f)
                }
                super::render::TreeEntryKind::Group => {
                    format_tree_line(pfx, "", entry.label, "")
                }
                super::render::TreeEntryKind::Derived => {
                    let val_str = derived_val(entry.label);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
                super::render::TreeEntryKind::Layout { field_type: _, field_offset } => {
                    let val_str = format!("+{}", field_offset);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
            };
            left_owned.push(line);
            let label: &str = &entry.label;
            if let Some(&color) = tree_row_colors.get(label) {
                tree_highlight_rows.push((left_owned.len() - 1, color));
            }
        }
    } else {
        left_owned.push(format!("{:<pl$}", "[Tree hidden - press t to show]", pl = PL));
    }

    if !show_tree && show_hex {
        // Full-width hex dump (left panel area)
        let hex_len = hex_slice.len();
        let hex_end_off = hex_start + hex_len.saturating_sub(1);
        lines.push(Line::from(Span::raw(l(&format!(
            "Hex Dump ({}, 0x{:04x}-0x{:04x}, {} bytes):",
            hex_label, hex_start, hex_end_off, hex_len
        )))));
        for row in &hex_rows {
            let hex_content: String = row.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>().concat();
            lines.push(Line::from(Span::raw(l(&hex_content))));
        }
    } else {
        let hex_header: String = if show_hex {
            let hex_len = hex_slice.len();
            let hex_end_off = hex_start + hex_len.saturating_sub(1);
            format!("Hex Dump ({}, 0x{:04x}-0x{:04x}, {} bytes):",
                hex_label, hex_start, hex_end_off, hex_len)
        } else {
            "[Hex dump hidden - press h to show]".into()
        };

        let max_rows = left_owned.len().max(1 + hex_rows.len());
        for i in 0..max_rows {
            let lv = left_owned.get(i).map(|s| s.as_str()).unwrap_or("");
            if i == 0 {
                lines.push(plain_line(lv, &hex_header));
            } else {
                let ri = i - 1;
                let right = if ri < hex_rows.len() {
                    padding_hex_right(hex_rows[ri].clone())
                } else {
                    vec![Span::raw(" ".repeat(PR))]
                };
                if let Some(&(_, color)) = tree_highlight_rows.iter().find(|(idx, _)| *idx == i) {
                    let left_span = Span::styled(
                        format!("{:<pl$}", lv, pl = PL),
                        Style::new().bg(color).fg(Color::Black),
                    );
                    lines.push(span_line(vec![left_span], right));
                } else {
                    lines.push(full_line(lv, right));
                }
            }
        }
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

// ── Section 2: PyInterpreterState ─────────────────────────────────
fn section_interpreter(data: &CollectedData) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let interp = &data.interpreter;
    let off = &data.offsets;
    // Show the whole GC state struct (raw_bytes is read to exactly gc.size bytes), so the
    // dump matches the "GC struct (N bytes)" header. A fixed cap truncated larger structs
    // like the +inc build's 232-byte state.
    let hex_end = interp.gc.raw_bytes.len();

    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(&format!(
        "PyInterpreterState @ {:#x}  (struct: {} bytes)",
        interp.addr,
        off.interpreter_state_size()
    )))));
    lines.push(Line::from(Span::raw(top())));

    // ── Left panel content ──
    enum LeftItem {
        Plain(String),
        Styled(String, Color),
    }

    let mut left_items: Vec<LeftItem> = Vec::new();
    left_items.push(LeftItem::Plain(format!(
        "{:<pl$}",
        "Key offset values stored in _Py_DebugOffsets:",
        pl = PL
    )));
    for f in data.runtime_offset_fields() {
        if f.name.starts_with("runtime_state") || f.name.starts_with("gc") {
            continue;
        }
        let v = fmt_val(f.value);
        left_items.push(LeftItem::Plain(format!("    {:<30}  {:>18}", f.name, v)));
    }
    left_items.push(LeftItem::Plain(format!("{:<pl$}", "", pl = PL)));
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));
    left_items.push(LeftItem::Plain(format!(
        "  | {:<tw$} |",
        format!("GC State @ {:#x}", interp.addr + interp.gc_offset),
        tw = INNER_TW
    )));
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));

    // The descriptor `gc` sub-struct is append-only and shorter on older builds (2 fields on
    // 3.13/3.14, all 5 on 3.15+), so list only the fields this version actually publishes —
    // otherwise absent fields render as phantom `@ gc+0` / NULL rows. `gc_debug_fields()` is
    // the same version-correct source Section 1 uses.
    let present: Vec<&'static str> = off.gc_debug_fields().into_iter().map(|(n, _)| n).collect();

    let collecting_off = off.gc_collecting() as usize;
    let collecting_val = interp.gc.raw_bytes.get(collecting_off).copied().unwrap_or(0);
    if present.contains(&"size") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!("  {:<15} (store)    {}", "size", interp.gc_size),
            tw = INNER_TW
        )));
    }
    if present.contains(&"collecting") {
        left_items.push(LeftItem::Styled(
            format!(
                "  {:<15} @ gc+{:<4}  {}",
                "collecting", collecting_off, collecting_val
            ),
            Color::Yellow,
        ));
    }

    let frame_off = off.gc_frame() as usize;
    let frame_val = if frame_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[frame_off..frame_off + 8].try_into().unwrap())
    } else {
        0
    };
    if present.contains(&"frame") {
        left_items.push(LeftItem::Styled(
            format!(
                "  {:<15} @ gc+{:<4}  {:#x}",
                "frame", frame_off, frame_val
            ),
            Color::Cyan,
        ));
    }

    if present.contains(&"generation_stats_size") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!(
                "  {:<15} (store)    {}",
                "gen_stats_size",
                off.gc_generation_stats_size()
            ),
            tw = INNER_TW
        )));
    }

    let gen_stats_off = off.gc_generation_stats() as usize;
    let gen_stats_ptr = if gen_stats_off + 8 <= interp.gc.raw_bytes.len() {
        u64::from_le_bytes(interp.gc.raw_bytes[gen_stats_off..gen_stats_off + 8].try_into().unwrap())
    } else {
        0
    };
    let ptr_str = if gen_stats_ptr != 0 {
        format!("{:#x}", gen_stats_ptr)
    } else {
        "NULL".into()
    };
    if present.contains(&"generation_stats") {
        left_items.push(LeftItem::Plain(format!(
            "  | {:<tw$} |",
            format!("  {:<15} @ gc+{:<4}  {}", "gen_stats", gen_stats_off, ptr_str),
            tw = INNER_TW
        )));
    }
    left_items.push(LeftItem::Plain(format!("  +{}+", "-".repeat(INNER_W))));

    // ── Right panel: hex dump ──
    let right_header = format!("{:<pr$}", format!("GC struct ({} bytes) hex dump:", interp.gc_size), pr = PR);

    // GC struct highlights: collecting field (8 bytes) + frame field (8 bytes). Two separate
    // gates: the outer one is presence + bounds — it keeps absent fields on 3.13/3.14 (whose
    // offset accessors return 0, so `frame_val` would read the `size` bytes at offset 0) from
    // painting a bogus highlight. The inner one is value != 0 — a live signal the GC is
    // collecting right now, so a region only lights up while it's actually busy.
    let mut gc_highlights: Vec<(usize, u8, Color)> = Vec::new();
    // Kept as nested ifs (presence/bounds vs. live value) for readability, not collapsed.
    #[allow(clippy::collapsible_if)]
    if collecting_val != 0 {
        if present.contains(&"collecting") && collecting_off + 8 <= interp.gc.raw_bytes.len() {
            gc_highlights.push((collecting_off, 8, Color::Yellow));
        }
    }
    #[allow(clippy::collapsible_if)]
    if frame_val != 0 {
        if present.contains(&"frame") && frame_off + 8 <= interp.gc.raw_bytes.len() {
            gc_highlights.push((frame_off, 8, Color::Cyan));
        }
    }

    let hex_rows = hex_dump_rows(&interp.gc.raw_bytes, hex_end, &gc_highlights, 0);

    // ── Combine ──
    let max_rows = left_items.len().max(1 + hex_rows.len());
    for i in 0..max_rows {
        let ri = i.saturating_sub(1);
        let right = if i == 0 {
            // Header row: left header + right header
            let lv = match left_items.get(0) {
                Some(LeftItem::Plain(s)) => s.as_str(),
                _ => "",
            };
            lines.push(plain_line(lv, &right_header));
            continue;
        } else if ri < hex_rows.len() {
            padding_hex_right(hex_rows[ri].clone())
        } else {
            vec![Span::raw(" ".repeat(PR))]
        };

        match left_items.get(i) {
            Some(LeftItem::Plain(s)) => lines.push(full_line(s, right)),
            Some(LeftItem::Styled(s, c)) => {
                let left_spans = styled_left_inner_box(s, Some(*c));
                lines.push(span_line(left_spans, right));
            }
            None => lines.push(full_line("", right)),
        }
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

// ── Section 3: GC Generation Stats ────────────────────────────────
fn section_gc_stats(data: &CollectedData, rate_per_gen: [f64; 3], avg_coll_time_per_gen: [f64; 3], selected_slot: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let gc = &data.interpreter.gc.generation_stats;

    lines.push(Line::from(Span::raw(top())));

    if gc.stats_addr == 0 || gc.slots.is_empty() {
        lines.push(Line::from(Span::raw(l("GC Generation Stats: not available"))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }

    lines.push(Line::from(Span::raw(l(&format!(
        "GC Generation Stats Buffer @ {:#x}  (size: {} bytes)",
        gc.stats_addr, gc.stats_size
    )))));
    lines.push(Line::from(Span::raw(top())));

    let item_size = if gc.stats_size > 24 && gc.stats_size < 10000 {
        ((gc.stats_size - 24) / 17) as usize
    } else {
        gc.raw_stats_bytes.len().min(64)
    };

    // ── Left panel ──
    let mut left: Vec<String> = Vec::new();
    let gen_names = [
        ("Gen 0 (Young) - 11 slots", rate_per_gen[0], avg_coll_time_per_gen[0]),
        ("Gen 1 (Middle) - 3 slots", rate_per_gen[1], avg_coll_time_per_gen[1]),
        ("Gen 2 (Oldest) - 3 slots", rate_per_gen[2], avg_coll_time_per_gen[2]),
    ];
    for (name, rate, avg_coll) in &gen_names {
        let rate_str = fmt_rate(*rate);
        let coll_str = fmt_duration(*avg_coll);
        left.push(format!("{:<pl$}", format!("{}  (rate = {}, avg coll = {})", name, rate_str, coll_str), pl = PL));
    }
    left.push(format!(
        "{:<pl$}",
        format!("slot size: {} bytes  |  total buffer: {} bytes", item_size, gc.stats_size),
        pl = PL
    ));
    left.push(format!("{:<pl$}", "", pl = PL));
    let hdr = format!(
        "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11}",
        "gen", "slot", "collections", "collected", "heap", "duration(s)"
    );
    let hdr_len = hdr.len();
    left.push(format!("{:<pl$}", hdr, pl = PL));
    left.push(format!("  {}", "-".repeat(hdr_len - 2)));

    for slot in &gc.slots {
        let gen_label = format!("{}", slot.generation);
        let heap = fmt_bytes(slot.heap_size as u64);
        left.push(format!(
            "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11.3}",
            gen_label, slot.slot, slot.collections, slot.collected, heap, slot.duration
        ));
    }

    // ── Right panel ──
    let slot = &gc.slots[selected_slot];
    let slot_raw_start = slot.byte_offset;
    let slot_raw_end = (slot_raw_start + item_size).min(gc.raw_stats_bytes.len());
    let slot_bytes = &gc.raw_stats_bytes[slot_raw_start..slot_raw_end];
    let display_bytes = slot_bytes.len();

    let slot_field_names = [
        "ts_start",
        "ts_stop",
        "collections",
        "collected",
        "uncollectable",
        "candidates",
        "duration",
        "heap_size",
    ];

    let mut right_items: Vec<Vec<Span<'static>>> = Vec::new();
    // Header
    right_items.push(vec![Span::raw(format!(
        "{:<pr$}",
        format!("Slot #{} (gen {}, slot {}) of stats buffer:", selected_slot + 1, slot.generation, slot.slot),
        pr = PR
    ))]);

    // Hex dump of selected slot bytes with offset_of!-based highlights
    let slot_highlight_regions = data.offsets.gc_slot_highlight_regions();
    let slot_label_colors: std::collections::HashMap<&str, Color> = [
        ("ts_start", Color::Blue),
        ("ts_stop", Color::Blue),
        ("collections", Color::Green),
        ("collected", Color::Magenta),
        ("duration", Color::Yellow),
        ("heap_size", Color::Cyan),
    ].into_iter().collect();
    let adjusted_highlights: Vec<(usize, u8, Color)> = slot_highlight_regions.iter()
        .map(|&(off, len, label)| {
            let c = slot_label_colors.get(label).copied().unwrap_or(Color::White);
            (off + slot.byte_offset, len, c)
        })
        .collect();
    let hex_rows = hex_dump_rows(slot_bytes, display_bytes, &adjusted_highlights, slot.byte_offset);
    for hr in &hex_rows {
        right_items.push(padding_hex_right(hr.clone()));
    }

    // Slot field table (inner box)
    let dashes = PR - 12;
    let tw = dashes - 2;

    right_items.push(vec![Span::raw(format!("  +{}+", "-".repeat(dashes)))]);
    right_items.push(vec![Span::raw(format!(
        "  | {:<tw$} |",
        format!("GC Generation Stats Slot #{} (gen {}, slot {}) @ {:#x}",
            selected_slot + 1, slot.generation, slot.slot,
            gc.stats_addr + slot.byte_offset as u64),
        tw = tw
    ))]);
    right_items.push(vec![Span::raw(format!("  +{}+", "-".repeat(dashes)))]);

    for (i, name) in slot_field_names.iter().enumerate() {
        let offset = i * 8;
        if offset + 8 > slot_bytes.len() {
            break;
        }
        let val = u64::from_le_bytes(slot_bytes[offset..offset + 8].try_into().unwrap());
        let val_fmt = if *name == "duration" {
            let d = f64::from_le_bytes(slot_bytes[offset..offset + 8].try_into().unwrap());
            format!("{:.6}", d)
        } else if *name == "ts_start" || *name == "ts_stop" {
            fmt_thousands(val)
        } else if val > 0xFFFF_FFFF {
            format!("{:#x}", val)
        } else {
            format!("{}", val)
        };

        let content = format!("  {:<15} @ +{:<4}  {}", name, offset, val_fmt);
        let name_str: &str = name;
        let color = slot_label_colors.get(name_str).copied();

        if let Some(c) = color {
            let prefix_span = Span::raw("  | ".to_string());
            let content_span = Span::styled(
                format!("{:<tw$}", content, tw = tw),
                Style::new().bg(c).fg(Color::Black),
            );
            let suffix_span = Span::raw(" |".to_string());
            right_items.push(vec![prefix_span, content_span, suffix_span]);
        } else {
            right_items.push(vec![Span::raw(format!(
                "  | {:<tw$} |",
                content, tw = tw
            ))]);
        }
    }
    right_items.push(vec![Span::raw(format!("  +{}+", "-".repeat(dashes)))]);

    // ── Combine ──
    let selected_left_idx = 7 + selected_slot; // left items 0-6 are headers
    let max_rows = left.len().max(right_items.len());
    for i in 0..max_rows {
        let lv = left.get(i).map(|s| s.as_str()).unwrap_or("");
        let right = right_items
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![Span::raw(" ".repeat(PR))]);

        if i == selected_left_idx && i < left.len() {
            let left_span = Span::styled(
                format!("{:<pl$}", lv, pl = PL),
                Style::new().bg(Color::DarkGray).fg(Color::White),
            );
            lines.push(span_line(vec![left_span], right));
        } else {
            lines.push(full_line(lv, right));
        }
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

// ── Format helpers ────────────────────────────────────────────────
fn fmt_val(val: u64) -> String {
    if val > 0xFFFF_FFFF {
        format!("{:#x}", val)
    } else if val > 0x10000 {
        format!("{}", val)
    } else {
        format!("{}", val)
    }
}

fn fmt_thousands(val: u64) -> String {
    let s = val.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.char_indices() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('_');
        }
        out.push(c);
    }
    out
}

fn fmt_bytes(val: u64) -> String {
    if val >= 1000 * 1000 {
        format!("{:.1}M", val as f64 / (1000.0 * 1000.0))
    } else if val >= 1000 {
        format!("{:.1}K", val as f64 / 1000.0)
    } else {
        format!("{}", val)
    }
}

fn fmt_duration(dur: f64) -> String {
    if dur < 1.0 {
        format!("{:.3}ms", dur * 1000.0)
    } else {
        format!("{:.3}s", dur)
    }
}

fn fmt_duration_ns(d: Duration) -> String {
    let ns = d.as_nanos() as f64;
    if ns >= 1_000_000.0 {
        format!("{:.3}ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1}\u{00b5}s", ns / 1_000.0)
    } else {
        format!("{:.0}ns", ns)
    }
}

fn fmt_rate(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{:.1}/s", rate)
    } else if rate >= 0.01 {
        format!("{:.2}/s", rate)
    } else {
        "0.0/s".to_string()
    }
}

// ── Glitch effects ─────────────────────────────────────────────────
fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn rand_range(rng: &mut u32, min: u32, max: u32) -> u32 {
    if min >= max {
        return min;
    }
    min + xorshift32(rng) % (max - min + 1)
}

fn apply_glitch(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32) {
    let count = if rand_range(rng, 0, 1) == 0 { 1 } else { 2 };
    for _ in 0..count {
        apply_one_glitch(buffer, rng);
    }
}

fn apply_one_glitch(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32) {
    let w = buffer.area.width as usize;
    let h = buffer.area.height as usize;
    if w < 4 || h < 2 {
        return;
    }

    match rand_range(rng, 0, 2) {
        0 => {
            // Screen tear: shift a block of rows rightwards
            let tear_h = rand_range(rng, 2, 6.min(h as u32)) as usize;
            let tear_y = rand_range(rng, 0, (h - tear_h) as u32) as usize;
            let shift = rand_range(rng, 3, 10.min(w as u32)) as usize;
            for dy in 0..tear_h {
                let y = tear_y + dy;
                let row_base = y * w;
                let saved: Vec<(String, Color, Color)> = (shift..w)
                    .map(|x| {
                        let c = &buffer.content[row_base + x - shift];
                        (c.symbol().to_string(), c.fg, c.bg)
                    })
                    .collect();
                for (i, (sym, fg, bg)) in saved.into_iter().enumerate() {
                    let idx = row_base + shift + i;
                    buffer.content[idx].set_symbol(&sym);
                    buffer.content[idx].fg = fg;
                    buffer.content[idx].bg = bg;
                }
                for x in 0..shift {
                    let idx = row_base + x;
                    buffer.content[idx].set_char(rand_range(rng, 33, 126) as u8 as char);
                }
            }
        }
        1 => {
            // Block corruption: replace symbols with random ASCII
            let bw = rand_range(rng, 5, 30.min(w as u32)) as usize;
            let bh = rand_range(rng, 2, 6.min(h as u32)) as usize;
            let bx = rand_range(rng, 0, (w - bw) as u32) as usize;
            let by = rand_range(rng, 0, (h - bh) as u32) as usize;
            for y in by..by + bh {
                let row_base = y * w;
                for x in bx..bx + bw {
                    let idx = row_base + x;
                    buffer.content[idx].set_char(rand_range(rng, 33, 126) as u8 as char);
                }
            }
        }
        2 => {
            // Color invert: swap fg and bg in a rectangle
            let bw = rand_range(rng, 8, 40.min(w as u32)) as usize;
            let bh = rand_range(rng, 2, 6.min(h as u32)) as usize;
            let bx = rand_range(rng, 0, (w - bw) as u32) as usize;
            let by = rand_range(rng, 0, (h - bh) as u32) as usize;
            for y in by..by + bh {
                let row_base = y * w;
                for x in bx..bx + bw {
                    let idx = row_base + x;
                    let cell = &mut buffer.content[idx];
                    let tmp = cell.fg;
                    cell.fg = cell.bg;
                    cell.bg = tmp;
                }
            }
        }
        _ => {}
    }
}

// ── Connection-lost sequence ───────────────────────────────────────
fn apply_connection_lost_buildup(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32, progress: f64) {
    let w = buffer.area.width as usize;
    let h = buffer.area.height as usize;
    if w < 4 || h < 2 {
        return;
    }

    // Ramp from 2 effects/frame to 15+
    let count = 2 + (progress * 18.0) as u32;
    for _ in 0..count {
        apply_one_glitch(buffer, rng);
    }

    // At 85% progress corrupt the bottom half of the screen heavily
    if progress >= 0.85 {
        let bh = h / 2;
        if bh > 0 {
            let bw = (w as f64 * 0.6) as usize;
            let bx = rand_range(rng, 0, (w - bw) as u32) as usize;
            for dy in 0..bh {
                let y = (h - bh + dy).max(0).min(h - 1);
                let row_base = y * w;
                for x in bx..bx + bw {
                    buffer.content[row_base + x]
                        .set_char(rand_range(rng, 33, 126) as u8 as char);
                }
            }
        }
    }
}

fn draw_connection_lost_box(buffer: &mut ratatui::buffer::Buffer, jx: i32, jy: i32) {
    let w = buffer.area.width as usize;
    let h = buffer.area.height as usize;
    if w < 22 || h < 7 {
        return;
    }

    let box_w: usize = 22; // "+-- CONNECTION LOST --+"
    let box_h: usize = 5;

    let bx = ((w - box_w) / 2) as i32 + jx;
    let by = ((h - box_h) / 2) as i32 + jy;

    let rows = [
        format!("+{}+", "-".repeat(box_w - 2)),
        format!("|{:^width$}|", "", width = box_w - 2),
        format!("|{:^width$}|", "CONNECTION LOST", width = box_w - 2),
        format!("|{:^width$}|", "", width = box_w - 2),
        format!("+{}+", "-".repeat(box_w - 2)),
    ];

    for (dy, row_str) in rows.iter().enumerate() {
        let row_y = (by + dy as i32).max(0).min(h as i32 - 1) as usize;
        for (dx, ch) in row_str.chars().enumerate() {
            let col_x = (bx + dx as i32).max(0).min(w as i32 - 1) as usize;
            let idx = row_y * w + col_x;
            buffer.content[idx].fg = Color::Black;
            buffer.content[idx].bg = Color::Red;
            if dy == 2 {
                buffer.content[idx].modifier = ratatui::style::Modifier::BOLD;
            }
            buffer.content[idx].set_char(ch);
        }
    }
}
