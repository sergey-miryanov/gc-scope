//! The glitch / "connection lost" visual-effect subsystem: a deterministic timer state
//! machine ([`GlitchState`]) plus the buffer-mutating renderers it drives. Split out of the
//! render loop so its transitions can be tested against an injected clock and a seeded PRNG.
use std::time::{Duration, Instant};

use ratatui::style::Color;

// ── Glitch / connection-lost timer ────────────────────────────────
/// The glitch and "connection lost" visual-effect state machine, split out of the render
/// loop so its transitions can be tested against an injected clock. `tick`/`update_jitter`
/// take `now` as a parameter instead of calling `Instant::now()` internally.
pub(super) struct GlitchState {
    glitch_active: bool,
    next_glitch_at: Instant,
    glitch_end: Instant,
    pub(super) cl_active: bool,
    cl_phase: u8, // 0=inactive, 1=build-up, 2=message
    pub(super) cl_phase_start: Instant,
    cl_end: Instant,
    next_cl_show: Instant,
    pub(super) cl_jx: i32,
    pub(super) cl_jy: i32,
    cl_last_jitter: Instant,
}

impl GlitchState {
    pub(super) fn new(now: Instant) -> Self {
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
    pub(super) fn tick(&mut self, now: Instant, glitch_enabled: bool, rng: &mut u32) {
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
    pub(super) fn update_jitter(&mut self, now: Instant, rng: &mut u32) {
        if self.cl_active
            && self.cl_phase == 2
            && now >= self.cl_last_jitter + Duration::from_millis(200)
        {
            self.cl_jx = ((rand_range(rng, 0, 2) as i32) - 1).clamp(-1, 1);
            self.cl_jy = ((rand_range(rng, 0, 2) as i32) - 1).clamp(-1, 1);
            self.cl_last_jitter = now;
        }
    }

    pub(super) fn should_glitch(&self, enabled: bool) -> bool {
        enabled && !self.cl_active && self.glitch_active
    }
    pub(super) fn should_buildup(&self, enabled: bool) -> bool {
        enabled && self.cl_active && self.cl_phase == 1
    }
    pub(super) fn should_msg(&self, enabled: bool) -> bool {
        enabled && self.cl_active && self.cl_phase == 2
    }
    pub(super) fn badge_active(&self) -> bool {
        self.glitch_active || self.cl_active
    }
}

// ── PRNG ──────────────────────────────────────────────────────────
fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

pub(super) fn rand_range(rng: &mut u32, min: u32, max: u32) -> u32 {
    if min >= max {
        return min;
    }
    min + xorshift32(rng) % (max - min + 1)
}

// ── Glitch effects ─────────────────────────────────────────────────
pub(super) fn apply_glitch(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32) {
    let count = if rand_range(rng, 0, 1) == 0 { 1 } else { 2 };
    for _ in 0..count {
        apply_one_glitch(buffer, rng);
    }
}

pub(super) fn apply_one_glitch(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32) {
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
pub(super) fn apply_connection_lost_buildup(buffer: &mut ratatui::buffer::Buffer, rng: &mut u32, progress: f64) {
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

pub(super) fn draw_connection_lost_box(buffer: &mut ratatui::buffer::Buffer, jx: i32, jy: i32) {
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

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

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

    // ── Glitch timer (GlitchState) ────────────────────────────────
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
