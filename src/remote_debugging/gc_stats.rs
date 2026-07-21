use crate::remote_debugging::offsets::offset_table::GcItemLayout;

/// One decoded GC generation-stats slot, as a lean **view** over the slot's raw bytes plus
/// the version's field layout — not a fixed struct enumerating every possible field.
///
/// The set of fields a slot carries is a property of the build (a regular build has only the
/// core counters; `+inc` and other custom builds add per-phase timestamps and sizes), so it
/// lives in the `GcItemLayout` (name → offset), not in named struct fields. Consumers read
/// fields by name through [`get`](Self::get)/[`iter_fields`](Self::iter_fields) (or the typed
/// convenience accessors for the always-present core). This is the single decode primitive the
/// Chrome exporter and the TUI right-side detail panel both use.
pub struct GcStat {
    pub generation: u32,
    pub slot: usize,
    pub interpreter_id: i64,
    /// This slot's raw item bytes (`layout.item_size` long).
    bytes: Vec<u8>,
    /// The version's per-slot field layout, mapping each field name to its byte offset.
    layout: &'static GcItemLayout,
}

impl GcStat {
    /// Wrap an owned slot-bytes buffer. Used by the decoder, which slices the region into
    /// per-slot windows.
    pub fn new(
        generation: u32,
        slot: usize,
        interpreter_id: i64,
        bytes: Vec<u8>,
        layout: &'static GcItemLayout,
    ) -> Self {
        Self { generation, slot, interpreter_id, bytes, layout }
    }

    /// Wrap a borrowed slot-byte window (copies it). For consumers that already hold the raw
    /// region — e.g. the TUI building a view over one selected slot.
    pub fn from_slot(
        bytes: &[u8],
        layout: &'static GcItemLayout,
        generation: u32,
        slot: usize,
        interpreter_id: i64,
    ) -> Self {
        Self::new(generation, slot, interpreter_id, bytes.to_vec(), layout)
    }

    /// The 8 little-endian bytes at `off` as a `u64`, or `None` if the slot is too short (a
    /// plausible teardown race — never panics, unlike a raw slice+`unwrap`).
    fn raw_at(&self, off: usize) -> Option<u64> {
        self.bytes.get(off..off + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// The `i64` value of `name`, or `None` if this build's layout lacks the field (or the
    /// slot is short). `None` — not `Some(0)` — is what marks a field genuinely absent.
    pub fn get(&self, name: &str) -> Option<i64> {
        self.raw_at(self.layout.field_offset(name)?).map(|v| v as i64)
    }

    /// The `f64` value of `name` (e.g. `duration`), reinterpreting the raw bits.
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.raw_at(self.layout.field_offset(name)?).map(f64::from_bits)
    }

    /// Whether this build's layout defines `name`.
    pub fn has(&self, name: &str) -> bool {
        self.layout.has_field(name)
    }

    /// Every field the layout defines, in layout order, as `(name, offset-within-slot, raw u64
    /// bits)`. The offset feeds the TUI's hex-highlight; the caller formats the bits by
    /// name (`duration` via `f64::from_bits`, `ts_*` as timestamps, large values as hex).
    pub fn iter_fields(&self) -> impl Iterator<Item = (&'static str, usize, u64)> + '_ {
        self.layout
            .fields
            .iter()
            .filter_map(move |&(name, off)| self.raw_at(off).map(|v| (name, off, v)))
    }

    // Typed convenience for the always-present core fields (dedup, summaries, exporter core).
    pub fn ts_start(&self) -> i64 { self.get("ts_start").unwrap_or(0) }
    pub fn ts_stop(&self) -> i64 { self.get("ts_stop").unwrap_or(0) }
    pub fn collections(&self) -> i64 { self.get("collections").unwrap_or(0) }
    pub fn collected(&self) -> i64 { self.get("collected").unwrap_or(0) }
    pub fn uncollectable(&self) -> i64 { self.get("uncollectable").unwrap_or(0) }
    pub fn candidates(&self) -> i64 { self.get("candidates").unwrap_or(0) }
    pub fn duration(&self) -> f64 { self.get_f64("duration").unwrap_or(0.0) }
    pub fn heap_size(&self) -> i64 { self.get("heap_size").unwrap_or(0) }
}

#[cfg(any(test, feature = "test-hooks"))]
impl GcStat {
    /// Build a stat by naming the fields to set (as `i64` little-endian), zero-filling the
    /// rest — the test analogue of the old `GcStat { field: v, ..Default::default() }`. Fields
    /// not in `layout` are ignored. `f64` fields (`duration`) aren't settable here; no test
    /// asserts a decoded duration, so this stays `i64`-only for simplicity.
    pub fn from_fields(
        generation: u32,
        slot: usize,
        interpreter_id: i64,
        layout: &'static GcItemLayout,
        fields: &[(&str, i64)],
    ) -> Self {
        let mut bytes = vec![0u8; layout.item_size];
        for &(name, val) in fields {
            if let Some(off) = layout.field_offset(name) {
                bytes[off..off + 8].copy_from_slice(&val.to_le_bytes());
            }
        }
        Self::new(generation, slot, interpreter_id, bytes, layout)
    }
}

/// Whether any slot comes from an extended (`+inc`) build. `increment_size` (and the rest of
/// the `+inc` set) is present in the layout only on such builds, so its presence in ANY slot
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
            "generation", "Slot", "IntID",
            "Collections", "Collected", "Uncollect.", "Candidates",
            "HeapSize", "Duration",
            "IncrSize", "AliveSize", "Finalized", "ClearWKRef",
            "DeletedGC", "MarkAlive"
        )
    } else {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10}",
            "generation", "Slot", "IntID",
            "Collections", "Collected", "Uncollect.", "Candidates",
            "HeapSize", "Duration"
        )
    }
}

