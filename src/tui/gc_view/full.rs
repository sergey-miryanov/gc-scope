//! The `g`-toggled GC-stats buffer view: the entry table over the selected entry's decoded
//! fields on the left, a hexdump of the *whole* stats buffer on the right — the dedicated
//! surface future GC widgets will hang off. Shares this module tree's decode/format helpers
//! with the compact view ([`super::section_gc_stats`]).
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::remote_debugging::gc_stats::GcStat;
use crate::snapshot::collect::CollectedData;

use crate::tui::layout::{GC_PL, gc_two_col, hex_dump_rows, l, top};

use super::{
    entry_field_color, entry_table_header, entry_table_row, field_highlights, format_field_value,
    gen_summary_lines, ring_index_offsets, selected_entry_bytes,
};

/// The GC-stats-only "buffer view" (`g` toggles it). Left column: the entry table (top,
/// arrow-selectable) over the selected entry's decoded field→value list (bottom). Right
/// column: a hexdump of the *whole* stats buffer — the selected entry's byte range shaded
/// `DarkGray` with its decoded fields colored in place, and (on ring builds) each generation's
/// `i8` ring index shaded `Red` as a fixed visual anchor, so the reader can watch the active
/// entry number change in place. Reuses the two-column box, the hex renderer, and
/// `entry_field_color` from the compact view; the one difference from `section_gc_stats` is
/// that the hexdump spans the entire buffer, not one entry.
pub(in crate::tui) fn build_gc_buffer_view(
    data: &CollectedData,
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
    selected_entry: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let gc = &data.interpreter.gc.generation_stats;

    lines.push(Line::from(Span::raw(top())));
    lines.push(Line::from(Span::raw(l(
        "GC Stats Buffer View  ([g] back to full layout)",
    ))));
    lines.push(Line::from(Span::raw(top())));

    if gc.stats_addr == 0 || gc.entries.is_empty() {
        lines.push(Line::from(Span::raw(l(
            "GC Generation Stats: not available",
        ))));
        lines.push(Line::from(Span::raw(top())));
        return lines;
    }

    let offset_table = data.resolved.table().clone();
    let item_size = if gc.item_size > 0 {
        gc.item_size
    } else {
        gc.raw_stats_bytes.len().min(64)
    };
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
        selected + 1,
        sel.generation,
        sel.index,
        gc.stats_addr + sel.byte_offset as u64
    ))]);
    let name_w = entry_fields
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0)
        .max(12);
    for (name, offset, valbits) in entry_view.iter().flat_map(|v| v.iter_fields()) {
        let val_fmt = format_field_value(name, valbits);
        let content = format!(
            "  {:<name_w$} @ +{:<4}  {}",
            name,
            offset,
            val_fmt,
            name_w = name_w
        );
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
    let right = hex_dump_rows(
        &gc.raw_stats_bytes,
        gc.raw_stats_bytes.len(),
        &highlights,
        0,
    );

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
    use crate::snapshot::collect::{GcEntry, GcStatsSnapshot, GcSubState, InterpreterSnapshot};
    use crate::tui::layout::OUTER_W;
    use crate::tui::test_support::{join_lines, legacy_data};

    /// A synthetic ring snapshot (3.15-style geometry) built from the public `OffsetTable`
    /// fields. The buffer view reads only the table + stats (never the `Resolved` tier), so
    /// `Resolved::Legacy` is just the cheapest table wrapper that still drives the ring path.
    /// The buffer is all zero except each generation's index byte, set to `0x5A` so the Red
    /// index highlight can be found positionally in the hexdump.
    fn ring_data() -> CollectedData {
        use crate::remote_debugging::offsets::offset_table::{
            GcItemLayout, GcStatsKind, compute_ring_base_offsets,
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
    fn build_gc_buffer_view_shows_the_entry_table_field_list_and_whole_buffer_hexdump() {
        let data = legacy_data(true);
        let lines = build_gc_buffer_view(&data, [None; 3], [None; 3], 0);
        let out = join_lines(&lines);
        assert!(out.contains("GC Stats Buffer View"), "mode header: {out}");
        assert!(
            out.contains("Buffer @ 0x7000"),
            "buffer address line: {out}"
        );
        // Left: the entry table row and the selected entry's decoded field list.
        assert!(out.contains("collections"), "field table: {out}");
        assert!(
            out.contains("Entry #1 (gen 0, entry 0)"),
            "selected-entry header: {out}"
        );
        // Right: a whole-buffer hexdump — the 72-byte buffer spans past the first entry, so an
        // offset row beyond the first 16 bytes proves it dumps the whole buffer, not one entry.
        assert!(
            out.contains("00000010"),
            "hexdump must cover the whole buffer: {out}"
        );
        // No line overflows the fixed frame width.
        let border = format!("+{}+", "-".repeat(OUTER_W));
        assert!(
            out.lines().any(|l| l == border),
            "a full-width border must appear"
        );
        assert!(
            out.lines().all(|l| l.chars().count() <= OUTER_W + 2),
            "a line exceeded the frame width: {out}"
        );
    }

    #[test]
    fn build_gc_buffer_view_reports_absent_stats_and_never_panics_on_an_empty_buffer() {
        // No entries → the not-available short-circuit.
        let out = join_lines(&build_gc_buffer_view(
            &legacy_data(false),
            [None; 3],
            [None; 3],
            0,
        ));
        assert!(out.contains("GC Generation Stats: not available"), "{out}");
        // Entries but an empty raw buffer with a past-the-end offset must clamp, not panic.
        let mut data = legacy_data(true);
        let stats = &mut data.interpreter.gc.generation_stats;
        stats.raw_stats_bytes = Vec::new();
        stats.entries[0].byte_offset = 48;
        let out = join_lines(&build_gc_buffer_view(&data, [None; 3], [None; 3], 0));
        assert!(out.contains("Entry #1 (gen 0, entry 0)"), "{out}");
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
    fn build_gc_buffer_view_on_a_ring_highlights_every_index_byte_and_shows_the_legend() {
        let lines = build_gc_buffer_view(&ring_data(), [Some(15.0), None, None], [None; 3], 0);

        // The per-generation summary header renders (entry counts + rate/avg, "n/a" where absent).
        let text = join_lines(&lines);
        assert!(
            text.contains("Gen 0 (Young) - 11 entries  (rate = 15.0/s, avg coll = n/a)"),
            "summary: {text}"
        );
        assert!(
            text.contains("Gen 2 (Oldest) - 3 entries"),
            "gen-2 summary: {text}"
        );

        // The legend appears (ring builds only) with a Red swatch.
        assert!(
            text.contains("per-generation ring index"),
            "legend text missing"
        );
        assert!(
            lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.bg == Some(Color::Red) && s.content.trim() == "ring"),
            "legend must carry a Red swatch"
        );

        // The one index byte of each generation (0x5A) is Red in the hexdump — exactly three,
        // one per generation, and the only 0x5A bytes in the otherwise-zero buffer.
        assert_eq!(
            red_bytes(&lines, "5a"),
            3,
            "all three ring index bytes must be highlighted"
        );

        // Selected-entry decoration still works alongside the index anchors: its `collections`
        // field keeps Green and its whole-entry range keeps the DarkGray shade.
        let spans = || lines.iter().flat_map(|l| &l.spans);
        assert!(
            spans().any(|s| s.style.bg == Some(Color::Green)),
            "field colours must survive"
        );
        assert!(
            spans().any(|s| s.style.bg == Some(Color::DarkGray)),
            "selected-entry shade must survive"
        );
    }

    #[test]
    fn build_gc_buffer_view_on_an_inline_build_has_no_index_highlight_or_legend() {
        let lines = build_gc_buffer_view(&legacy_data(true), [None; 3], [None; 3], 0);
        assert!(
            !join_lines(&lines).contains("ring index"),
            "inline/legacy must show no legend"
        );
        assert!(
            !lines
                .iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.style.bg == Some(Color::Red)),
            "inline/legacy has no ring index to highlight"
        );
    }
}
