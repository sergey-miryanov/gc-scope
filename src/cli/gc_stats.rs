//! The `gc-stats` table: how a slice of decoded [`GcStat`] entries looks as text.
//!
//! Presentation, so it lives here rather than beside the decode primitive it consumes —
//! [ADR 0008](../../docs/adr/0008-reader-consumer-package-layering.md) keeps
//! `remote_debugging` free of consumer-shaped types. A second rendering of the same numbers
//! is therefore written as a peer consumer of that primitive, not against this printer.

use crate::remote_debugging::gc_stats::GcStat;

/// Whether any entry comes from an extended (`+inc`) build. `increment_size` (and the rest of
/// the `+inc` set) is present in the layout only on such builds, so its presence in ANY entry
/// selects `print_stats`' wider column set. Pulled out of `print_stats` so the column-selection
/// decision is unit-testable without capturing stdout.
fn has_extended(stats: &[GcStat]) -> bool {
    stats.iter().any(|s| s.has("increment_size"))
}

/// The column header, matched to the column set `has_extended` selects. Pure so the row
/// formatter's column count can be pinned against it without capturing stdout.
fn format_header(has_extended: bool) -> String {
    if has_extended {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14}",
            "generation",
            "Entry",
            "IntID",
            "Collections",
            "Collected",
            "Uncollect.",
            "Candidates",
            "HeapSize",
            "Duration",
            "IncrSize",
            "AliveSize",
            "Finalized",
            "ClearWKRef",
            "DeletedGC",
            "MarkAlive"
        )
    } else {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10}",
            "generation",
            "Entry",
            "IntID",
            "Collections",
            "Collected",
            "Uncollect.",
            "Candidates",
            "HeapSize",
            "Duration"
        )
    }
}

/// Format one stats row. On the extended path the six `+inc` columns are appended, each read
/// with a zero fallback so a entry missing one (an absent field or a torn read) prints `0`
/// rather than dropping a column and misaligning the whole table. Pure — returns the line — so
/// the extended column layout and its fallbacks are unit-testable without capturing stdout.
fn format_row(s: &GcStat, has_extended: bool) -> String {
    if has_extended {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10.6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14}",
            s.generation,
            s.index,
            s.interpreter_id,
            s.collections(),
            s.collected(),
            s.uncollectable(),
            s.candidates(),
            s.heap_size(),
            s.duration(),
            s.get("increment_size").unwrap_or(0),
            s.get("alive_size").unwrap_or(0),
            s.get("finalized_garbage_count").unwrap_or(0),
            s.get("clear_weakrefs_count").unwrap_or(0),
            s.get("deleted_garbage_count").unwrap_or(0),
            s.get("ts_mark_alive_start").unwrap_or(0),
        )
    } else {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10.6}",
            s.generation,
            s.index,
            s.interpreter_id,
            s.collections(),
            s.collected(),
            s.uncollectable(),
            s.candidates(),
            s.heap_size(),
            s.duration(),
        )
    }
}

/// The whole table as lines, ready to print. Pure for the same reason the row and header
/// formatters are: it puts the parts of the table that are not a single row — the
/// empty-slice message, the separator's width, the order the lines come in — behind an
/// assertion instead of behind stdout, which no test can read back.
fn render(stats: &[GcStat]) -> Vec<String> {
    if stats.is_empty() {
        return vec!["No GC stats found.".to_string()];
    }

    let has_extended = has_extended(stats);

    let header = format_header(has_extended);
    let separator = "-".repeat(header.len());

    let mut lines = Vec::with_capacity(stats.len() + 2);
    lines.push(header);
    lines.push(separator);
    lines.extend(stats.iter().map(|s| format_row(s, has_extended)));
    lines
}

