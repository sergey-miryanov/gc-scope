//! The compact section 3 of the full layout: the per-generation summary + entry table on the
//! left, one selected entry's hexdump and decoded field box on the right. The whole-buffer
//! view ([`super::build_gc_buffer_view`]) shares this module tree's decode/format helpers.
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::remote_debugging::gc_stats::GcStat;
use crate::snapshot::collect::CollectedData;

use crate::tui::layout::{full_line, hex_dump_rows, l, padding_hex_right, span_line, top, PL, PR};

use super::{
    entry_field_color, entry_table_header, entry_table_row, field_highlights, format_field_value,
    gen_summary_lines, selected_entry_bytes,
};

pub(in crate::tui) fn section_gc_stats(data: &CollectedData, rate_per_gen: [Option<f64>; 3], avg_coll_time_per_gen: [Option<f64>; 3], selected_entry: usize) -> Vec<Line<'static>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::{join_lines, legacy_data};

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
}
