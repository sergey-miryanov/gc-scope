//! The GC generation-stats views (all tiers). [`section_gc_stats`] is section 3 of the full
//! layout — the per-generation summary + entry table beside one selected entry's hexdump.
//! [`build_gc_only_lines`] is the `g`-toggled buffer view: the entry table over the selected
//! entry's fields, beside a hexdump of the *whole* stats buffer. The two decode and format the
//! same bytes, so their shared helpers live here between them.
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::remote_debugging::gc_stats::GcStat;
use crate::snapshot::collect::{CollectedData, GcEntry};

use super::format::{fmt_bytes, fmt_duration, fmt_rate, fmt_thousands};
use super::layout::{
    full_line, gc_two_col, hex_dump_rows, l, padding_hex_right, span_line, top, GC_PL, PL, PR,
};

// ── Section 3: GC Generation Stats ────────────────────────────────
pub(super) fn section_gc_stats(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3], selected_entry: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let gc = &data.interpreter.gc.generation_stats;

    lines.push(Line::from(Span::raw(top())));

    if gc.stats_addr == 0 || gc.entries.is_empty() {
        lines.push(Line::from(Span::raw(l("GC Generation Stats: not available"))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }

    lines.push(Line::from(Span::raw(l(&format!(
        "GC Generation Stats Buffer @ {:#x}  (size: {} bytes)",
        gc.stats_addr, gc.stats_size
    )))));
    lines.push(Line::from(Span::raw(top())));

    // Version-correct geometry/layout for this build (IO-free): drives the per-entry size,
    // the per-generation entry counts, and the entry-items field list below. Sourced from
    // the flat table the session already built, so it works for every tier (incl. Legacy).
    let offset_table = data.resolved.table().clone();
    let item_size = if gc.item_size > 0 { gc.item_size } else { gc.raw_stats_bytes.len().min(64) };
    // Per-generation entry counts come from the collected snapshot (version/layout-derived,
    // FT-correct) rather than a GIL-assuming literal or a per-frame tally.
    let entries_per_gen = gc.entries_per_gen;
    // Version-correct per-entry field layout (name → offset): drives both the hex-dump
    // highlights and the entry-items table. Using it (not the fixed ring layout) keeps the
    // 3.13/3.14 inline entries — collections@0/collected@8 — highlighted at the right bytes.
    let entry_fields: &[(&str, usize)] = offset_table.gc_layout.map(|l| l.fields).unwrap_or(&[]);

    // ── Left panel ──
    let mut left: Vec<String> = Vec::new();
    for line in gen_summary_lines(entries_per_gen, rate_per_gen, avg_coll_time_per_gen) {
        left.push(format!("{:<pl$}", line, pl = PL));
    }
    left.push(format!(
        "{:<pl$}",
        format!("entry size: {} bytes  |  total buffer: {} bytes", item_size, gc.stats_size),
        pl = PL
    ));
    left.push(format!("{:<pl$}", "", pl = PL));
    let hdr = entry_table_header();
    let hdr_len = hdr.len();
    left.push(format!("{:<pl$}", hdr, pl = PL));
    left.push(format!("  {}", "-".repeat(hdr_len - 2)));

    for entry in &gc.entries {
        left.push(entry_table_row(entry));
    }

    // ── Right panel ──
    let entry = &gc.entries[selected_entry];
    let entry_bytes = selected_entry_bytes(&gc.raw_stats_bytes, entry.byte_offset, item_size);
    let display_bytes = entry_bytes.len();
    // Decode this entry's fields through the shared `GcStat` primitive — the same by-name/offset
    // path the Chrome exporter uses — instead of re-reading the raw bytes inline here.
    let entry_view = offset_table
        .gc_layout
        .map(|l| GcStat::from_entry(entry_bytes, l, entry.generation, entry.index, 0));

    let mut right_items: Vec<Vec<Span<'static>>> = Vec::new();
    // Header
    right_items.push(vec![Span::raw(format!(
        "{:<pr$}",
        format!("Entry #{} (gen {}, entry {}) of stats buffer:", selected_entry + 1, entry.generation, entry.index),
        pr = PR
    ))]);

    // Hex dump of selected entry bytes, highlighting each present, colored field at its real
    // per-version offset. Deriving from the actual layout keeps the 3.13/3.14 inline entries
    // (collections@0, collected@8) from being painted at the ring offsets (16/24).
    let adjusted_highlights = field_highlights(&entry_view, entry.byte_offset);
    let hex_rows = hex_dump_rows(entry_bytes, display_bytes, &adjusted_highlights, entry.byte_offset);
    for hr in &hex_rows {
        right_items.push(padding_hex_right(hr.clone()));
    }

    // Entry field table (inner box)
    let dashes = PR - 12;
    let tw = dashes - 2;

    right_items.push(vec![Span::raw(format!("  +{}+", "-".repeat(dashes)))]);
    right_items.push(vec![Span::raw(format!(
        "  | {:<tw$} |",
        format!("GC Generation Stats Entry #{} (gen {}, entry {}) @ {:#x}",
            selected_entry + 1, entry.generation, entry.index,
            gc.stats_addr + entry.byte_offset as u64),
        tw = tw
    ))]);
    right_items.push(vec![Span::raw(format!("  +{}+", "-".repeat(dashes)))]);

    // Width the name column to the widest field this build actually has, so the `@ +offset`
    // and value columns stay aligned even for the long `+inc` names (e.g.
    // `ts_handle_weakref_callbacks_start`). Floored at 15 so short-field builds are unchanged.
    let name_w = entry_fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(15);

    for (name, offset, valbits) in entry_view.iter().flat_map(|v| v.iter_fields()) {
        let val_fmt = format_field_value(name, valbits);
        let content = format!("  {:<name_w$} @ +{:<4}  {}", name, offset, val_fmt, name_w = name_w);
        let color = entry_field_color(name);

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
    let selected_left_idx = 7 + selected_entry; // left items 0-6 are headers
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
fn entry_field_color(name: &str) -> Option<Color> {
    match name {
        "ts_start" | "ts_stop" => Some(Color::Blue),
        "collections" => Some(Color::Green),
        "collected" => Some(Color::Magenta),
        "duration" => Some(Color::Yellow),
        "heap_size" => Some(Color::Cyan),
        _ => None,
    }
}

// ── Shared GC-stats rendering helpers ─────────────────────────────
// The full view (`section_gc_stats`) and the buffer view (`build_gc_only_lines`) lay their
// panels out differently but decode and format the same bytes; these keep that shared logic in
// one place so the two can't drift.

/// The per-generation summary lines — entry count, collections rate, avg collection duration —
/// with `n/a` where the layout lacks the field. Unpadded; each view pads/wraps to its own width.
fn gen_summary_lines(
    entries_per_gen: [u64; 3],
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
) -> [String; 3] {
    const LABELS: [&str; 3] = ["Gen 0 (Young)", "Gen 1 (Middle)", "Gen 2 (Oldest)"];
    std::array::from_fn(|g| {
        let rate = match rate_per_gen[g] { Some(r) => fmt_rate(r), None => "n/a".to_string() };
        let coll = match avg_coll_time_per_gen[g] { Some(d) => fmt_duration(d), None => "n/a".to_string() };
        format!("{} - {} entries  (rate = {}, avg coll = {})", LABELS[g], entries_per_gen[g], rate, coll)
    })
}

/// The entry-table column header shared by both views' left tables.
fn entry_table_header() -> String {
    format!(
        "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11}",
        "gen", "idx", "collections", "collected", "heap", "duration(s)"
    )
}

/// One row of the entry table — same columns as [`entry_table_header`].
fn entry_table_row(entry: &GcEntry) -> String {
    format!(
        "  {:<5} {:>4}  {:>12}  {:>12}  {:>10}  {:>11.3}",
        entry.generation, entry.index, entry.collections, entry.collected,
        fmt_bytes(entry.heap_size as u64), entry.duration
    )
}

/// One entry's window into the raw stats buffer, clamped so a short/absent buffer yields an
/// empty slice instead of an out-of-range panic (`byte_offset + item_size` can exceed the
/// collected bytes when a request skipped the raw payload).
fn selected_entry_bytes(raw: &[u8], byte_offset: usize, item_size: usize) -> &[u8] {
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

/// Hex-dump highlights for a decoded entry's colored fields, each 8 bytes at its real
/// per-version offset (shifted by the entry's `byte_offset` into the buffer). Fields without a
/// color (uncollectable, candidates, the `+inc` extras) are left unhighlighted.
fn field_highlights(entry_view: &Option<GcStat>, byte_offset: usize) -> Vec<(usize, u8, Color)> {
    entry_view
        .iter()
        .flat_map(|v| v.iter_fields())
        .filter_map(|(name, off, _)| entry_field_color(name).map(|c| (off + byte_offset, 8u8, c)))
        .collect()
}

/// The byte offset of each generation's ring index in the raw buffer. CPython stores an `i8`
/// (plus 7 bytes of padding) right *after* each generation's entries — per
/// `compute_ring_base_offsets`, generation `g`'s entries start at `bases[g]`, so its index sits
/// at `bases[g] + entries[g] * item_size`. The index value is the active entry number for that
/// generation's ring. Empty for inline/legacy builds (one entry per generation, no index).
fn ring_index_offsets(
    table: &crate::remote_debugging::offsets::offset_table::OffsetTable,
    item_size: usize,
) -> Vec<usize> {
    use crate::remote_debugging::offsets::offset_table::GcStatsKind;
    if table.gc_stats_kind != GcStatsKind::RingBuffer || item_size == 0 {
        return Vec::new();
    }
    let (Some(bases), Some(entries)) = (table.gc_gen_base_offsets, table.gc_entries_per_gen) else {
        return Vec::new();
    };
    (0..3)
        .map(|g| bases[g] as usize + entries[g] as usize * item_size)
        .collect()
}

/// The GC-stats-only "buffer view" (`g` toggles it). Left column: the entry table (top,
/// arrow-selectable) over the selected entry's decoded field→value list (bottom). Right
/// column: a hexdump of the *whole* stats buffer — the selected entry's byte range shaded
/// `DarkGray` with its decoded fields colored in place, and (on ring builds) each generation's
/// `i8` ring index shaded `Red` as a fixed visual anchor, so the reader can watch the active
/// entry number change in place. Reuses the two-column box, the hex renderer, and
/// `entry_field_color` from the full view; the one difference from `section_gc_stats` is that
/// the hexdump spans the entire buffer, not one entry.
pub(super) fn build_gc_only_lines(
    data: &CollectedData,
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
    selected_entry: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let gc = &data.interpreter.gc.generation_stats;

    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l("GC Stats Buffer View  ([g] back to full layout)"))));
    lines.push(Line::from(Span::raw(top())));

    if gc.stats_addr == 0 || gc.entries.is_empty() {
        lines.push(Line::from(Span::raw(l("GC Generation Stats: not available"))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }

    let offset_table = data.resolved.table().clone();
    let item_size = if gc.item_size > 0 { gc.item_size } else { gc.raw_stats_bytes.len().min(64) };
    let entry_fields: &[(&str, usize)] = offset_table.gc_layout.map(|l| l.fields).unwrap_or(&[]);
    let selected = selected_entry.min(gc.entries.len() - 1);
    let sel = &gc.entries[selected];

    // Byte offsets of each generation's ring index (ring builds only), shaded in the hexdump
    // below as an anchor for reading the active entry number.
    let index_offsets = ring_index_offsets(&offset_table, item_size);

    // ── Left column ──
    let mut left: Vec<Vec<Span<'static>>> = Vec::new();
    left.push(vec![Span::raw(format!(
        "Buffer @ {:#x}  (size: {} bytes, entry: {} bytes)",
        gc.stats_addr, gc.stats_size, item_size
    ))]);
    // Per-generation summary — entry count, collections rate, and avg collection duration.
    for line in gen_summary_lines(gc.entries_per_gen, rate_per_gen, avg_coll_time_per_gen) {
        left.push(vec![Span::raw(line)]);
    }
    if !index_offsets.is_empty() {
        left.push(vec![
            Span::raw("legend: ".to_string()),
            Span::styled(" ring ", Style::new().bg(Color::Red).fg(Color::Black)),
            Span::raw(" = per-generation ring index (i8, points at the active entry)".to_string()),
        ]);
    }
    left.push(vec![Span::raw(String::new())]);

    // Entry table (top) — one row per entry, the selected row shaded.
    let hdr = entry_table_header();
    let hdr_len = hdr.len();
    left.push(vec![Span::raw(hdr)]);
    left.push(vec![Span::raw(format!("  {}", "-".repeat(hdr_len - 2)))]);
    for (i, entry) in gc.entries.iter().enumerate() {
        let content = entry_table_row(entry);
        if i == selected {
            left.push(vec![Span::styled(
                format!("{:<pl$}", content, pl = GC_PL),
                Style::new().bg(Color::DarkGray).fg(Color::White),
            )]);
        } else {
            left.push(vec![Span::raw(content)]);
        }
    }

    // Entry table (bottom) — the selected entry decoded field→value, each colored field shaded
    // to match the hexdump.
    let entry_bytes = selected_entry_bytes(&gc.raw_stats_bytes, sel.byte_offset, item_size);
    let entry_view = offset_table
        .gc_layout
        .map(|l| GcStat::from_entry(entry_bytes, l, sel.generation, sel.index, 0));

    left.push(vec![Span::raw(String::new())]);
    left.push(vec![Span::raw(format!(
        "Entry #{} (gen {}, entry {}) @ {:#x}",
        selected + 1, sel.generation, sel.index, gc.stats_addr + sel.byte_offset as u64
    ))]);
    let name_w = entry_fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0).max(12);
    for (name, offset, valbits) in entry_view.iter().flat_map(|v| v.iter_fields()) {
        let val_fmt = format_field_value(name, valbits);
        let content = format!("  {:<name_w$} @ +{:<4}  {}", name, offset, val_fmt, name_w = name_w);
        match entry_field_color(name) {
            Some(c) => left.push(vec![Span::styled(
                format!("{:<pl$}", content, pl = GC_PL),
                Style::new().bg(c).fg(Color::Black),
            )]),
            None => left.push(vec![Span::raw(content)]),
        }
    }

    // ── Right column: whole-buffer hexdump ──
    // `hex_dump_rows` takes the FIRST matching highlight, so order is priority: the selected
    // entry's colored fields, then its DarkGray whole-entry shade, then each generation's ring
    // index in Red. The index gaps never overlap a entry, so their order relative to the entry
    // shades doesn't matter. `item_size` is small (24 inline, the ring-struct size otherwise)
    // but capped to the `u8` highlight length.
    let mut highlights = field_highlights(&entry_view, sel.byte_offset);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::remote_debugging::offsets::pre_3_13;
    use crate::remote_debugging::session::Resolved;
    use crate::snapshot::collect::{GcStatsSnapshot, GcSubState, InterpreterSnapshot};
    use crate::tui::layout::OUTER_W;
    use crate::tui::test_support::{join_lines, legacy_data};

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
        let entries_per_gen = [11u64, 3, 3];
        let bases = compute_ring_base_offsets(item as u64, &entries_per_gen);
        let region = bases[2] as usize + entries_per_gen[2] as usize * item + 8;

        let mut raw = vec![0u8; region];
        for g in 0..3 {
            let idx_off = bases[g] as usize + entries_per_gen[g] as usize * item;
            raw[idx_off] = 0x5A;
        }

        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_kind = GcStatsKind::RingBuffer;
        table.gc_item_size = Some(item as u64);
        table.gc_entries_per_gen = Some(entries_per_gen);
        table.gc_gen_base_offsets = Some(bases);
        table.gc_layout = Some(&RING_LAYOUT);

        let entries = (0..3)
            .map(|g| GcEntry {
                generation: g as u32,
                index: 0,
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
                        entries_per_gen,
                        has_timestamps: true,
                        has_duration: false,
                        raw_stats_bytes: raw,
                        entries,
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

    #[test]
    fn section_gc_stats_renders_the_generation_table_with_na_summaries() {
        let data = legacy_data(true);
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats Buffer @ 0x7000"), "{out}");
        assert!(out.contains("Gen 0 (Young) - 1 entries"), "{out}");
        assert!(out.contains("entry size: 24 bytes"), "{out}");
        // None rate/avg must degrade to "n/a", not "0" or a panic.
        assert!(out.contains("n/a"), "{out}");
        // The one entry's decoded counters appear in the left table.
        assert!(out.contains("GC Generation Stats Entry #1"), "right-panel entry box: {out}");
    }

    #[test]
    fn section_gc_stats_reports_absent_stats_when_there_are_no_entries() {
        let data = legacy_data(false);
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats: not available"), "{out}");
    }

    /// If the raw stats buffer is empty (e.g. a request skipped it) but decoded `entries`
    /// remain, the right-panel byte slice must not panic even when the selected entry's
    /// `byte_offset` points past the (empty) buffer — the start clamp keeps `start <= end`.
    #[test]
    fn section_gc_stats_does_not_panic_when_raw_is_empty_but_a_entry_is_selected() {
        let mut data = legacy_data(true);
        let stats = &mut data.interpreter.gc.generation_stats;
        stats.raw_stats_bytes = Vec::new();
        stats.entries[0].byte_offset = 48; // a gen-1/2-style offset, past the empty buffer
        // Must render (empty hex panel) rather than slice-index panic.
        let out = join_lines(&section_gc_stats(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats Entry #1"), "{out}");
    }

    #[test]
    fn build_gc_only_lines_shows_the_entry_table_field_list_and_whole_buffer_hexdump() {
        let data = legacy_data(true);
        let lines = build_gc_only_lines(&data, [None; 3], [None; 3], 0);
        let out = join_lines(&lines);
        assert!(out.contains("GC Stats Buffer View"), "mode header: {out}");
        assert!(out.contains("Buffer @ 0x7000"), "buffer address line: {out}");
        // Left: the entry table row and the selected entry's decoded field list.
        assert!(out.contains("collections"), "field table: {out}");
        assert!(out.contains("Entry #1 (gen 0, entry 0)"), "selected-entry header: {out}");
        // Right: a whole-buffer hexdump — the 72-byte buffer spans past the first entry, so an
        // offset row beyond the first 16 bytes proves it dumps the whole buffer, not one entry.
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
        // No entries → the not-available short-circuit.
        let out = join_lines(&build_gc_only_lines(&legacy_data(false), [None; 3], [None; 3], 0));
        assert!(out.contains("GC Generation Stats: not available"), "{out}");
        // Entries but an empty raw buffer with a past-the-end offset must clamp, not panic.
        let mut data = legacy_data(true);
        let stats = &mut data.interpreter.gc.generation_stats;
        stats.raw_stats_bytes = Vec::new();
        stats.entries[0].byte_offset = 48;
        let out = join_lines(&build_gc_only_lines(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("Entry #1 (gen 0, entry 0)"), "{out}");
    }

    #[test]
    fn ring_index_offsets_point_just_past_each_generations_entries() {
        use crate::remote_debugging::offsets::offset_table::{compute_ring_base_offsets, GcStatsKind};

        // A GIL ring: entries [11, 3, 3], 24-byte items. Build the geometry from the public
        // fields (set_ring is private to the offsets module).
        let item = 24usize;
        let entries = [11u64, 3, 3];
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_kind = GcStatsKind::RingBuffer;
        table.gc_item_size = Some(item as u64);
        table.gc_entries_per_gen = Some(entries);
        table.gc_gen_base_offsets = Some(compute_ring_base_offsets(item as u64, &entries));
        let bases = table.gc_gen_base_offsets.unwrap();

        // Each index sits immediately after its generation's entries, i.e. 8 bytes before the
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

        // The per-generation summary header renders (entry counts + rate/avg, "n/a" where absent).
        let text = join_lines(&lines);
        assert!(text.contains("Gen 0 (Young) - 11 entries  (rate = 15.0/s, avg coll = n/a)"), "summary: {text}");
        assert!(text.contains("Gen 2 (Oldest) - 3 entries"), "gen-2 summary: {text}");

        // The legend appears (ring builds only) with a Red swatch.
        assert!(text.contains("per-generation ring index"), "legend text missing");
        assert!(
            lines.iter().flat_map(|l| &l.spans)
                .any(|s| s.style.bg == Some(Color::Red) && s.content.trim() == "ring"),
            "legend must carry a Red swatch"
        );

        // The one index byte of each generation (0x5A) is Red in the hexdump — exactly three,
        // one per generation, and the only 0x5A bytes in the otherwise-zero buffer.
        assert_eq!(red_bytes(&lines, "5a"), 3, "all three ring index bytes must be highlighted");

        // Selected-entry decoration still works alongside the index anchors: its `collections`
        // field keeps Green and its whole-entry range keeps the DarkGray shade.
        let spans = || lines.iter().flat_map(|l| &l.spans);
        assert!(spans().any(|s| s.style.bg == Some(Color::Green)), "field colours must survive");
        assert!(spans().any(|s| s.style.bg == Some(Color::DarkGray)), "selected-entry shade must survive");
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
}