pub fn print_stats(stats: &[GcStat]) {
    for line in render(stats) {
        println!("{}", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A standard build's entry layout — core counters only, no `+inc` extras.
    static REGULAR: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start", "collections", "collected"]));

    /// An extended (`+inc`) build's entry layout — the core counters plus the `increment_size`
    /// set that `print_stats` widens its columns for. Exactly the field names `print_stats`
    /// reads on the extended path.
    static EXTENDED: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "collections",
            "collected",
            "uncollectable",
            "candidates",
            "heap_size",
            "increment_size",
            "alive_size",
            "finalized_garbage_count",
            "clear_weakrefs_count",
            "deleted_garbage_count",
            "ts_mark_alive_start",
        ])
    });

    /// `print_stats` selects its wide `+inc` column set from `has_extended`, which fires when
    /// ANY entry carries `increment_size`. A slice mixing a core-only entry with one extended
    /// entry still counts as extended; an all-core (or empty) slice does not.
    #[test]
    fn has_extended_is_true_when_any_entry_carries_increment_size() {
        let core = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 1)]);
        let ext = GcStat::from_fields(0, 0, 1, *EXTENDED, &[("increment_size", 1)]);

        assert!(!has_extended(&[]), "empty slice is not extended");
        assert!(
            !has_extended(std::slice::from_ref(&core)),
            "core-only is not extended"
        );
        assert!(
            has_extended(std::slice::from_ref(&ext)),
            "an extended entry is extended"
        );
        // A mixed slice is extended as soon as one entry has the field.
        let core2 = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 2)]);
        let ext2 = GcStat::from_fields(0, 0, 1, *EXTENDED, &[("increment_size", 2)]);
        assert!(has_extended(&[core, ext, core2, ext2]));
    }

    /// The extended row (the `+inc` print path) must place all 15 columns —
    /// generation/entry/intid, the six core counters, then the six `+inc` fields — in order.
    /// A wrong order or a dropped `.unwrap_or` would silently print a value under the wrong
    /// header; splitting the row on whitespace pins the exact column contents.
    #[test]
    fn extended_row_lays_out_every_plus_inc_column_in_order() {
        let s = GcStat::from_fields(
            0,
            1,
            2,
            *EXTENDED,
            &[
                ("collections", 7),
                ("collected", 8),
                ("uncollectable", 9),
                ("candidates", 10),
                ("heap_size", 11),
                ("increment_size", 100),
                ("alive_size", 200),
                ("finalized_garbage_count", 3),
                ("clear_weakrefs_count", 4),
                ("deleted_garbage_count", 5),
                ("ts_mark_alive_start", 999),
            ],
        );

        let row = format_row(&s, true);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            cols,
            [
                "0", "1", "2", // generation, entry, interpreter_id
                "7", "8", "9", "10", "11",       // collections..heap_size
                "0.000000", // duration (from_fields can't set an f64; stays 0.0)
                "100", "200", "3", "4", "5", "999", // the six +inc columns, in order
            ]
        );
        // The row carries exactly as many columns as the extended header.
        assert_eq!(cols.len(), format_header(true).split_whitespace().count());
    }

    /// On the extended path a entry whose layout is missing an `+inc` field (a torn read, or a
    /// partially-extended build) must still print that column as `0` via `.unwrap_or(0)` — the
    /// column stays present so the table never misaligns.
    #[test]
    fn extended_row_prints_zero_for_a_missing_plus_inc_field() {
        // Extended enough to take the wide path (`increment_size` present) but WITHOUT the
        // other `+inc` fields, so their `get(...)` returns None and must fall back to 0.
        let layout = seq_layout(&["collections", "increment_size"]);
        let s = GcStat::from_fields(
            0,
            0,
            0,
            layout,
            &[("collections", 1), ("increment_size", 42)],
        );

        let row = format_row(&s, true);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols.len(), format_header(true).split_whitespace().count());
        // increment_size prints its value; the fields the layout lacks print 0, not garbage.
        assert_eq!(cols[9], "42"); // increment_size
        assert_eq!(
            &cols[10..],
            ["0", "0", "0", "0", "0"],
            "absent +inc fields fall back to 0"
        );
    }

    /// An empty slice — the ordinary answer from a target that has not collected yet, and
    /// from every 3.8 build — renders the message and nothing else. No header, no separator,
    /// no blank trailing line: a bare ruler under a bare header would read as "the table is
    /// there but the values failed to decode", which is a different diagnosis.
    #[test]
    fn an_empty_slice_renders_only_the_not_found_message() {
        assert_eq!(render(&[]), ["No GC stats found."]);
    }

    /// The table's non-row parts, which no per-row test reaches: a header, then a separator
    /// exactly as wide as that header, then one row per entry in the order given. The width
    /// tie is the assertion that matters — the ruler is built from `header.len()`, so a
    /// header change that forgot it would print a rule that stops short of the columns.
    #[test]
    fn the_table_is_a_header_then_a_matching_rule_then_one_row_per_entry() {
        let stats = [
            GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 5)]),
            GcStat::from_fields(1, 0, 1, *REGULAR, &[("collections", 6)]),
            GcStat::from_fields(2, 0, 1, *REGULAR, &[("collections", 7)]),
        ];

        let lines = render(&stats);

        assert_eq!(lines.len(), stats.len() + 2, "header + rule + one row each");
        assert_eq!(lines[0], format_header(false));
        assert_eq!(lines[1].len(), lines[0].len(), "the rule spans the header");
        assert!(lines[1].chars().all(|c| c == '-'));
        // Rows keep the order they were given, so generation 2 is not printed above 0.
        let collections: Vec<&str> = lines[2..]
            .iter()
            .map(|l| l.split_whitespace().nth(3).unwrap())
            .collect();
        assert_eq!(collections, ["5", "6", "7"]);
    }

    /// The core (non-extended) row is the 9-column subset — no `+inc` columns — matching the
    /// non-extended header.
    #[test]
    fn core_row_has_only_the_nine_base_columns() {
        let s = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 5)]);
        let row = format_row(&s, false);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols.len(), 9);
        assert_eq!(cols.len(), format_header(false).split_whitespace().count());
        assert_eq!(cols[3], "5"); // collections
    }
}
