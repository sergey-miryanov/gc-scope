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

use crate::remote_debugging::gc_stats::GcStat;
use crate::snapshot::collect::{
    avg_collection_time_per_gen, collections_rate_from_slots, CollectRequest, CollectedData, GcSlot,
};
use crate::snapshot::poller::SnapshotPoller;
use super::tree::{debug_offsets_tree, gen_stats_layout, tree_prefixes};
use crate::remote_debugging::offsets::VersionedOffsets;

// ── Layout constants ──────────────────────────────────────────────
const OUTER_W: usize = 158;
const PL: usize = 65;
const PR: usize = 90;
const INNER_W: usize = PL - 4;      // 61
const INNER_TW: usize = INNER_W - 2; // 59

// The GC-stats buffer view splits the same 160-col frame differently from the full view: a
// 16-byte hexdump row is exactly 78 cols wide, so the right column is pinned to that and the
// left gets the rest (`| L | R |` = 1+77+3+78+1 = 160). The left is wider than the full view's
// `PL` so a `+inc` build's long field names (`ts_handle_weakref_callbacks_start`, …) fit.
const GC_PR: usize = 78;
const GC_PL: usize = OUTER_W + 2 - GC_PR - 5; // 77 (frame 160 minus the `| … | … |` framing)


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
pub fn run_tui(pid: Option<u32>, rate_ms: u64, duration_secs: Option<u64>, glitch_enabled: bool) -> Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    stdout().execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // PID selection dialog if no PID given
    let initial_pid = match pid {
        Some(p) => p,
        None => {
            let (processes, pid_info_map) = crate::list_pids::list_python_processes()?;
            match super::pid_dialog::show_pid_dialog(&mut terminal, &processes, &pid_info_map)? {
                Some(p) => p,
                None => return Ok(()), // user cancelled the picker — exit cleanly
            }
        }
    };

    let mut poller = SnapshotPoller::attach_with(initial_pid, CollectRequest::tui())?;
    let mut start = Instant::now();
    let mut frame: u64 = 0;

    // View state (scroll, slot selection, panel toggles, rate, glitch-enable) — every
    // mutation goes through `TuiState::handle_key`, so the key bindings are unit-testable.
    let mut state = TuiState::new(rate_ms, glitch_enabled);

    // Glitch / connection-lost timer state, advanced once per frame by `GlitchState::tick`.
    // `rng_state` stays beside it (not inside) so the render closure can borrow it while the
    // timer struct is left untouched.
    let mut rng_state: u32 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(12345);
    let mut glitch = GlitchState::new(Instant::now());

    // `s` requests a frame dump, but the current `data` isn't polled until below, so the
    // key sets a flag the loop acts on once the frame exists. `dump_note` carries the
    // result into the header title (writing to stderr would corrupt the alt-screen).
    let mut pending_dump = false;
    let mut dump_note: Option<String> = None;

    let result = loop {
        if event::poll(Duration::from_millis(state.rate_ms))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match state.handle_key(key.code) {
                KeyOutcome::Quit => break Ok(()),
                KeyOutcome::DumpSnapshot => pending_dump = true,
                KeyOutcome::PickPid => {
                    if let Ok((processes, pid_info_map)) = crate::list_pids::list_python_processes()
                        && let Ok(Some(new_pid)) = super::pid_dialog::show_pid_dialog(&mut terminal, &processes, &pid_info_map)
                        && poller.retarget(new_pid).is_ok()
                    {
                        // `retarget` swaps the session in only on a successful attach, so a
                        // failed re-pick leaves the old session live; commit the view reset
                        // only once it fully resolves.
                        start = Instant::now();
                        frame = 0;
                        state.reset_view();
                    }
                }
                KeyOutcome::Continue => {}
            }
        }

        let data = match poller.poll() {
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
        state.clamp_slot(slot_count);

        // Act on a pending `s` dump now that this frame's `data` exists. Overwrites
        // `<pid>.txt` in the working directory with the current view.
        if pending_dump {
            pending_dump = false;
            let path = format!("{}.txt", poller.pid());
            let frame = render_snapshot(&data, state.selected_slot, state.debug_offsets_show_tree, state.debug_offsets_show_hex, state.show_runtime_hex, state.gc_only);
            dump_note = Some(match std::fs::write(&path, frame) {
                Ok(()) => format!(" — Saved {path}"),
                Err(e) => format!(" — Save failed: {e}"),
            });
        }

        let elapsed = start.elapsed();
        frame += 1;

        // Auto-exit if duration exceeded
        if let Some(max_dur) = duration_secs
            && elapsed.as_secs() >= max_dur
        {
            break Ok(());
        }

        // Advance the glitch / connection-lost state machine. `now` is captured once and
        // passed in (rather than read inside) so the transitions are deterministically
        // testable; see `GlitchState::tick`.
        let now = Instant::now();
        glitch.tick(now, state.glitch_enabled, &mut rng_state);
        glitch.update_jitter(now, &mut rng_state);

        let stats = &data.interpreter.gc.generation_stats;
        let slots = &stats.slots;
        let (rate_per_gen, avg_coll_time_per_gen) = (
            collections_rate_from_slots(slots, stats.has_timestamps),
            avg_collection_time_per_gen(slots, stats.has_duration),
        );
        let styled_lines = if state.gc_only {
            build_gc_only_lines(&data, rate_per_gen, avg_coll_time_per_gen, state.selected_slot)
        } else {
            build_lines(&data, rate_per_gen, avg_coll_time_per_gen, state.selected_slot, state.debug_offsets_show_tree, state.debug_offsets_show_hex, state.show_runtime_hex).0
        };

        // Pre-compute glitch draw conditions for this frame.
        let should_glitch = glitch.should_glitch(state.glitch_enabled);
        let should_buildup = glitch.should_buildup(state.glitch_enabled);
        let should_msg = glitch.should_msg(state.glitch_enabled);
        let buildup_progress = if should_buildup {
            glitch.cl_phase_start.elapsed().as_secs_f64().min(1.0)
        } else {
            0.0
        };
        let glitch_badge_active = glitch.badge_active();
        let (cl_active, cl_jx, cl_jy) = (glitch.cl_active, glitch.cl_jx, glitch.cl_jy);

        // Copy the view scalars the render closure reads; `scroll` is taken mutably so the
        // closure can clamp it, then written back below. `pid` is re-read from the poller
        // each frame so a mid-loop pick-pid retarget is reflected in the title.
        let pid = poller.pid();
        let rate_ms = state.rate_ms;
        let selected_slot = state.selected_slot;
        let glitch_enabled = state.glitch_enabled;
        let dump_note_str = dump_note.as_deref().unwrap_or("");
        let mut scroll = state.scroll;
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
                " gcscope tui — PID {} — Frame {} @ {:.1}s — Rate {}ms — Poll {}{}{} ",
                pid,
                frame,
                elapsed.as_secs_f64(),
                rate_ms,
                fmt_duration_ns(data.collect_duration),
                duration_secs.map_or(String::new(), |d| format!(" — Dur {d}s")),
                dump_note_str
            );
            let content = Paragraph::new(Text::from(styled_lines))
                .block(Block::bordered().border_type(BorderType::Plain).title(title))
                .scroll((scroll, 0));

            let status = status_bar(scroll, max_scroll, selected_slot, slot_count, glitch_badge_active, cl_active, glitch_enabled);
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
        state.scroll = scroll;
    };

    // Terminal teardown is handled by `_guard` on drop, covering every exit path.
    result
}

// ── View state (key reducer) ──────────────────────────────────────
#[derive(Debug, Clone, PartialEq, Eq)]
struct TuiState {
    scroll: u16,
    selected_slot: usize,
    debug_offsets_show_tree: bool,
    debug_offsets_show_hex: bool,
    show_runtime_hex: bool,
    /// `true` = the GC-stats-only buffer view; `false` = the full layout. Toggled by `g`.
    gc_only: bool,
    rate_ms: u64,
    glitch_enabled: bool,
}

/// What a key press asks the render loop to do. `PickPid`, `Quit`, and `DumpSnapshot` are
/// the outcomes that need terminal/session/file I/O, which the loop owns — the reducer
/// stays pure.
#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    Continue,
    Quit,
    PickPid,
    DumpSnapshot,
}

impl TuiState {
    fn new(rate_ms: u64, glitch_enabled: bool) -> Self {
        TuiState {
            scroll: 0,
            selected_slot: 0,
            debug_offsets_show_tree: true,
            debug_offsets_show_hex: true,
            show_runtime_hex: false,
            gc_only: false,
            rate_ms,
            glitch_enabled,
        }
    }

    /// Resets the per-view state after a successful PID re-pick. Mirrors the original inline
    /// reset, which deliberately left `rate_ms` and `glitch_enabled` alone.
    fn reset_view(&mut self) {
        self.scroll = 0;
        self.selected_slot = 0;
        self.debug_offsets_show_tree = true;
        self.debug_offsets_show_hex = true;
        self.show_runtime_hex = false;
        self.gc_only = false;
    }

    /// Pulls `selected_slot` back into `[0, slot_count)` when a new snapshot has fewer slots.
    fn clamp_slot(&mut self, slot_count: usize) {
        let max_slot = slot_count.saturating_sub(1);
        if self.selected_slot > max_slot {
            self.selected_slot = max_slot;
        }
    }