/// Format one stats row. On the extended path the six `+inc` columns are appended, each read
/// with a zero fallback so a slot missing one (an absent field or a torn read) prints `0`
/// rather than dropping a column and misaligning the whole table. Pure — returns the line — so
/// the extended column layout and its fallbacks are unit-testable without capturing stdout.
fn format_row(s: &GcStat, has_extended: bool) -> String {
    if has_extended {
        format!(
            "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10.6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>14}",
            s.generation, s.slot, s.interpreter_id,
            s.collections(), s.collected(), s.uncollectable(),
            s.candidates(), s.heap_size(), s.duration(),
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
            s.generation, s.slot, s.interpreter_id,
            s.collections(), s.collected(), s.uncollectable(),
            s.candidates(), s.heap_size(), s.duration(),
        )
    }
}

pub fn print_stats(stats: &[GcStat]) {
    if stats.is_empty() {
        println!("No GC stats found.");
        return;
    }

    let has_extended = has_extended(stats);

    let header = format_header(has_extended);
    println!("{}", header);
    println!("{}", "-".repeat(header.len()));

    for s in stats {
        println!("{}", format_row(s, has_extended));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::seq_layout;
    use std::sync::LazyLock;

    /// A standard build's slot layout — core counters only, no `+inc` extras.
    static REGULAR: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start", "collections", "collected"]));

    /// An extended (`+inc`) build's slot layout — the core counters plus the `increment_size`
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

    /// A view over a standard-set slot must report the extended fields as genuinely absent:
    /// `get` returns `None` (never `Some(0)` or a read past the field list), `has` is false,
    /// and `iter_fields` yields exactly the layout's own fields — the view can't fabricate a
    /// field the build doesn't have.
    #[test]
    fn reading_extra_fields_from_a_standard_slot_returns_none() {
        let s = GcStat::from_fields(0, 0, 1, *REGULAR, &[("ts_start", 100), ("collections", 5)]);

        // Present fields decode normally.
        assert_eq!(s.ts_start(), 100);
        assert_eq!(s.collections(), 5);

        // Extended fields this layout lacks: absent, not fabricated.
        assert_eq!(s.get("increment_size"), None);
        assert_eq!(s.get("ts_mark_alive_start"), None);
        assert!(!s.has("increment_size"));
        // A core accessor for an absent field falls back to zero, not garbage.
        assert_eq!(s.heap_size(), 0);

        // `iter_fields` walks the layout, so it yields only the fields the build defines.
        let names: Vec<&str> = s.iter_fields().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["ts_start", "collections", "collected"]);
    }

    /// `print_stats` selects its wide `+inc` column set from `has_extended`, which fires when
    /// ANY slot carries `increment_size`. A slice mixing a core-only slot with one extended
    /// slot still counts as extended; an all-core (or empty) slice does not.
    #[test]
    fn has_extended_is_true_when_any_slot_carries_increment_size() {
        let core = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 1)]);
        let ext = GcStat::from_fields(0, 0, 1, *EXTENDED, &[("increment_size", 1)]);

        assert!(!has_extended(&[]), "empty slice is not extended");
        assert!(!has_extended(std::slice::from_ref(&core)), "core-only is not extended");
        assert!(has_extended(std::slice::from_ref(&ext)), "an extended slot is extended");
        // A mixed slice is extended as soon as one slot has the field.
        let core2 = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 2)]);
        let ext2 = GcStat::from_fields(0, 0, 1, *EXTENDED, &[("increment_size", 2)]);
        assert!(has_extended(&[core, ext, core2, ext2]));
    }

    /// `iter_fields` is the TUI's hex-highlight feed, so it yields the full `(name,
    /// offset, raw u64 bits)` tuple — not just the name the other tests check. The offset is
    /// the field's byte position within the slot, and the bits are the exact little-endian
    /// contents: raw, NOT sign-interpreted the way `get` reads them (a `-1` slot reads back as
    /// `u64::MAX` here but `Some(-1)` through `get`).
    #[test]
    fn iter_fields_yields_name_offset_and_raw_bits() {
        let s = GcStat::from_fields(
            0,
            0,
            1,
            *REGULAR,
            &[("ts_start", 0x1122), ("collections", -1), ("collected", 9)],
        );

        let fields: Vec<(&str, usize, u64)> = s.iter_fields().collect();
        assert_eq!(
            fields,
            [
                ("ts_start", 0, 0x1122u64),
                ("collections", 8, u64::MAX), // raw bits of -1i64, not a signed reading
                ("collected", 16, 9u64),
            ]
        );

        // `get` reinterprets those same bytes as i64 — the contrast that makes the raw-bits
        // contract matter.
        assert_eq!(s.get("collections"), Some(-1));
    }

    /// A slot shorter than the layout (a teardown-race truncation) makes `iter_fields` skip
    /// the fields that would read past the end: `raw_at` returns `None` and the `filter_map`
    /// drops them, rather than panicking on an out-of-range slice.
    #[test]
    fn iter_fields_skips_fields_past_a_truncated_slot() {
        // REGULAR wants 24 bytes (fields at 0, 8, 16); give it only 16 so `collected`
        // (offset 16, needs bytes 16..24) can't be read.
        let s = GcStat::new(0, 0, 1, vec![0u8; 16], *REGULAR);

        let names: Vec<&str> = s.iter_fields().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["ts_start", "collections"]);
        // The dropped field reads back `None` through `get` too — a short slot never panics.
        assert_eq!(s.get("collected"), None);
    }

    /// The typed core accessors each read their named field, falling back to zero when the
    /// build's layout lacks it. `ts_stop` in particular has no other coverage.
    #[test]
    fn typed_core_accessors_read_named_fields_with_a_zero_fallback() {
        let layout = seq_layout(&["ts_start", "ts_stop", "candidates"]);
        let s = GcStat::from_fields(
            0,
            0,
            1,
            layout,
            &[("ts_start", 10), ("ts_stop", 20), ("candidates", 30)],
        );

        assert_eq!(s.ts_start(), 10);
        assert_eq!(s.ts_stop(), 20);
        assert_eq!(s.candidates(), 30);

        // Fields absent from this layout fall back to zero, never a panic.
        assert_eq!(s.collected(), 0);
        assert_eq!(s.uncollectable(), 0);
        assert_eq!(s.heap_size(), 0);
    }

    /// The extended print path reads each `+inc` field by name via `get`. A view over an
    /// extended slot must decode every one of them (not fall back to zero the way a core-only
    /// layout does), while the always-present core stays readable and `iter_fields` yields the
    /// full extended set in layout order.
    #[test]
    fn an_extended_slot_decodes_its_plus_inc_fields() {
        let s = GcStat::from_fields(
            0,
            0,
            2,
            *EXTENDED,
            &[
                ("collections", 7),
                ("increment_size", 100),
                ("alive_size", 200),
                ("finalized_garbage_count", 3),
                ("clear_weakrefs_count", 4),
                ("deleted_garbage_count", 5),
                ("ts_mark_alive_start", 999),
            ],
        );

        // The build is recognized as extended, and each `+inc` field `print_stats` prints
        // decodes to its set value — Some(v), never None or a zero fallback.
        assert!(s.has("increment_size"));
        assert_eq!(s.get("increment_size"), Some(100));
        assert_eq!(s.get("alive_size"), Some(200));
        assert_eq!(s.get("finalized_garbage_count"), Some(3));
        assert_eq!(s.get("clear_weakrefs_count"), Some(4));
        assert_eq!(s.get("deleted_garbage_count"), Some(5));
        assert_eq!(s.get("ts_mark_alive_start"), Some(999));

        // The core accessors still work on the same slot.
        assert_eq!(s.collections(), 7);

        // `iter_fields` yields the full extended layout, in order.
        let names: Vec<&str> = s.iter_fields().map(|(n, _, _)| n).collect();
        assert_eq!(
            names,
            [
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
            ]
        );
    }

    /// The extended row (the `+inc` print path) must place all 15 columns —
    /// generation/slot/intid, the six core counters, then the six `+inc` fields — in order.
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
                "0", "1", "2", // generation, slot, interpreter_id
                "7", "8", "9", "10", "11", // collections..heap_size
                "0.000000", // duration (from_fields can't set an f64; stays 0.0)
                "100", "200", "3", "4", "5", "999", // the six +inc columns, in order
            ]
        );
        // The row carries exactly as many columns as the extended header.
        assert_eq!(cols.len(), format_header(true).split_whitespace().count());
    }

    /// On the extended path a slot whose layout is missing an `+inc` field (a torn read, or a
    /// partially-extended build) must still print that column as `0` via `.unwrap_or(0)` — the
    /// column stays present so the table never misaligns.
    #[test]
    fn extended_row_prints_zero_for_a_missing_plus_inc_field() {
        // Extended enough to take the wide path (`increment_size` present) but WITHOUT the
        // other `+inc` fields, so their `get(...)` returns None and must fall back to 0.
        let layout = seq_layout(&["collections", "increment_size"]);
        let s = GcStat::from_fields(0, 0, 0, layout, &[("collections", 1), ("increment_size", 42)]);

        let row = format_row(&s, true);
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(cols.len(), format_header(true).split_whitespace().count());
        // increment_size prints its value; the fields the layout lacks print 0, not garbage.
        assert_eq!(cols[9], "42"); // increment_size
        assert_eq!(&cols[10..], ["0", "0", "0", "0", "0"], "absent +inc fields fall back to 0");
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
