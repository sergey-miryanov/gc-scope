//! Frame geometry and the low-level line/box/hex primitives every section builder emits
//! through, plus the two top-level assemblers: [`build_lines`] (the full layout) and
//! [`render_snapshot`] (the plain-text `tui --output` flatten). The section builders live in
//! `sections` (the `_Py_DebugOffsets` struct) and `gc_view` (the GC generation stats).
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::snapshot::collect::{avg_collection_time_per_gen, collections_rate_from_entries, CollectedData};

use super::gc_view::{build_gc_only_lines, section_gc_stats};
use super::sections::{section_debug_offsets, section_interpreter, section_interpreter_legacy};

// ── Layout constants ──────────────────────────────────────────────
pub(super) const OUTER_W: usize = 158;
pub(super) const PL: usize = 65;
pub(super) const PR: usize = 90;
pub(super) const INNER_W: usize = PL - 4;      // 61
pub(super) const INNER_TW: usize = INNER_W - 2; // 59

// The GC-stats buffer view splits the same 160-col frame differently from the full view: a
// 16-byte hexdump row is exactly 78 cols wide, so the right column is pinned to that and the
// left gets the rest (`| L | R |` = 1+77+3+78+1 = 160). The left is wider than the full view's
// `PL` so a `+inc` build's long field names (`ts_handle_weakref_callbacks_start`, …) fit.
pub(super) const GC_PR: usize = 78;
pub(super) const GC_PL: usize = OUTER_W + 2 - GC_PR - 5; // 77 (frame 160 minus the `| … | … |` framing)

// Seven heterogeneous scalars, all read off the render loop's local state at the single
// call site below. Poll rate and poll time live in the header title instead, so they don't
// appear here twice.
pub(super) fn status_bar(scroll: u16, max_scroll: u16, entry: usize, entry_count: usize, glitch_active: bool, cl_active: bool, glitch_enabled: bool) -> Paragraph<'static> {
    let style = Style::new().bg(Color::Blue).fg(Color::White);
    // u32 math on purpose: `scroll` is a u16 and `scroll * 100` overflows it once the
    // scrollback passes 655 rows — a debug-build panic in a view that can easily be
    // longer than that. `checked_div` covers the max_scroll == 0 (nothing to scroll) case.
    let scroll_pct = match (scroll as u32 * 100).checked_div(max_scroll as u32) {
        Some(pct) => format!(" {pct:>3}%"),
        None => " 100%".to_string(),
    };
    let entry_text = if entry_count > 0 {
        format!(" entry {}/{} ", entry + 1, entry_count)
    } else {
        " no entries ".to_string()
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
        Span::styled(entry_text, style.bg(Color::DarkGray)),
        Span::styled(format!(" [PgUp/PgDn] scroll{} ", scroll_pct), style),
    ]);
    Paragraph::new(text)
}

// ── Main line builder ─────────────────────────────────────────────
pub(super) fn build_lines(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3], selected_entry: usize, debug_offsets_show_tree: bool, debug_offsets_show_hex: bool, show_runtime_hex: bool) -> (Vec<Line<'static>>, usize) {
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
    lines.extend(section_gc_stats(data, rate_per_gen, avg_coll_time_per_gen, selected_entry));
    // Entry row in section_gc_stats starts at index 3 (top/buffer/top) + 7 header lines in the interleave
    let entry_line_idx = s1_len + sep1 + s2_len + 1 + 3 + 7 + selected_entry;
    (lines, entry_line_idx)
}

// ── Box helpers ───────────────────────────────────────────────────
pub(super) fn top() -> String {
    format!("+{}+", "-".repeat(OUTER_W))
}

// `| ` + content + ` |` must total `top()`'s width (OUTER_W + 2), so the padded content
// area is OUTER_W - 2 — not OUTER_W, which overpads full-width rows by 2 and pushes them
// past the box border (harmless clipping in a live terminal, visible misalignment in a
// `tui --output` file dump).
pub(super) fn l(content: &str) -> String {
    format!("| {:<w$} |", content, w = OUTER_W - 2)
}

pub(super) fn plain_line(left: &str, right: &str) -> Line<'static> {
    Line::from(Span::raw(format!(
        "|{:<pl$} | {:<pr$}|",
        left, right, pl = PL, pr = PR
    )))
}

pub(super) fn full_line(left: &str, right_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw(format!("|{:<pl$} | ", left, pl = PL))];
    let rw: usize = right_spans.iter().map(|s| s.content.len()).sum();
    spans.extend(right_spans);
    if rw < PR {
        spans.push(Span::raw(" ".repeat(PR - rw)));
    }
    spans.push(Span::raw("|"));
    Line::from(spans)
}