    /// Applies one key press to the view state, returning whether the loop should continue,
    /// quit, or open the PID picker. Pure aside from `&mut self`, so every binding is
    /// directly unit-testable without a terminal.
    fn handle_key(&mut self, code: KeyCode) -> KeyOutcome {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.selected_slot = self.selected_slot.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected_slot = self.selected_slot.saturating_add(1),
            KeyCode::Char('t') => self.debug_offsets_show_tree = !self.debug_offsets_show_tree,
            KeyCode::Char('h') => self.debug_offsets_show_hex = !self.debug_offsets_show_hex,
            KeyCode::Char('o') => {
                if self.debug_offsets_show_tree || self.debug_offsets_show_hex {
                    self.debug_offsets_show_tree = false;
                    self.debug_offsets_show_hex = false;
                } else {
                    self.debug_offsets_show_tree = true;
                    self.debug_offsets_show_hex = true;
                }
            }
            KeyCode::Char('d') => self.show_runtime_hex = !self.show_runtime_hex,
            KeyCode::Char('r') => self.rate_ms = self.rate_ms.saturating_sub(10).max(10),
            KeyCode::Char('R') => self.rate_ms = self.rate_ms.saturating_add(10),
            KeyCode::Char('g') => self.gc_only = !self.gc_only,
            KeyCode::Char('G') => self.glitch_enabled = !self.glitch_enabled,
            KeyCode::Char('s') => return KeyOutcome::DumpSnapshot,
            KeyCode::Char('p') => return KeyOutcome::PickPid,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            _ => {}
        }
        KeyOutcome::Continue
    }
}

// ── Glitch / connection-lost timer ────────────────────────────────
/// The glitch and "connection lost" visual-effect state machine, split out of the render
/// loop so its transitions can be tested against an injected clock. `tick`/`update_jitter`
/// take `now` as a parameter instead of calling `Instant::now()` internally.
struct GlitchState {
    glitch_active: bool,
    next_glitch_at: Instant,
    glitch_end: Instant,
    cl_active: bool,
    cl_phase: u8, // 0=inactive, 1=build-up, 2=message
    cl_phase_start: Instant,
    cl_end: Instant,
    next_cl_show: Instant,
    cl_jx: i32,
    cl_jy: i32,
    cl_last_jitter: Instant,
}

impl GlitchState {
    fn new(now: Instant) -> Self {
        GlitchState {
            glitch_active: false,
            next_glitch_at: now,
            glitch_end: now,
            cl_active: false,
            cl_phase: 0,
            cl_phase_start: now,
            cl_end: now,
            next_cl_show: now + Duration::from_secs(30),
            cl_jx: 0,
            cl_jy: 0,
            cl_last_jitter: now,
        }
    }

    /// Advances the glitch + connection-lost timers by one frame. No-op while `glitch_enabled`
    /// is false, matching the original inline guard.
    fn tick(&mut self, now: Instant, glitch_enabled: bool, rng: &mut u32) {
        if !glitch_enabled {
            return;
        }
        if self.cl_active {
            if self.cl_phase == 1 {
                // Build-up phase lasts 1 second, then the message shows.
                if now >= self.cl_phase_start + Duration::from_secs(1) {
                    self.cl_phase = 2;
                    self.cl_phase_start = now;
                    let msg_dur = rand_range(rng, 4000, 8000);
                    self.cl_end = now + Duration::from_millis(msg_dur as u64);
                }
            } else if self.cl_phase == 2 && now >= self.cl_end {
                self.cl_active = false;
                self.cl_phase = 0;
                // Double the next normal glitch cooldown, and reschedule the next sequence.
                let delay = rand_range(rng, 1000, 8000) * 2;
                self.next_glitch_at = now + Duration::from_millis(delay as u64);
                let interval = rand_range(rng, 25000, 35000);
                self.next_cl_show = now + Duration::from_millis(interval as u64);
            }
        } else if now >= self.next_cl_show {
            self.cl_active = true;
            self.cl_phase = 1;
            self.cl_phase_start = now;
        } else if self.glitch_active {
            if now >= self.glitch_end {
                self.glitch_active = false;
                let delay = rand_range(rng, 1000, 8000);
                self.next_glitch_at = now + Duration::from_millis(delay as u64);
            }
        } else if now >= self.next_glitch_at {
            self.glitch_active = true;
            let dur = rand_range(rng, 200, 600);
            self.glitch_end = now + Duration::from_millis(dur as u64);
        }
    }

    /// Updates the connection-lost box jitter, throttled to ~5 Hz and only during the
    /// message phase.
    fn update_jitter(&mut self, now: Instant, rng: &mut u32) {
        if self.cl_active
            && self.cl_phase == 2
            && now >= self.cl_last_jitter + Duration::from_millis(200)
        {
            self.cl_jx = ((rand_range(rng, 0, 2) as i32) - 1).clamp(-1, 1);
            self.cl_jy = ((rand_range(rng, 0, 2) as i32) - 1).clamp(-1, 1);
            self.cl_last_jitter = now;
        }
    }

    fn should_glitch(&self, enabled: bool) -> bool {
        enabled && !self.cl_active && self.glitch_active
    }
    fn should_buildup(&self, enabled: bool) -> bool {
        enabled && self.cl_active && self.cl_phase == 1
    }
    fn should_msg(&self, enabled: bool) -> bool {
        enabled && self.cl_active && self.cl_phase == 2
    }
    fn badge_active(&self) -> bool {
        self.glitch_active || self.cl_active
    }
}

// Seven heterogeneous scalars, all read off the render loop's local state at the single
// call site below. Poll rate and poll time live in the header title instead, so they don't
// appear here twice.
fn status_bar(scroll: u16, max_scroll: u16, slot: usize, slot_count: usize, glitch_active: bool, cl_active: bool, glitch_enabled: bool) -> Paragraph<'static> {
    let style = Style::new().bg(Color::Blue).fg(Color::White);
    // u32 math on purpose: `scroll` is a u16 and `scroll * 100` overflows it once the
    // scrollback passes 655 rows — a debug-build panic in a view that can easily be
    // longer than that. `checked_div` covers the max_scroll == 0 (nothing to scroll) case.
    let scroll_pct = match (scroll as u32 * 100).checked_div(max_scroll as u32) {
        Some(pct) => format!(" {pct:>3}%"),
        None => " 100%".to_string(),
    };
    let slot_text = if slot_count > 0 {
        format!(" slot {}/{} ", slot + 1, slot_count)
    } else {
        " no slots ".to_string()
    };
    let badge = if cl_active {
        Span::styled(" [CL] ", style.bg(Color::Red).fg(Color::White).add_modifier(ratatui::style::Modifier::BOLD))
    } else if glitch_active {
        Span::styled(" [FX] ", style.bg(Color::Red).fg(Color::White).add_modifier(ratatui::style::Modifier::BOLD))
    } else {
        Span::raw("")
    };
    let glitch_label = if glitch_enabled { "on" } else { "off" };
    let glitch_style = if glitch_enabled { style } else { style.bg(Color::DarkGray) };
    let text = Line::from(vec![
        Span::styled(" [q] quit ", style.bg(Color::DarkGray)),
        Span::styled(" [s] save ", style),
        Span::styled(" [p] pick pid ", style),
        Span::styled(" [g] gc-view ", style),
        Span::styled(" [t] tree [h] hex [o] collapse [d] Dbg/Rt", style),
        Span::styled(" [r/R] rate", style),
        Span::styled(format!(" [G] glitch:{}", glitch_label), glitch_style),
        badge,
        Span::styled(" [\u{2191}\u{2193}/jk] ", style),
        Span::styled(slot_text, style.bg(Color::DarkGray)),
        Span::styled(format!(" [PgUp/PgDn] scroll{} ", scroll_pct), style),
    ]);
    Paragraph::new(text)
}

// ── Main line builder ─────────────────────────────────────────────
fn build_lines(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3], selected_slot: usize, debug_offsets_show_tree: bool, debug_offsets_show_hex: bool, show_runtime_hex: bool) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    // Sections 1–2 render the `_Py_DebugOffsets` struct — 3.13+ only. Pre-3.13 skips
    // section 1 entirely and shows a focused interpreter header (section 2), then the
    // GC generation-stats table.
    let (s1, s2) = match data.offsets() {
        Some(off) => (
            section_debug_offsets(data, off, debug_offsets_show_tree, debug_offsets_show_hex, show_runtime_hex),
            section_interpreter(data, off),
        ),
        None => (Vec::new(), section_interpreter_legacy(data)),
    };
    let s1_len = s1.len();
    let s2_len = s2.len();
    lines.extend(s1);
    // Blank separator after section 1 only when it was rendered (3.13+).
    let sep1 = if s1_len > 0 { lines.push(Line::from("")); 1 } else { 0 };
    lines.extend(s2);
    lines.push(Line::from(""));
    lines.extend(section_gc_stats(data, rate_per_gen, avg_coll_time_per_gen, selected_slot));
    // Slot row in section_gc_stats starts at index 3 (top/buffer/top) + 7 header lines in the interleave
    let slot_line_idx = s1_len + sep1 + s2_len + 1 + 3 + 7 + selected_slot;
    (lines, slot_line_idx)
}

// ── Box helpers ───────────────────────────────────────────────────
fn top() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}

