//! The GC generation-stats views (all tiers) and the decode/format helpers they share.
//! [`section_gc_stats`] (`compact`) is the compact section 3 of the full layout: the
//! per-generation summary and entry table, beside one selected entry's hexdump.
//! [`build_gc_buffer_view`] (`full`) is the `g`-toggled full view — the entry table over the
//! selected entry's fields, beside a hexdump of the *entire* stats buffer, and the surface
//! future GC widgets will hang off. The two lay their panels out differently but decode and
//! format the same bytes, so that shared logic lives here between them.
use ratatui::style::Color;

use crate::remote_debugging::gc_stats::GcStat;
use crate::snapshot::collect::GcEntry;

use super::format::{fmt_bytes, fmt_duration, fmt_rate, fmt_thousands};

mod compact;
mod full;

pub(super) use compact::section_gc_stats;
pub(super) use full::build_gc_buffer_view;

/// The color a GC-stat field is painted in, in both the hexdump highlights and the field
/// table. `None` = left unhighlighted (uncollectable, candidates, the `+inc` extras). Shared
/// by the compact view (`section_gc_stats`) and the full view (`build_gc_buffer_view`) so the
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

/// The per-generation summary lines — entry count, collections rate, avg collection duration —
/// with `n/a` where the layout lacks the field. Unpadded; each view pads/wraps to its own width.
fn gen_summary_lines(
    entries_per_gen: [u64; 3],
    rate_per_gen: [Option<f64>; 3],
    avg_coll_time_per_gen: [Option<f64>; 3],
) -> [String; 3] {
    const LABELS: [&str; 3] = ["Gen 0 (Young)", "Gen 1 (Middle)", "Gen 2 (Oldest)"];
    std::array::from_fn(|g| {
        let rate = match rate_per_gen[g] {
            Some(r) => fmt_rate(r),
            None => "n/a".to_string(),
        };
        let coll = match avg_coll_time_per_gen[g] {
            Some(d) => fmt_duration(d),
            None => "n/a".to_string(),
        };
        format!(
            "{} - {} entries  (rate = {}, avg coll = {})",
            LABELS[g], entries_per_gen[g], rate, coll
        )
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
        entry.generation,
        entry.index,
        entry.collections,
        entry.collected,
        fmt_bytes(entry.heap_size as u64),
        entry.duration
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::pre_3_13;
    use crate::tui::test_support::legacy_data;

    #[test]
    fn ring_index_offsets_point_just_past_each_generations_entries() {
        use crate::remote_debugging::offsets::offset_table::{
            GcStatsKind, compute_ring_base_offsets,
        };

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
        assert_eq!(
            offs[0],
            bases[1] as usize - 8,
            "gen-0 index is the 8-byte gap before gen 1"
        );
        assert_eq!(
            offs[1],
            bases[2] as usize - 8,
            "gen-1 index is the 8-byte gap before gen 2"
        );

        // Inline/Legacy is not a ring → no index gaps at all.
        let legacy = legacy_data(true);
        assert!(ring_index_offsets(legacy.resolved.table(), 24).is_empty());
    }
}