pub(super) fn span_line(left_spans: Vec<Span<'static>>, right_spans: Vec<Span<'static>>) -> Line<'static> {
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
pub(super) fn gc_two_col(left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
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

pub(super) fn styled_left_inner_box(content: &str, color: Option<Color>) -> Vec<Span<'static>> {
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

pub(super) fn padding_hex_right(hex_spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let rw: usize = hex_spans.iter().map(|s| s.content.len()).sum();
    let mut spans = hex_spans;
    if rw < PR {
        spans.push(Span::raw(" ".repeat(PR - rw)));
    }
    spans
}

// ── Hex dump renderer ─────────────────────────────────────────────
pub(super) fn hex_dump_rows(
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
pub(super) fn hex_col_emitted(n: usize) -> usize {
    (0..n)
        .map(|i| 2 + if i < 15 { if i == 7 { 2 } else { 1 } } else { 0 })
        .sum()
}

/// Renders a single static TUI frame as plain text (no styling, no glitch overlay) — the
/// non-interactive counterpart to `run_tui`, used by `tui --output` to dump a frame to a
/// file. Unlike the interactive draw loop the styled `Line`s are flattened to their text,
/// and the PID/version header the loop puts in the `Paragraph` title bar (absent from
/// `build_lines`) is prepended here, since a file has no title bar. Always compiled so both
/// `run_tui_snapshot` and the integration tests can reach it through the public API — in
/// particular the **Full-tier** section builders (`section_debug_offsets` /
/// `section_interpreter`) that only run against a real 3.13+ `_Py_DebugOffsets` struct and so
/// can't be reached from the synthetic-Legacy unit tests.
pub fn render_snapshot(
    data: &CollectedData,
    selected_entry: usize,
    show_tree: bool,
    show_hex: bool,
    show_runtime_hex: bool,
    gc_only: bool,
) -> String {
    let stats = &data.interpreter.gc.generation_stats;
    let rate = collections_rate_from_entries(&stats.entries, stats.has_timestamps);
    let avg = avg_collection_time_per_gen(&stats.entries, stats.has_duration);
    let lines = if gc_only {
        build_gc_only_lines(data, rate, avg, selected_entry)
    } else {
        build_lines(data, rate, avg, selected_entry, show_tree, show_hex, show_runtime_hex).0
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    use crate::tui::test_support::{join_lines, legacy_data};

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

    // ── build_lines / render_snapshot ─────────────────────────────

    #[test]
    fn build_lines_on_a_legacy_snapshot_skips_the_debug_offsets_section() {
        let data = legacy_data(true);
        let (lines, _entry_idx) = build_lines(&data, [None; 3], [None; 3], 0, true, true, false);
        let out = join_lines(&lines);
        // Pre-3.13 → no _Py_DebugOffsets section, straight to the legacy header + GC table.
        assert!(!out.contains("_Py_DebugOffsets (embedded"), "legacy must skip section 1: {out}");
        assert!(out.contains("pre-3.13: no _Py_DebugOffsets"), "{out}");
        assert!(out.contains("Gen 0 (Young) - 1 entries"), "{out}");
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
        assert!(out.contains("Gen 0 (Young) - 1 entries"), "{out}");
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
        entry: usize,
        entry_count: usize,
        glitch_active: bool,
        cl_active: bool,
        glitch_enabled: bool,
    ) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, 220, 1));
        status_bar(scroll, max_scroll, entry, entry_count, glitch_active, cl_active, glitch_enabled)
            .render(buf.area, &mut buf);
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn status_bar_shows_entry_position_and_the_100_percent_sentinel_when_unscrollable() {
        let out = render_status(0, 0, 0, 1, false, false, true);
        assert!(out.contains("[q] quit"), "{out}");
        assert!(out.contains("[p] pick pid"), "{out}");
        assert!(out.contains("entry 1/1"), "{out}");
        // max_scroll == 0 → checked_div is None → the "100%" branch.
        assert!(out.contains("100%"), "{out}");
        assert!(out.contains("[g] gc-view"), "gc-view mode hint: {out}");
        assert!(out.contains("glitch:on"), "glitch-enabled label: {out}");
    }

    #[test]
    fn status_bar_reflects_no_entries_disabled_glitch_and_the_badges() {
        assert!(render_status(0, 0, 0, 0, false, false, true).contains("no entries"));
        assert!(render_status(0, 0, 0, 1, false, false, false).contains("glitch:off"));
        // Connection-lost outranks the ordinary glitch badge.
        assert!(render_status(0, 0, 0, 1, true, true, true).contains("[CL]"));
        // The firing-glitch badge is `[FX]`, distinct from the `[G]` (mode) / `[G] glitch` keys.
        assert!(render_status(0, 0, 0, 1, true, false, true).contains("[FX]"));
    }
}