// `| ` + content + ` |` must total `top()`'s width (OUTER_W + 2), so the padded content
// area is OUTER_W - 2 — not OUTER_W, which overpads full-width rows by 2 and pushes them
// past the box border (harmless clipping in a live terminal, visible misalignment in a
// `tui --output` file dump).
fn l(content: &str) -> String {
    format!("| {:<w$} |", content, w = OUTER_W - 2)
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

/// Two-column body row for the GC-stats buffer view: `|<left> | <right>|`, padding each
/// column to `GC_PL`/`GC_PR` so the frame borders line up. Like [`span_line`] but with the
/// buffer view's wider-left split instead of the full view's `PL`/`PR`.
fn gc_two_col(left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw("|")];
    let lw: usize = left.iter().map(|s| s.content.len()).sum();
    spans.extend(left);
    if lw < GC_PL {
        spans.push(Span::raw(" ".repeat(GC_PL - lw)));
    }
    spans.push(Span::raw(" | "));
    let rw: usize = right.iter().map(|s| s.content.len()).sum();
    spans.extend(right);
    if rw < GC_PR {
        spans.push(Span::raw(" ".repeat(GC_PR - rw)));
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
            let next_in_same = hl.is_some_and(|&(off, len, _)| {
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
fn section_debug_offsets(data: &CollectedData, off: &VersionedOffsets, show_tree: bool, show_hex: bool, show_runtime_hex: bool) -> Vec<Line<'static>> {
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
    let gen_stats_size = off.gc_generation_stats_size();
    let gs = gen_stats_layout(gen_stats_size);

    // Drive the GC-state subtree from actual, version-correct layout: the `gc`
    // sub-struct fields and the resolved per-slot field layout (which reflects the
    // clean-vs-`+inc` selection).
    let gc_fields = off.gc_debug_fields();
    let offset_table = off.to_offset_table(data.pid, data.runtime_addr);
    let slot_fields = offset_table.gc_layout.map(|l| l.fields);
    let tree = debug_offsets_tree(&gc_fields, slot_fields);
    let prefixes = tree_prefixes(&tree);

    let debug_highlights = if !show_runtime_hex {
        off.debug_offsets_highlight_regions()
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

    let mut tree_highlight_rows: Vec<(usize, Color)> = Vec::new();

    let mut left_owned: Vec<String> = Vec::new();
    if show_tree {
        left_owned.push(format!("{:<pl$}", "Fields:", pl = PL));
        for (i, entry) in tree.iter().enumerate() {
            let pfx = &prefixes[i];
            let line = match entry.kind {
                super::tree::TreeEntryKind::RawValue { offset } => {
                    let val = read_u64(offset);
                    let f = fmt_val(val, entry.label);
                    format_tree_line(pfx, &format!("0x{:04x}  ", offset), entry.label, &f)
                }
                super::tree::TreeEntryKind::Group => {
                    format_tree_line(pfx, "", entry.label, "")
                }
                super::tree::TreeEntryKind::Derived => {
                    let val_str = derived_val(entry.label);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
                super::tree::TreeEntryKind::Layout { field_type: _, field_offset } => {
                    let val_str = format!("+{}", field_offset);
                    format_tree_line(pfx, "", entry.label, &val_str)
                }
            };
            left_owned.push(line);
            if let Some(color) = debug_tree_row_color(entry.label, entry.kind) {
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
// Pre-3.13 focused interpreter header: no `_Py_DebugOffsets`, so no field table or
// GC-state box — just the interpreter address/id and a note; GC stats follow below.
fn section_interpreter_legacy(data: &CollectedData) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let interp = &data.interpreter;
    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(&format!(
        "PyInterpreterState @ {:#x}  (id: {})",
        interp.addr, interp.id
    )))));
    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(
        "pre-3.13: no _Py_DebugOffsets struct — showing GC generation stats only",
    ))));
    lines.push(Line::from(Span::raw(top())));
    lines
}

fn section_interpreter(data: &CollectedData, off: &VersionedOffsets) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let interp = &data.interpreter;
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
            let lv = match left_items.first() {
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
fn section_gc_stats(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3], selected_slot: usize) -> Vec<Line<'static>> {
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

    // Version-correct geometry/layout for this build (IO-free): drives the per-slot size,
    // the per-generation slot counts, and the slot-items field list below. Sourced from
    // the flat table the session already built, so it works for every tier (incl. Legacy).
    let offset_table = data.resolved.table().clone();
    let item_size = if gc.item_size > 0 { gc.item_size } else { gc.raw_stats_bytes.len().min(64) };
    // Per-generation slot counts come from the collected snapshot (version/layout-derived,
    // FT-correct) rather than a GIL-assuming literal or a per-frame tally.
    let slots_per_gen = gc.slots_per_gen;
    // Version-correct per-slot field layout (name → offset): drives both the hex-dump
    // highlights and the slot-items table. Using it (not the fixed ring layout) keeps the
    // 3.13/3.14 inline slots — collections@0/collected@8 — highlighted at the right bytes.
    let slot_fields: &[(&str, usize)] = offset_table.gc_layout.map(|l| l.fields).unwrap_or(&[]);

    // ── Left panel ──
    let mut left: Vec<String> = Vec::new();
    for line in gen_summary_lines(slots_per_gen, rate_per_gen, avg_coll_time_per_gen) {
        left.push(format!("{:<pl$}", line, pl = PL));
    }
    left.push(format!(
        "{:<pl$}",
        format!("slot size: {} bytes  |  total buffer: {} bytes", item_size, gc.stats_size),
        pl = PL
    ));
    left.push(format!("{:<pl$}", "", pl = PL));
    let hdr = slot_table_header();
    let hdr_len = hdr.len();
    left.push(format!("{:<pl$}", hdr, pl = PL));
    left.push(format!("  {}", "-".repeat(hdr_len - 2)));

    for slot in &gc.slots {
        left.push(slot_table_row(slot));
    }

    // ── Right panel ──
    let slot = &gc.slots[selected_slot];
    let slot_bytes = selected_slot_bytes(&gc.raw_stats_bytes, slot.byte_offset, item_size);
    let display_bytes = slot_bytes.len();
    // Decode this slot's fields through the shared `GcStat` primitive — the same by-name/offset
    // path the Chrome exporter uses — instead of re-reading the raw bytes inline here.
    let slot_view = offset_table
        .gc_layout
        .map(|l| GcStat::from_slot(slot_bytes, l, slot.generation, slot.slot, 0));

    let mut right_items: Vec<Vec<Span<'static>>> = Vec::new();
    // Header
    right_items.push(vec![Span::raw(format!(
        "{:<pr$}",
        format!("Slot #{} (gen {}, slot {}) of stats buffer:", selected_slot + 1, slot.generation, slot.slot),
        pr = PR
    ))]);

    // Hex dump of selected slot bytes, highlighting each present, colored field at its real
    // per-version offset. Deriving from the actual layout keeps the 3.13/3.14 inline slots
    // (collections@0, collected@8) from being painted at the ring offsets (16/24).
    let adjusted_highlights = field_highlights(&slot_view, slot.byte_offset);
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

    // Width the name column to the widest field this build actually has, so the `@ +offset`
    // and value columns stay aligned even for the long `+inc` names (e.g.
    // `ts_handle_weakref_callbacks_start`). Floored at 15 so short-field builds are unchanged.
    let name_w = slot_fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(15);

    for (name, offset, valbits) in slot_view.iter().flat_map(|v| v.iter_fields()) {
        let val_fmt = format_field_value(name, valbits);
        let content = format!("  {:<name_w$} @ +{:<4}  {}", name, offset, val_fmt, name_w = name_w);
        let color = slot_field_color(name);

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

/// The color a GC-stat field is painted in, in both the hexdump highlights and the field
/// table. `None` = left unhighlighted (uncollectable, candidates, the `+inc` extras). Shared
/// by the full view (`section_gc_stats`) and the buffer view (`build_gc_only_lines`) so the
/// two never drift.
fn slot_field_color(name: &str) -> Option<Color> {
    match name {
        "ts_start" | "ts_stop" => Some(Color::Blue),
        "collections" => Some(Color::Green),
        "collected" => Some(Color::Magenta),
        "duration" => Some(Color::Yellow),
        "heap_size" => Some(Color::Cyan),
        _ => None,
    }
}

/// Highlight color for one `_Py_DebugOffsets` tree row, or `None` to leave it unshaded. The
/// named runtime fields match their hexdump-region colors (see the `hex_highlights` mapping);
/// every first-slot field item (`Layout` kind — the per-slot `gc_generation_stats` fields on
/// ring builds) shares one color so the slot's layout reads as a single group. That group is
/// tree-only: those items have no hexdump region of their own.
fn debug_tree_row_color(label: &str, kind: super::tree::TreeEntryKind) -> Option<Color> {
    use super::tree::TreeEntryKind;
    match label {
        "cookie[8]" => Some(Color::Green),
        "interpreters_head" => Some(Color::Cyan),
        "next" => Some(Color::Yellow),
        "gc" => Some(Color::Magenta),
        _ => matches!(kind, TreeEntryKind::Layout { .. }).then_some(Color::Blue),
    }
}

// ── Shared GC-stats rendering helpers ─────────────────────────────
// The full view (`section_gc_stats`) and the buffer view (`build_gc_only_lines`) lay their
// panels out differently but decode and format the same bytes; these keep that shared logic in
// one place so the two can't drift.

/// The per-generation summary lines — slot count, collections rate, avg collection duration —
/// with `n/a` where the layout lacks the field. Unpadded; each view pads/wraps to its own width.
fn gen_summary_lines(
    slots_per_gen: [u64; 3],
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
) -> [String; 3] {
    const LABELS: [&str; 3] = ["Gen 0 (Young)", "Gen 1 (Middle)", "Gen 2 (Oldest)"];
    std::array::from_fn(|g| {
        let rate = match rate_per_gen[g] { Some(r) => fmt_rate(r), None => "n/a".to_string() };
        let coll = match avg_coll_time_per_gen[g] { Some(d) => fmt_duration(d), None => "n/a".to_string() };
        format!("{} - {} slots  (rate = {}, avg coll = {})", LABELS[g], slots_per_gen[g], rate, coll)
    })
}

/// The slot-table column header shared by both views' left tables.
fn slot_table_header() -> String {
    format!(
        "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11}",
        "gen", "slot", "collections", "collected", "heap", "duration(s)"
    )
}

/// One row of the slot table — same columns as [`slot_table_header`].
fn slot_table_row(slot: &GcSlot) -> String {
    format!(
        "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11.3}",
        slot.generation, slot.slot, slot.collections, slot.collected,
        fmt_bytes(slot.heap_size as u64), slot.duration
    )
}

/// One slot's window into the raw stats buffer, clamped so a short/absent buffer yields an
/// empty slice instead of an out-of-range panic (`byte_offset + item_size` can exceed the
/// collected bytes when a request skipped the raw payload).
fn selected_slot_bytes(raw: &[u8], byte_offset: usize, item_size: usize) -> &[u8] {
    let start = byte_offset.min(raw.len());
    let end = (start + item_size).min(raw.len());
    &raw[start..end]
}

/// Format one decoded field value for display: `duration` as seconds, `ts_*` grouped by
/// thousands, values above `u32::MAX` as hex, everything else decimal.
fn format_field_value(name: &str, valbits: u64) -> String {
    if name == "duration" {
        format!("{:.6}", f64::from_bits(valbits))
    } else if name.starts_with("ts_") {
        fmt_thousands(valbits)
    } else if valbits > 0xFFFF_FFFF {
        format!("{:#x}", valbits)
    } else {
        format!("{}", valbits)
    }
}

/// Hex-dump highlights for a decoded slot's colored fields, each 8 bytes at its real
/// per-version offset (shifted by the slot's `byte_offset` into the buffer). Fields without a
/// color (uncollectable, candidates, the `+inc` extras) are left unhighlighted.
fn field_highlights(slot_view: &Option<GcStat>, byte_offset: usize) -> Vec<(usize, u8, Color)> {
    slot_view
        .iter()
        .flat_map(|v| v.iter_fields())
        .filter_map(|(name, off, _)| slot_field_color(name).map(|c| (off + byte_offset, 8u8, c)))
        .collect()
}

/// The byte offset of each generation's ring index in the raw buffer. CPython stores an `i8`
/// (plus 7 bytes of padding) right *after* each generation's slots — per
/// `compute_ring_base_offsets`, generation `g`'s slots start at `bases[g]`, so its index sits
/// at `bases[g] + slots[g] * item_size`. The index value is the active slot number for that
/// generation's ring. Empty for inline/legacy builds (one slot per generation, no index).
fn ring_index_offsets(
    table: &crate::remote_debugging::offsets::offset_table::OffsetTable,
    item_size: usize,
) -> Vec<usize> {
    use crate::remote_debugging::offsets::offset_table::GcStatsKind;
    if table.gc_stats_kind != GcStatsKind::RingBuffer || item_size == 0 {
        return Vec::new();
    }
    let (Some(bases), Some(slots)) = (table.gc_gen_base_offsets, table.gc_slots_per_gen) else {
        return Vec::new();
    };
    (0..3)
        .map(|g| bases[g] as usize + slots[g] as usize * item_size)
        .collect()
}

/// The GC-stats-only "buffer view" (`g` toggles it). Left column: the slot table (top,
/// arrow-selectable) over the selected slot's decoded field→value list (bottom). Right
/// column: a hexdump of the *whole* stats buffer — the selected slot's byte range shaded
/// `DarkGray` with its decoded fields colored in place, and (on ring builds) each generation's
/// `i8` ring index shaded `Red` as a fixed visual anchor, so the reader can watch the active
/// slot number change in place. Reuses the two-column box, the hex renderer, and
/// `slot_field_color` from the full view; the one difference from `section_gc_stats` is that
/// the hexdump spans the entire buffer, not one slot.
fn build_gc_only_lines(
    data: &CollectedData,
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
    selected_slot: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let gc = &data.interpreter.gc.generation_stats;

    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l("GC Stats Buffer View  ([g] back to full layout)"))));
    lines.push(Line::from(Span::raw(top())));

    if gc.stats_addr == 0 || gc.slots.is_empty() {
        lines.push(Line::from(Span::raw(l("GC Generation Stats: not available"))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }

    let offset_table = data.resolved.table().clone();
    let item_size = if gc.item_size > 0 { gc.item_size } else { gc.raw_stats_bytes.len().min(64) };
    let slot_fields: &[(&str, usize)] = offset_table.gc_layout.map(|l| l.fields).unwrap_or(&[]);
    let selected = selected_slot.min(gc.slots.len() - 1);
    let sel = &gc.slots[selected];

    // Byte offsets of each generation's ring index (ring builds only), shaded in the hexdump
    // below as an anchor for reading the active slot number.
    let index_offsets = ring_index_offsets(&offset_table, item_size);

    // ── Left column ──
    let mut left: Vec<Vec<Span<'static>>> = Vec::new();
    left.push(vec![Span::raw(format!(
        "Buffer @ {:#x}  (size: {} bytes, slot: {} bytes)",
        gc.stats_addr, gc.stats_size, item_size
    ))]);
    // Per-generation summary — slot count, collections rate, and avg collection duration.
    for line in gen_summary_lines(gc.slots_per_gen, rate_per_gen, avg_coll_time_per_gen) {
        left.push(vec![Span::raw(line)]);
    }
    if !index_offsets.is_empty() {
        left.push(vec![
            Span::raw("legend: ".to_string()),
            Span::styled(" idx ", Style::new().bg(Color::Red).fg(Color::Black)),
            Span::raw(" = per-generation ring index (i8, active slot #)".to_string()),
        ]);
    }
    left.push(vec![Span::raw(String::new())]);

    // Slot table (top) — one row per slot, the selected row shaded.
    let hdr = slot_table_header();
    let hdr_len = hdr.len();
    left.push(vec![Span::raw(hdr)]);
    left.push(vec![Span::raw(format!("  {}", "-".repeat(hdr_len - 2)))]);
    for (i, slot) in gc.slots.iter().enumerate() {
        let content = slot_table_row(slot);
        if i == selected {
            left.push(vec![Span::styled(
                format!("{:<pl$}", content, pl = GC_PL),
                Style::new().bg(Color::DarkGray).fg(Color::White),
            )]);
        } else {
            left.push(vec![Span::raw(content)]);
        }
    }

    // Slot table (bottom) — the selected slot decoded field→value, each colored field shaded
    // to match the hexdump.
    let slot_bytes = selected_slot_bytes(&gc.raw_stats_bytes, sel.byte_offset, item_size);
    let slot_view = offset_table
        .gc_layout
        .map(|l| GcStat::from_slot(slot_bytes, l, sel.generation, sel.slot, 0));

    left.push(vec![Span::raw(String::new())]);
    left.push(vec![Span::raw(format!(
        "Slot #{} (gen {}, slot {}) @ {:#x}",
        selected + 1, sel.generation, sel.slot, gc.stats_addr + sel.byte_offset as u64
    ))]);
    let name_w = slot_fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(12);
    for (name, offset, valbits) in slot_view.iter().flat_map(|v| v.iter_fields()) {
        let val_fmt = format_field_value(name, valbits);
        let content = format!("  {:<name_w$} @ +{:<4}  {}", name, offset, val_fmt, name_w = name_w);
        match slot_field_color(name) {
            Some(c) => left.push(vec![Span::styled(
                format!("{:<pl$}", content, pl = GC_PL),
                Style::new().bg(c).fg(Color::Black),
            )]),
            None => left.push(vec![Span::raw(content)]),
        }
    }

    // ── Right column: whole-buffer hexdump ──
    // `hex_dump_rows` takes the FIRST matching highlight, so order is priority: the selected
    // slot's colored fields, then its DarkGray whole-slot shade, then each generation's ring
    // index in Red. The index gaps never overlap a slot, so their order relative to the slot
    // shades doesn't matter. `item_size` is small (24 inline, the ring-struct size otherwise)
    // but capped to the `u8` highlight length.
    let mut highlights = field_highlights(&slot_view, sel.byte_offset);
    highlights.push((sel.byte_offset, item_size.min(255) as u8, Color::DarkGray));
    // The index is an `i8` followed by 7 bytes of padding; shade the whole 8-byte field so it
    // reads as one anchor block between generations.
    for &off in &index_offsets {
        if off + 8 <= gc.raw_stats_bytes.len() {
            highlights.push((off, 8, Color::Red));
        }
    }

    // A 16-byte hexdump row is exactly `GC_PR` wide; `gc_two_col` pads the short final row.
    let right = hex_dump_rows(&gc.raw_stats_bytes, gc.raw_stats_bytes.len(), &highlights, 0);

    // ── Combine ──
    let max_rows = left.len().max(right.len());
    for i in 0..max_rows {
        let lv = left.get(i).cloned().unwrap_or_default();
        let rv = right.get(i).cloned().unwrap_or_default();
        lines.push(gc_two_col(lv, rv));
    }

    lines.push(Line::from(Span::raw(top())));
    lines
}

// ── Format helpers ────────────────────────────────────────────────
fn fmt_val(val: u64) -> String {
    if val > 0xFFFF_FFFF {
        format!("{:#x}", val)
    } else {
        val.to_string()
    }
}

fn fmt_thousands(val: u64) -> String {
    let s = val.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.char_indices() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
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

/// Renders the TUI body for a snapshot to plain strings (spans flattened), so the live
/// integration test can exercise the **Full-tier** section builders
/// (`section_debug_offsets` / `section_interpreter`) that only run against a real 3.13+
/// `_Py_DebugOffsets` struct and so can't be reached from the synthetic-Legacy unit tests.
/// Renders a single static TUI frame as plain text (no styling, no glitch overlay) — the
/// non-interactive counterpart to `run_tui`, used by `tui --output` to dump a frame to a
/// file. Unlike the interactive draw loop the styled `Line`s are flattened to their text,
/// and the PID/version header the loop puts in the `Paragraph` title bar (absent from
/// `build_lines`) is prepended here, since a file has no title bar. Always compiled so both
/// `run_tui_snapshot` and the integration tests can reach it through the public API.
pub fn render_snapshot(
    data: &CollectedData,
    selected_slot: usize,
    show_tree: bool,
    show_hex: bool,
    show_runtime_hex: bool,
    gc_only: bool,
) -> String {
    let stats = &data.interpreter.gc.generation_stats;
    let rate = collections_rate_from_slots(&stats.slots, stats.has_timestamps);
    let avg = avg_collection_time_per_gen(&stats.slots, stats.has_duration);
    let lines = if gc_only {
        build_gc_only_lines(data, rate, avg, selected_slot)
    } else {
        build_lines(data, rate, avg, selected_slot, show_tree, show_hex, show_runtime_hex).0
    };
    let mut out = format!(
        "gcscope tui — PID {} — Python 0x{:08x}\n",
        data.pid, data.runtime_version
    );
    for line in &lines {
        for span in &line.spans {
            out.push_str(span.content.as_ref());
        }
        out.push('\n');
    }
    out
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
                    std::mem::swap(&mut cell.fg, &mut cell.bg);
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
                let y = (h - bh + dy).min(h - 1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    use crate::snapshot::collect::{
        GcSlot, GcStatsSnapshot, GcSubState, InterpreterSnapshot,
    };
    use crate::remote_debugging::offsets::pre_3_13;
    use crate::remote_debugging::session::Resolved;

    // ── Format helpers ────────────────────────────────────────────
    // Leaf input→string logic every section depends on; the thresholds and units are
    // the contract, so pin them exactly.

    #[test]
    fn fmt_val_switches_to_hex_only_above_u32_max() {
        assert_eq!(fmt_val(0), "0");
        assert_eq!(fmt_val(255), "255");
        // Exactly u32::MAX still renders decimal (the guard is strictly greater).
        assert_eq!(fmt_val(0xFFFF_FFFF), "4294967295");
        assert_eq!(fmt_val(0x1_0000_0000), "0x100000000");
    }

    #[test]
    fn fmt_thousands_groups_from_the_right() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(123), "123");
        assert_eq!(fmt_thousands(1234), "1_234");
        assert_eq!(fmt_thousands(1_234_567), "1_234_567");
    }

    #[test]
    fn fmt_bytes_scales_at_the_k_and_m_thresholds() {
        assert_eq!(fmt_bytes(0), "0");
        assert_eq!(fmt_bytes(999), "999");
        assert_eq!(fmt_bytes(1000), "1.0K");
        assert_eq!(fmt_bytes(1500), "1.5K");
        assert_eq!(fmt_bytes(1_000_000), "1.0M");
        assert_eq!(fmt_bytes(2_500_000), "2.5M");
    }

    #[test]
    fn fmt_duration_crosses_from_ms_to_s_at_one_second() {
        assert_eq!(fmt_duration(0.0), "0.000ms");
        assert_eq!(fmt_duration(0.001), "1.000ms");
        assert_eq!(fmt_duration(0.5), "500.000ms");
        // 1.0 is NOT < 1.0, so it renders in seconds.
        assert_eq!(fmt_duration(1.0), "1.000s");
        assert_eq!(fmt_duration(2.5), "2.500s");
    }

    #[test]
    fn fmt_duration_ns_picks_ns_us_ms_by_magnitude() {
        assert_eq!(fmt_duration_ns(Duration::from_nanos(0)), "0ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(500)), "500ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(999)), "999ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(1000)), "1.0\u{00b5}s");
        assert_eq!(fmt_duration_ns(Duration::from_micros(2)), "2.0\u{00b5}s");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(1_500_000)), "1.500ms");
        assert_eq!(fmt_duration_ns(Duration::from_millis(5)), "5.000ms");
    }

    #[test]
    fn fmt_rate_uses_one_decimal_high_two_decimals_mid_and_floors_low_to_zero() {
        assert_eq!(fmt_rate(15.0), "15.0/s");
        assert_eq!(fmt_rate(10.0), "10.0/s");
        assert_eq!(fmt_rate(9.99), "9.99/s");
        assert_eq!(fmt_rate(1.5), "1.50/s");
        assert_eq!(fmt_rate(0.01), "0.01/s");
        // Below 0.01 collapses to the sentinel rather than "0.00/s".
        assert_eq!(fmt_rate(0.009), "0.0/s");
        assert_eq!(fmt_rate(0.0), "0.0/s");
    }

    // ── PRNG ──────────────────────────────────────────────────────
    // Seeded and pure, so both the exact first value and the whole sequence are
    // reproducible — that determinism is what makes the glitch effects testable.

    #[test]
    fn xorshift32_is_deterministic_and_advances_state() {
        // Hand-computed for seed 1: 1 ^ (1<<13) = 8193; ^ (8193>>17)=0; ^ (8193<<5) = 0x42021.
        let mut s = 1u32;
        assert_eq!(xorshift32(&mut s), 0x42021);
        assert_eq!(s, 0x42021, "state must be updated to the returned value");

        // Same seed → identical sequence from two independent states.
        let (mut a, mut b) = (42u32, 42u32);
        let seq_a: Vec<u32> = (0..5).map(|_| xorshift32(&mut a)).collect();
        let seq_b: Vec<u32> = (0..5).map(|_| xorshift32(&mut b)).collect();
        assert_eq!(seq_a, seq_b);
        assert!(seq_a.iter().any(|&v| v != seq_a[0]), "sequence must vary");
    }

    #[test]
    fn rand_range_stays_within_bounds_and_returns_min_when_empty() {
        // Degenerate ranges short-circuit to min without consuming the rng.
        let mut s = 7u32;
        assert_eq!(rand_range(&mut s, 5, 5), 5);
        assert_eq!(rand_range(&mut s, 10, 5), 10);
        assert_eq!(s, 7, "empty range must not advance the rng");

        // Real ranges always land inside [min, max], inclusive.
        let mut s = 123u32;
        for _ in 0..500 {
            let v = rand_range(&mut s, 3, 9);
            assert!((3..=9).contains(&v), "out of range: {v}");
        }
    }

    // ── Hex helpers ───────────────────────────────────────────────

    #[test]
    fn hex_col_emitted_matches_the_documented_widths() {
        assert_eq!(hex_col_emitted(0), 0);
        assert_eq!(hex_col_emitted(1), 3);
        assert_eq!(hex_col_emitted(8), 25);
        assert_eq!(hex_col_emitted(15), 46);
        // A full 16-byte row is 48 chars wide (per the doc comment).
        assert_eq!(hex_col_emitted(16), 48);
    }

    fn row_text(row: &[Span]) -> String {
        row.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn hex_dump_rows_lays_out_offset_bytes_and_ascii() {
        let bytes = b"Hello, World!\x00\xff\x7f"; // 16 bytes
        let rows = hex_dump_rows(bytes, bytes.len(), &[], 0x1000);
        assert_eq!(rows.len(), 1);
        let text = row_text(&rows[0]);
        assert!(text.contains("00001000"), "base offset in row: {text:?}");
        // Non-graphic bytes collapse to '.' in the ascii gutter.
        assert!(text.contains("Hello, World!..."), "ascii gutter: {text:?}");
    }

    #[test]
    fn hex_dump_rows_wraps_at_16_and_honours_the_limit() {
        assert_eq!(hex_dump_rows(&[0u8; 17], 17, &[], 0).len(), 2);
        // `limit` truncates the input before chunking.
        assert_eq!(hex_dump_rows(&[0u8; 32], 16, &[], 0).len(), 1);
    }

    #[test]
    fn hex_dump_rows_styles_the_highlighted_bytes() {
        let rows = hex_dump_rows(&[0xAAu8; 8], 8, &[(0, 4, Color::Green)], 0);
        assert!(
            rows[0].iter().any(|s| s.style.bg == Some(Color::Green)),
            "highlighted bytes must carry the region colour"
        );
    }

    // ── Synthetic Legacy snapshot ─────────────────────────────────
    // The pre-3.13 (`Legacy`) tier is the only one constructible without a live process
    // (flat `OffsetTable`, no bindgen struct). Same trick as `ascii::tests::legacy_data`.

    fn legacy_data(with_slots: bool) -> CollectedData {
        let table = pre_3_13::table_for_version(3, 12).unwrap();
        let slots = if with_slots {
            vec![GcSlot {
                generation: 0,
                slot: 0,
                byte_offset: 0,
                start_ts: 0,
                stop_ts: 0,
                collections: 5,
                collected: 10,
                uncollectable: 0,
                candidates: 3,
                duration: 0.0,
                heap_size: 0,
            }]
        } else {
            Vec::new()
        };
        CollectedData {
            pid: 4321,
            runtime_addr: 0x5000,
            runtime_version: 0x030c0000,
            runtime_raw_bytes: Vec::new(),
            debug_offsets_size: 0,
            resolved: Arc::new(Resolved::Legacy { table }),
            interpreter: InterpreterSnapshot {
                addr: 0x6000,
                gc: GcSubState {
                    raw_bytes: vec![0u8; 64],
                    generation_stats: GcStatsSnapshot {
                        stats_addr: if with_slots { 0x7000 } else { 0 },
                        stats_size: 72,
                        item_size: 24,
                        slots_per_gen: [1, 1, 1],
                        has_timestamps: false,
                        has_duration: false,
                        raw_stats_bytes: vec![0u8; 72],
                        slots,
                    },
                },
                gc_offset: 0x80,
                gc_size: 64,
                id: 0,
                next_addr: 0,
            },
            collect_duration: Duration::from_millis(1),
        }
    }

    /// A synthetic ring snapshot (3.15-style geometry) built from the public `OffsetTable`
    /// fields. The buffer view reads only the table + stats (never the `Resolved` tier), so
    /// `Resolved::Legacy` is just the cheapest table wrapper that still drives the ring path.
    /// The buffer is all zero except each generation's index byte, set to `0x5A` so the Red
    /// index highlight can be found positionally in the hexdump.
    fn ring_data() -> CollectedData {
        use crate::remote_debugging::offsets::offset_table::{
            compute_ring_base_offsets, GcItemLayout, GcStatsKind,
        };
        static RING_LAYOUT: GcItemLayout = GcItemLayout {
            item_size: 24,
            fields: &[("ts_start", 0), ("collections", 8), ("collected", 16)],
        };
        let item = 24usize;
        let slots_per_gen = [11u64, 3, 3];
        let bases = compute_ring_base_offsets(item as u64, &slots_per_gen);
        let region = bases[2] as usize + slots_per_gen[2] as usize * item + 8;

        let mut raw = vec![0u8; region];
        for g in 0..3 {
            let idx_off = bases[g] as usize + slots_per_gen[g] as usize * item;
            raw[idx_off] = 0x5A;
        }

        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_kind = GcStatsKind::RingBuffer;
        table.gc_item_size = Some(item as u64);
        table.gc_slots_per_gen = Some(slots_per_gen);
        table.gc_gen_base_offsets = Some(bases);
        table.gc_layout = Some(&RING_LAYOUT);

        let slots = (0..3)
            .map(|g| GcSlot {
                generation: g as u32,
                slot: 0,
                byte_offset: bases[g as usize] as usize,
                start_ts: 0,
                stop_ts: 0,
                collections: 0,
                collected: 0,
                uncollectable: 0,
                candidates: 0,
                duration: 0.0,
                heap_size: 0,
            })
            .collect();

        CollectedData {
            pid: 4321,
            runtime_addr: 0x5000,
            runtime_version: 0x030f0000,
            runtime_raw_bytes: Vec::new(),
            debug_offsets_size: 0,
            resolved: Arc::new(Resolved::Legacy { table }),
            interpreter: InterpreterSnapshot {
                addr: 0x6000,
                gc: GcSubState {
                    raw_bytes: vec![0u8; 64],
                    generation_stats: GcStatsSnapshot {
                        stats_addr: 0x7000,
                        stats_size: region as u64,
                        item_size: item,
                        slots_per_gen,
                        has_timestamps: true,
                        has_duration: false,
                        raw_stats_bytes: raw,
                        slots,
                    },
                },
                gc_offset: 0x80,
                gc_size: 64,
                id: 0,
                next_addr: 0,
            },
            collect_duration: Duration::from_millis(1),
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn join_lines(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    // ── Section builders (Legacy tier) ────────────────────────────

    #[test]
    fn section_interpreter_legacy_names_the_interpreter_and_flags_the_missing_struct() {
        let data = legacy_data(true);
        let out = join_lines(&section_interpreter_legacy(&data));
        assert!(out.contains("PyInterpreterState @ 0x6000  (id: 0)"), "{out}");
        assert!(out.contains("pre-3.13: no _Py_DebugOffsets"), "{out}");
    }

    #[test]
    fn section_gc_stats_renders_the_generation_table_with_na_summaries() {
        let data = legacy_data(true);
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats Buffer @ 0x7000"), "{out}");
        assert!(out.contains("Gen 0 (Young) - 1 slots"), "{out}");
        assert!(out.contains("slot size: 24 bytes"), "{out}");
        // None rate/avg must degrade to "n/a", not "0" or a panic.
        assert!(out.contains("n/a"), "{out}");
        // The one slot's decoded counters appear in the left table.
        assert!(out.contains("GC Generation Stats Slot #1"), "right-panel slot box: {out}");
    }

    #[test]
    fn section_gc_stats_reports_absent_stats_when_there_are_no_slots() {
        let data = legacy_data(false);
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats: not available"), "{out}");
    }

    /// If the raw stats buffer is empty (e.g. a request skipped it) but decoded `slots`
    /// remain, the right-panel byte slice must not panic even when the selected slot's
    /// `byte_offset` points past the (empty) buffer — the start clamp keeps `start <= end`.
    #[test]
    fn section_gc_stats_does_not_panic_when_raw_is_empty_but_a_slot_is_selected() {
        let mut data = legacy_data(true);
        let stats = &mut data.interpreter.gc.generation_stats;
        stats.raw_stats_bytes = Vec::new();
        stats.slots[0].byte_offset = 48; // a gen-1/2-style offset, past the empty buffer
        // Must render (empty hex panel) rather than slice-index panic.
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats Slot #1"), "{out}");
    }

    #[test]
    fn build_gc_only_lines_shows_the_slot_table_field_list_and_whole_buffer_hexdump() {
        let data = legacy_data(true);
        let lines = build_gc_only_lines(&data, [None; 3], [None; 3], 0);
        let out = join_lines(&lines);
        assert!(out.contains("GC Stats Buffer View"), "mode header: {out}");
        assert!(out.contains("Buffer @ 0x7000"), "buffer address line: {out}");
        // Left: the slot table row and the selected slot's decoded field list.
        assert!(out.contains("collections"), "field table: {out}");
        assert!(out.contains("Slot #1 (gen 0, slot 0)"), "selected-slot header: {out}");
        // Right: a whole-buffer hexdump — the 72-byte buffer spans past the first slot, so an
        // offset row beyond the first 16 bytes proves it dumps the whole buffer, not one slot.
        assert!(out.contains("00000010"), "hexdump must cover the whole buffer: {out}");
        // No line overflows the fixed frame width.
        let border = format!("+{}+", "-".repeat(OUTER_W));
        assert!(out.lines().any(|l| l == border), "a full-width border must appear");
        assert!(
            out.lines().all(|l| l.chars().count() <= OUTER_W + 2),
            "a line exceeded the frame width: {out}"
        );
    }

    #[test]
    fn build_gc_only_lines_reports_absent_stats_and_never_panics_on_an_empty_buffer() {
        // No slots → the not-available short-circuit.
        let out = join_lines(&build_gc_only_lines(&legacy_data(false), [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats: not available"), "{out}");
        // Slots but an empty raw buffer with a past-the-end offset must clamp, not panic.
        let mut data = legacy_data(true);
        let stats = &mut data.interpreter.gc.generation_stats;
        stats.raw_stats_bytes = Vec::new();
        stats.slots[0].byte_offset = 48;
        let out = join_lines(&build_gc_only_lines(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("Slot #1 (gen 0, slot 0)"), "{out}");
    }

    #[test]
    fn ring_index_offsets_point_just_past_each_generations_slots() {
        use crate::remote_debugging::offsets::offset_table::{compute_ring_base_offsets, GcStatsKind};

        // A GIL ring: slots [11, 3, 3], 24-byte items. Build the geometry from the public
        // fields (set_ring is private to the offsets module).
        let item = 24usize;
        let slots = [11u64, 3, 3];
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_kind = GcStatsKind::RingBuffer;
        table.gc_item_size = Some(item as u64);
        table.gc_slots_per_gen = Some(slots);
        table.gc_gen_base_offsets = Some(compute_ring_base_offsets(item as u64, &slots));
        let bases = table.gc_gen_base_offsets.unwrap();

        // Each index sits immediately after its generation's slots, i.e. 8 bytes before the
        // next generation's base (and, for the last, at the buffer's trailing cursor).
        let offs = ring_index_offsets(&table, item);
        assert_eq!(
            offs,
            vec![
                bases[0] as usize + 11 * item,
                bases[1] as usize + 3 * item,
                bases[2] as usize + 3 * item,
            ]
        );
        assert_eq!(offs[0], bases[1] as usize - 8, "gen-0 index is the 8-byte gap before gen 1");
        assert_eq!(offs[1], bases[2] as usize - 8, "gen-1 index is the 8-byte gap before gen 2");

        // Inline/Legacy is not a ring → no index gaps at all.
        let legacy = legacy_data(true);
        assert!(ring_index_offsets(legacy.resolved.table(), 24).is_empty());
    }

    /// Every span across the frame with a `Red` background whose trimmed text equals `hex`.
    fn red_bytes(lines: &[Line], hex: &str) -> usize {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter(|s| s.style.bg == Some(Color::Red) && s.content.trim() == hex)
            .count()
    }

    #[test]
    fn build_gc_only_lines_on_a_ring_highlights_every_index_byte_and_shows_the_legend() {
        let lines = build_gc_only_lines(&ring_data(), [Some(15.0), None, None], [None; 3], 0);

        // The per-generation summary header renders (slot counts + rate/avg, "n/a" where absent).
        let text = join_lines(&lines);
        assert!(text.contains("Gen 0 (Young) - 11 slots  (rate = 15.0/s, avg coll = n/a)"), "summary: {text}");
        assert!(text.contains("Gen 2 (Oldest) - 3 slots"), "gen-2 summary: {text}");

        // The legend appears (ring builds only) with a Red swatch.
        assert!(text.contains("per-generation ring index"), "legend text missing");
        assert!(
            lines.iter().flat_map(|l| &l.spans)
                .any(|s| s.style.bg == Some(Color::Red) && s.content.trim() == "idx"),
            "legend must carry a Red swatch"
        );

        // The one index byte of each generation (0x5A) is Red in the hexdump — exactly three,
        // one per generation, and the only 0x5A bytes in the otherwise-zero buffer.
        assert_eq!(red_bytes(&lines, "5a"), 3, "all three ring index bytes must be highlighted");

        // Selected-slot decoration still works alongside the index anchors: its `collections`
        // field keeps Green and its whole-slot range keeps the DarkGray shade.
        let spans = || lines.iter().flat_map(|l| &l.spans);
        assert!(spans().any(|s| s.style.bg == Some(Color::Green)), "field colours must survive");
        assert!(spans().any(|s| s.style.bg == Some(Color::DarkGray)), "selected-slot shade must survive");
    }

    #[test]
    fn build_gc_only_lines_on_an_inline_build_has_no_index_highlight_or_legend() {
        let lines = build_gc_only_lines(&legacy_data(true), [None; 3], [None; 3], 0);
        assert!(!join_lines(&lines).contains("ring index"), "inline/legacy must show no legend");
        assert!(
            !lines.iter().flat_map(|l| &l.spans).any(|s| s.style.bg == Some(Color::Red)),
            "inline/legacy has no ring index to highlight"
        );
    }

    #[test]
    fn debug_tree_row_color_shades_named_fields_and_every_first_slot_item() {
        use crate::tui::tree::TreeEntryKind;
        // Named runtime fields keep their hexdump-region colors.
        assert_eq!(debug_tree_row_color("cookie[8]", TreeEntryKind::RawValue { offset: 0 }), Some(Color::Green));
        assert_eq!(debug_tree_row_color("gc", TreeEntryKind::RawValue { offset: 88 }), Some(Color::Magenta));
        // Every first-slot field item (Layout) shares one color, whatever its field name.
        let layout = |off| TreeEntryKind::Layout { field_type: "", field_offset: off };
        assert_eq!(debug_tree_row_color("ts_start", layout(0)), Some(Color::Blue));
        assert_eq!(debug_tree_row_color("increment_size", layout(32)), Some(Color::Blue));
        // Groups, derived rows, and unnamed raw values stay unshaded.
        assert_eq!(debug_tree_row_color("young_slots (11)", TreeEntryKind::Derived), None);
        assert_eq!(debug_tree_row_color("version", TreeEntryKind::RawValue { offset: 8 }), None);
    }

    #[test]
    fn build_lines_on_a_legacy_snapshot_skips_the_debug_offsets_section() {
        let data = legacy_data(true);
        let (lines, _slot_idx) = build_lines(&data, [None; 3], [None; 3], 0, true, true, false);
        let out = join_lines(&lines);
        // Pre-3.13 → no _Py_DebugOffsets section, straight to the legacy header + GC table.
        assert!(!out.contains("_Py_DebugOffsets (embedded"), "legacy must skip section 1: {out}");
        assert!(out.contains("pre-3.13: no _Py_DebugOffsets"), "{out}");
        assert!(out.contains("Gen 0 (Young) - 1 slots"), "{out}");
    }

    /// `render_snapshot` (the `tui --output` path) prepends the PID/version header the
    /// interactive title bar normally supplies — absent from `build_lines` — then flattens
    /// the styled frame to plain text. The body's box borders stay at the fixed frame width;
    /// only the prepended header is shorter.
    #[test]
    fn render_snapshot_prepends_header_and_flattens_the_frame() {
        let out = render_snapshot(&legacy_data(true), 0, true, true, false, false);
        assert!(
            out.lines().next().unwrap().contains("PID 4321")
                && out.lines().next().unwrap().contains("0x030c0000"),
            "first line must be the PID/version header: {out}"
        );
        // The flattened body still carries the GC section (proves build_lines ran + joined).
        assert!(out.contains("Gen 0 (Young) - 1 slots"), "{out}");
        // A full-width border appears and no line overflows the frame (header aside).
        let border = format!("+{}+", "-".repeat(OUTER_W));
        assert!(out.lines().any(|line| line == border), "a full-width border must appear");
        assert!(
            out.lines().all(|line| line.chars().count() <= OUTER_W + 2),
            "a line exceeded the frame width"
        );
    }

    // ── status_bar ────────────────────────────────────────────────
    // A `Paragraph` doesn't expose its text directly, so render it into a `Buffer` and
    // read the cells back.

    fn render_status(
        scroll: u16,
        max_scroll: u16,
        slot: usize,
        slot_count: usize,
        glitch_active: bool,
        cl_active: bool,
        glitch_enabled: bool,
    ) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, 220, 1));
        status_bar(scroll, max_scroll, slot, slot_count, glitch_active, cl_active, glitch_enabled)
            .render(buf.area, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn status_bar_shows_slot_position_and_the_100_percent_sentinel_when_unscrollable() {
        let out = render_status(0, 0, 0, 1, false, false, true);
        assert!(out.contains("[q] quit"), "{out}");
        assert!(out.contains("[p] pick pid"), "{out}");
        assert!(out.contains("slot 1/1"), "{out}");
        // max_scroll == 0 → checked_div is None → the "100%" branch.
        assert!(out.contains("100%"), "{out}");
        assert!(out.contains("[g] gc-view"), "gc-view mode hint: {out}");
        assert!(out.contains("glitch:on"), "glitch-enabled label: {out}");
    }

    #[test]
    fn status_bar_reflects_no_slots_disabled_glitch_and_the_badges() {
        assert!(render_status(0, 0, 0, 0, false, false, true).contains("no slots"));
        assert!(render_status(0, 0, 0, 1, false, false, false).contains("glitch:off"));
        // Connection-lost outranks the ordinary glitch badge.
        assert!(render_status(0, 0, 0, 1, true, true, true).contains("[CL]"));
        // The firing-glitch badge is `[FX]`, distinct from the `[G]` (mode) / `[G] glitch` keys.
        assert!(render_status(0, 0, 0, 1, true, false, true).contains("[FX]"));
    }

    // ── Glitch effects (buffer-mutating) ──────────────────────────
    // Seeded, so deterministic. We assert on invariants — no out-of-buffer writes (a bad
    // index would panic), dimensions preserved — not on exact glyphs.

    #[test]
    fn apply_glitch_never_writes_out_of_bounds_across_seeds() {
        for seed in [1u32, 2, 999, 0x1234_5678, u32::MAX] {
            let mut rng = seed;
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
            for _ in 0..200 {
                apply_glitch(&mut buf, &mut rng);
            }
            assert_eq!(buf.area, Rect::new(0, 0, 80, 24), "glitch must not resize the buffer");
        }
    }

    #[test]
    fn apply_glitch_is_a_noop_on_a_too_small_buffer() {
        let mut rng = 5u32;
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1)); // below the w>=4/h>=2 floor
        let before: Vec<String> = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        apply_glitch(&mut buf, &mut rng);
        let after: Vec<String> = buf.content.iter().map(|c| c.symbol().to_string()).collect();
        assert_eq!(before, after, "no cell should change below the size floor");
    }

    #[test]
    fn apply_connection_lost_buildup_runs_at_both_ends_of_progress() {
        let mut rng = 77u32;
        for progress in [0.0f64, 0.5, 0.85, 1.0] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 100, 30));
            apply_connection_lost_buildup(&mut buf, &mut rng, progress);
            assert_eq!(buf.area, Rect::new(0, 0, 100, 30));
        }
        // Too-small buffer is a clean no-op.
        let mut small = Buffer::empty(Rect::new(0, 0, 2, 1));
        apply_connection_lost_buildup(&mut small, &mut rng, 1.0);
    }

    #[test]
    fn draw_connection_lost_box_paints_the_message_and_tolerates_jitter() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        draw_connection_lost_box(&mut buf, 0, 0);
        let text: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("CONNECTION LOST"), "box message must be drawn: {text:?}");
        // Jitter offsets and a clamped-tiny buffer must not write out of bounds.
        draw_connection_lost_box(&mut buf, 1, -1);
        draw_connection_lost_box(&mut Buffer::empty(Rect::new(0, 0, 10, 3)), 0, 0);
    }

    // ── Tier A: key reducer (TuiState) ────────────────────────────
    // The interactive event loop's decision logic, extracted so each binding is a plain
    // input→state assertion with no terminal.

    #[test]
    fn handle_key_moves_the_slot_selection_with_saturation() {
        let mut s = TuiState::new(100, false);
        assert_eq!(s.handle_key(KeyCode::Down), KeyOutcome::Continue);
        assert_eq!(s.selected_slot, 1);
        s.handle_key(KeyCode::Char('j'));
        assert_eq!(s.selected_slot, 2);
        s.handle_key(KeyCode::Up);
        assert_eq!(s.selected_slot, 1);
        // Saturates at 0 rather than underflowing.
        s.handle_key(KeyCode::Char('k'));
        s.handle_key(KeyCode::Char('k'));
        assert_eq!(s.selected_slot, 0);
    }

    #[test]
    fn handle_key_toggles_the_debug_offsets_panels() {
        let mut s = TuiState::new(100, false);
        assert!(s.debug_offsets_show_tree && s.debug_offsets_show_hex, "both shown by default");
        s.handle_key(KeyCode::Char('t'));
        assert!(!s.debug_offsets_show_tree);
        s.handle_key(KeyCode::Char('h'));
        assert!(!s.debug_offsets_show_hex);
        s.handle_key(KeyCode::Char('d'));
        assert!(s.show_runtime_hex);
    }

    #[test]
    fn handle_key_s_requests_a_snapshot_dump_without_mutating_view_state() {
        let mut s = TuiState::new(100, false);
        let before = s.clone();
        assert_eq!(s.handle_key(KeyCode::Char('s')), KeyOutcome::DumpSnapshot);
        // The dump is pure I/O the loop performs; the key must not change the view.
        assert_eq!(s, before);
    }

    #[test]
    fn handle_key_o_collapses_when_anything_shown_and_expands_when_all_hidden() {
        let mut s = TuiState::new(100, false);
        // Both shown → collapse to both hidden.
        s.handle_key(KeyCode::Char('o'));
        assert!(!s.debug_offsets_show_tree && !s.debug_offsets_show_hex);
        // Both hidden → expand to both shown.
        s.handle_key(KeyCode::Char('o'));
        assert!(s.debug_offsets_show_tree && s.debug_offsets_show_hex);
        // Mixed (only tree shown) still counts as "shown" → collapse both.
        s.debug_offsets_show_hex = false;
        s.handle_key(KeyCode::Char('o'));
        assert!(!s.debug_offsets_show_tree && !s.debug_offsets_show_hex);
    }

    #[test]
    fn handle_key_rate_steps_by_ten_and_floors_at_ten() {
        let mut s = TuiState::new(100, false);
        s.handle_key(KeyCode::Char('r'));
        assert_eq!(s.rate_ms, 90);
        s.handle_key(KeyCode::Char('R'));
        assert_eq!(s.rate_ms, 100);
        // Down never drops below 10, even stepping from just above it.
        s.rate_ms = 15;
        s.handle_key(KeyCode::Char('r'));
        assert_eq!(s.rate_ms, 10);
        s.handle_key(KeyCode::Char('r'));
        assert_eq!(s.rate_ms, 10);
    }

    #[test]
    fn handle_key_scrolls_and_toggles_glitch() {
        let mut s = TuiState::new(100, true);
        s.handle_key(KeyCode::PageDown);
        assert_eq!(s.scroll, 10);
        s.handle_key(KeyCode::PageDown);
        assert_eq!(s.scroll, 20);
        s.handle_key(KeyCode::PageUp);
        assert_eq!(s.scroll, 10);
        s.handle_key(KeyCode::End);
        assert_eq!(s.scroll, u16::MAX);
        s.handle_key(KeyCode::Home);
        assert_eq!(s.scroll, 0);
        // Shift-`G` toggles the glitch effect (lower-case `g` is the view toggle below).
        s.handle_key(KeyCode::Char('G'));
        assert!(!s.glitch_enabled);
    }

    #[test]
    fn handle_key_g_toggles_the_gc_only_view_leaving_glitch_alone() {
        let mut s = TuiState::new(100, true);
        assert!(!s.gc_only, "full layout by default");
        s.handle_key(KeyCode::Char('g'));
        assert!(s.gc_only, "`g` switches to the GC-stats buffer view");
        assert!(s.glitch_enabled, "`g` must not touch the glitch effect");
        s.handle_key(KeyCode::Char('g'));
        assert!(!s.gc_only, "`g` toggles back to the full layout");
    }

    #[test]
    fn handle_key_signals_quit_pickpid_and_ignores_unbound_keys() {
        let mut s = TuiState::new(100, false);
        assert_eq!(s.handle_key(KeyCode::Char('q')), KeyOutcome::Quit);
        assert_eq!(s.handle_key(KeyCode::Esc), KeyOutcome::Quit);
        assert_eq!(s.handle_key(KeyCode::Char('p')), KeyOutcome::PickPid);
        let before = s.clone();
        assert_eq!(s.handle_key(KeyCode::Char('z')), KeyOutcome::Continue);
        assert_eq!(s, before, "an unbound key must not mutate state");
    }

    #[test]
    fn reset_view_clears_the_view_but_keeps_rate_and_glitch() {
        let mut s = TuiState::new(250, true);
        s.scroll = 40;
        s.selected_slot = 3;
        s.debug_offsets_show_tree = false;
        s.debug_offsets_show_hex = false;
        s.show_runtime_hex = true;
        s.reset_view();
        assert_eq!((s.scroll, s.selected_slot), (0, 0));
        assert!(s.debug_offsets_show_tree && s.debug_offsets_show_hex && !s.show_runtime_hex);
        // rate_ms and glitch_enabled survive a re-pick, as the original loop did.
        assert_eq!(s.rate_ms, 250);
        assert!(s.glitch_enabled);
    }

    #[test]
    fn clamp_slot_pulls_the_selection_into_range() {
        let mut s = TuiState::new(100, false);
        s.selected_slot = 5;
        s.clamp_slot(3);
        assert_eq!(s.selected_slot, 2, "clamped to the last valid slot");
        s.clamp_slot(0);
        assert_eq!(s.selected_slot, 0, "no slots → index 0");
        s.selected_slot = 1;
        s.clamp_slot(4);
        assert_eq!(s.selected_slot, 1, "an in-range selection is left alone");
    }

    // ── Tier B: glitch timer (GlitchState) ────────────────────────
    // `now` is injected, so every transition is deterministic without reading the clock.

    #[test]
    fn glitch_tick_is_a_noop_while_disabled() {
        let t0 = Instant::now();
        let mut g = GlitchState::new(t0);
        let mut rng = 1u32;
        // next_glitch_at == t0, so an *enabled* tick would fire; a disabled one must not.
        g.tick(t0 + Duration::from_millis(1), false, &mut rng);
        assert!(!g.glitch_active && !g.badge_active());
    }

    #[test]
    fn glitch_tick_activates_then_clears_an_ordinary_glitch() {
        let t0 = Instant::now();
        let mut g = GlitchState::new(t0);
        let mut rng = 12345u32;

        // First enabled tick past next_glitch_at turns the glitch on.
        g.tick(t0 + Duration::from_millis(1), true, &mut rng);
        assert!(g.glitch_active && g.should_glitch(true) && g.badge_active());

        // Ticking past glitch_end clears it and schedules the next one in the future.
        let end = g.glitch_end;
        g.tick(end + Duration::from_millis(1), true, &mut rng);
        assert!(!g.glitch_active);
        assert!(g.next_glitch_at > end);
    }

    #[test]
    fn glitch_tick_runs_the_connection_lost_sequence_through_its_phases() {
        let t0 = Instant::now();
        let mut g = GlitchState::new(t0);
        let mut rng = 999u32;

        // next_cl_show is ~30s out; crossing it enters the build-up phase.
        let enter = t0 + Duration::from_secs(31);
        g.tick(enter, true, &mut rng);
        assert!(g.cl_active && g.cl_phase == 1);
        assert!(g.should_buildup(true) && !g.should_glitch(true), "CL outranks the plain glitch");

        // Build-up lasts 1s → message phase.
        g.tick(enter + Duration::from_millis(1001), true, &mut rng);
        assert_eq!(g.cl_phase, 2);
        assert!(g.should_msg(true));

        // Past cl_end the sequence resets and reschedules both timers into the future.
        let end = g.cl_end;
        g.tick(end + Duration::from_millis(1), true, &mut rng);
        assert!(!g.cl_active && g.cl_phase == 0);
        assert!(g.next_cl_show > end && g.next_glitch_at > end);
    }

    #[test]
    fn update_jitter_moves_within_one_cell_and_throttles() {
        let t0 = Instant::now();
        let mut g = GlitchState::new(t0);
        let mut rng = 7u32;
        // Force the message phase, where jitter is live.
        g.cl_active = true;
        g.cl_phase = 2;

        let j = t0 + Duration::from_secs(1);
        g.update_jitter(j, &mut rng);
        assert!((-1..=1).contains(&g.cl_jx) && (-1..=1).contains(&g.cl_jy));
        assert_eq!(g.cl_last_jitter, j);

        // A second update inside the 200ms window is throttled out.
        g.update_jitter(j + Duration::from_millis(100), &mut rng);
        assert_eq!(g.cl_last_jitter, j, "jitter must not update faster than ~5 Hz");
    }
}
