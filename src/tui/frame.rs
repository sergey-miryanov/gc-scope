//! The interactive TUI driver: the terminal lifecycle ([`TerminalGuard`]), the render loop
//! ([`run_tui`]), and the pure key reducer ([`TuiState`]) that makes every binding unit-testable
//! without a terminal. The frame *content* is built elsewhere — `layout`/`sections`/`gc_view`
//! assemble the `Line`s, `glitch` overlays the effects, `format` does the leaf formatting.
use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::text::Text;
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::Terminal;

use crate::snapshot::collect::{avg_collection_time_per_gen, collections_rate_from_entries, CollectRequest};
use crate::snapshot::poller::SnapshotPoller;

use super::format::fmt_duration_ns;
use super::gc_view::build_gc_buffer_view;
use super::glitch::{
    apply_connection_lost_buildup, apply_glitch, apply_one_glitch, draw_connection_lost_box,
    rand_range, GlitchState,
};
use super::layout::{build_lines, render_snapshot, status_bar};

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

    // View state (scroll, entry selection, panel toggles, rate, glitch-enable) — every
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

        // Clamp selected_entry to valid range based on new data
        let entry_count = data.interpreter.gc.generation_stats.entries.len();
        state.clamp_entry(entry_count);

        // Act on a pending `s` dump now that this frame's `data` exists. Overwrites
        // `<pid>.txt` in the working directory with the current view.
        if pending_dump {
            pending_dump = false;
            let path = format!("{}.txt", poller.pid());
            let frame = render_snapshot(&data, state.selected_entry, state.debug_offsets_show_tree, state.debug_offsets_show_hex, state.show_runtime_hex, state.gc_only);
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
        let entries = &stats.entries;
        let (rate_per_gen, avg_coll_time_per_gen) = (
            collections_rate_from_entries(entries, stats.has_timestamps),
            avg_collection_time_per_gen(entries, stats.has_duration),
        );
        let styled_lines = if state.gc_only {
            build_gc_buffer_view(&data, rate_per_gen, avg_coll_time_per_gen, state.selected_entry)
        } else {
            build_lines(&data, rate_per_gen, avg_coll_time_per_gen, state.selected_entry, state.debug_offsets_show_tree, state.debug_offsets_show_hex, state.show_runtime_hex).0
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
        let selected_entry = state.selected_entry;
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

            let status = status_bar(scroll, max_scroll, selected_entry, entry_count, glitch_badge_active, cl_active, glitch_enabled);
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
    selected_entry: usize,
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
            selected_entry: 0,
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
        self.selected_entry = 0;
        self.debug_offsets_show_tree = true;
        self.debug_offsets_show_hex = true;
        self.show_runtime_hex = false;
        self.gc_only = false;
    }

    /// Pulls `selected_entry` back into `[0, entry_count)` when a new snapshot has fewer entries.
    fn clamp_entry(&mut self, entry_count: usize) {
        let max_entry = entry_count.saturating_sub(1);
        if self.selected_entry > max_entry {
            self.selected_entry = max_entry;
        }
    }

    /// Applies one key press to the view state, returning whether the loop should continue,
    /// quit, or open the PID picker. Pure aside from `&mut self`, so every binding is
    /// directly unit-testable without a terminal.
    fn handle_key(&mut self, code: KeyCode) -> KeyOutcome {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.selected_entry = self.selected_entry.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected_entry = self.selected_entry.saturating_add(1),
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Key reducer (TuiState) ────────────────────────────────────
    // The interactive event loop's decision logic, extracted so each binding is a plain
    // input→state assertion with no terminal.

    #[test]
    fn handle_key_moves_the_entry_selection_with_saturation() {
        let mut s = TuiState::new(100, false);
        assert_eq!(s.handle_key(KeyCode::Down), KeyOutcome::Continue);
        assert_eq!(s.selected_entry, 1);
        s.handle_key(KeyCode::Char('j'));
        assert_eq!(s.selected_entry, 2);
        s.handle_key(KeyCode::Up);
        assert_eq!(s.selected_entry, 1);
        // Saturates at 0 rather than underflowing.
        s.handle_key(KeyCode::Char('k'));
        s.handle_key(KeyCode::Char('k'));
        assert_eq!(s.selected_entry, 0);
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
        s.selected_entry = 3;
        s.debug_offsets_show_tree = false;
        s.debug_offsets_show_hex = false;
        s.show_runtime_hex = true;
        s.reset_view();
        assert_eq!((s.scroll, s.selected_entry), (0, 0));
        assert!(s.debug_offsets_show_tree && s.debug_offsets_show_hex && !s.show_runtime_hex);
        // rate_ms and glitch_enabled survive a re-pick, as the original loop did.
        assert_eq!(s.rate_ms, 250);
        assert!(s.glitch_enabled);
    }

    #[test]
    fn clamp_entry_pulls_the_selection_into_range() {
        let mut s = TuiState::new(100, false);
        s.selected_entry = 5;
        s.clamp_entry(3);
        assert_eq!(s.selected_entry, 2, "clamped to the last valid entry");
        s.clamp_entry(0);
        assert_eq!(s.selected_entry, 0, "no entries → index 0");
        s.selected_entry = 1;
        s.clamp_entry(4);
        assert_eq!(s.selected_entry, 1, "an in-range selection is left alone");
    }
}
