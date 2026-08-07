use crate::remote_debugging::offsets::offset_table::GcItemLayout;

/// One decoded GC generation-stats entry, as a lean **view** over the entry's raw bytes plus
/// the version's field layout — not a fixed struct enumerating every possible field.
///
/// The set of fields a entry carries is a property of the build (a regular build has only the
/// core counters; `+inc` and other custom builds add per-phase timestamps and sizes), so it
/// lives in the `GcItemLayout` (name → offset), not in named struct fields. Consumers read
/// fields by name through [`get`](Self::get)/[`iter_fields`](Self::iter_fields) (or the typed
/// convenience accessors for the always-present core). This is the single decode primitive the
/// Chrome exporter and the TUI right-side detail panel both use.
pub struct GcStat {
    pub generation: u32,
    pub index: usize,
    pub interpreter_id: i64,
    /// This entry's raw item bytes (`layout.item_size` long).
    bytes: Vec<u8>,
    /// The version's per-entry field layout, mapping each field name to its byte offset.
    layout: &'static GcItemLayout,
}

impl GcStat {
    /// Wrap an owned entry-bytes buffer. Used by the decoder, which slices the region into
    /// per-entry windows.
    pub fn new(
        generation: u32,
        index: usize,
        interpreter_id: i64,
        bytes: Vec<u8>,
        layout: &'static GcItemLayout,
    ) -> Self {
        Self {
            generation,
            index,
            interpreter_id,
            bytes,
            layout,
        }
    }

    /// Wrap a borrowed entry-byte window (copies it). For consumers that already hold the raw
    /// region — e.g. the TUI building a view over one selected entry.
    pub fn from_entry(
        bytes: &[u8],
        layout: &'static GcItemLayout,
        generation: u32,
        index: usize,
        interpreter_id: i64,
    ) -> Self {
        Self::new(generation, index, interpreter_id, bytes.to_vec(), layout)
    }

    /// The 8 little-endian bytes at `off` as a `u64`, or `None` if the entry is too short (a
    /// plausible teardown race — never panics, unlike a raw slice+`unwrap`).
    fn raw_at(&self, off: usize) -> Option<u64> {
        self.bytes
            .get(off..off + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// The `i64` value of `name`, or `None` if this build's layout lacks the field (or the
    /// entry is short). `None` — not `Some(0)` — is what marks a field genuinely absent.
    pub fn get(&self, name: &str) -> Option<i64> {
        self.raw_at(self.layout.field_offset(name)?)
            .map(|v| v as i64)
    }

    /// The `f64` value of `name` (e.g. `duration`), reinterpreting the raw bits.
    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.raw_at(self.layout.field_offset(name)?)
            .map(f64::from_bits)
    }

    /// Whether this build's layout defines `name`.
    pub fn has(&self, name: &str) -> bool {
        self.layout.has_field(name)
    }

    /// Whether this build publishes the timestamps that bound a collection.
    ///
    /// The tier selector, decided here and nowhere else: a build with these fields describes
    /// each collection as a span and can report a pause; one without publishes cumulative
    /// counts, so any pause figure from it would be fabricated. Keyed on the layout, never on
    /// a version (ADR 0003, ADR 0007, ADR 0017).
    pub fn has_timing(&self) -> bool {
        self.has("ts_start") && self.has("ts_stop")
    }

    /// Whether this entry describes a finished collection: `ts_start < ts_stop`.
    ///
    /// CPython publishes `ts_start` when a collection begins and `ts_stop` when it ends, so an
    /// entry read in between carries a fresh `ts_start` beside a `ts_stop` that is either zero
    /// or the stale value of the entry's previous occupant. Both read back as `ts_stop <=
    /// ts_start`, and so does a zero-width entry. The monitor's cursor and the TUI's
    /// `parse_gc_entries` share this one predicate.
    ///
    /// Gated on the layout, not the values: builds with no timestamp fields (inline, 3.8–3.14)
    /// cannot answer the question, so their entries all count as complete.
    pub fn is_complete(&self) -> bool {
        if !self.has_timing() {
            return true;
        }
        self.ts_start() < self.ts_stop()
    }

    /// Every field the layout defines, in layout order, as `(name, offset-within-entry, raw u64
    /// bits)`. The offset feeds the TUI's hex-highlight; the caller formats the bits by
    /// name (`duration` via `f64::from_bits`, `ts_*` as timestamps, large values as hex).
    pub fn iter_fields(&self) -> impl Iterator<Item = (&'static str, usize, u64)> + '_ {
        self.layout
            .fields
            .iter()
            .filter_map(move |&(name, off)| self.raw_at(off).map(|v| (name, off, v)))
    }

    // Typed convenience for the always-present core fields (dedup, summaries, exporter core).
    pub fn ts_start(&self) -> i64 {
        self.get("ts_start").unwrap_or(0)
    }
    pub fn ts_stop(&self) -> i64 {
        self.get("ts_stop").unwrap_or(0)
    }
    pub fn collections(&self) -> i64 {
        self.get("collections").unwrap_or(0)
    }
    pub fn collected(&self) -> i64 {
        self.get("collected").unwrap_or(0)
    }
    pub fn uncollectable(&self) -> i64 {
        self.get("uncollectable").unwrap_or(0)
    }
    pub fn candidates(&self) -> i64 {
        self.get("candidates").unwrap_or(0)
    }
    pub fn duration(&self) -> f64 {
        self.get_f64("duration").unwrap_or(0.0)
    }
    pub fn heap_size(&self) -> i64 {
        self.get("heap_size").unwrap_or(0)
    }
}

#[cfg(any(test, feature = "test-hooks"))]
impl GcStat {
    /// Build a stat by naming the fields to set (as `i64` little-endian), zero-filling the
    /// rest — the test analogue of the old `GcStat { field: v, ..Default::default() }`. Fields
    /// not in `layout` are ignored. Values go in as `i64`, so a `f64` field (`duration`) is set
    /// through its bits: `("duration", f64::to_bits(0.25) as i64)`.
    pub fn from_fields(
        generation: u32,
        index: usize,
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
        Self::new(generation, index, interpreter_id, bytes, layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::seq_layout;
    use std::sync::LazyLock;

    /// A standard build's entry layout — core counters only, no `+inc` extras.
    static REGULAR: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start", "collections", "collected"]));

    /// An extended (`+inc`) build's entry layout: the core counters plus the `increment_size`
    /// set a custom build adds. Carries the full set, so a decode test can assert that each
    /// field such a build publishes reads back.
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

    /// A view over a standard-set entry must report the extended fields as genuinely absent:
    /// `get` returns `None` (never `Some(0)` or a read past the field list), `has` is false,
    /// and `iter_fields` yields exactly the layout's own fields — the view can't fabricate a
    /// field the build doesn't have.
    #[test]
    fn reading_extra_fields_from_a_standard_entry_returns_none() {
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

    /// `iter_fields` is the TUI's hex-highlight feed, so it yields the full `(name,
    /// offset, raw u64 bits)` tuple — not just the name the other tests check. The offset is
    /// the field's byte position within the entry, and the bits are the exact little-endian
    /// contents: raw, NOT sign-interpreted the way `get` reads them (a `-1` entry reads back as
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

    /// A entry shorter than the layout (a teardown-race truncation) makes `iter_fields` skip
    /// the fields that would read past the end: `raw_at` returns `None` and the `filter_map`
    /// drops them, rather than panicking on an out-of-range slice.
    #[test]
    fn iter_fields_skips_fields_past_a_truncated_entry() {
        // REGULAR wants 24 bytes (fields at 0, 8, 16); give it only 16 so `collected`
        // (offset 16, needs bytes 16..24) can't be read.
        let s = GcStat::new(0, 0, 1, vec![0u8; 16], *REGULAR);

        let names: Vec<&str> = s.iter_fields().map(|(n, _, _)| n).collect();
        assert_eq!(names, ["ts_start", "collections"]);
        // The dropped field reads back `None` through `get` too — a short entry never panics.
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

    /// The monitor and the TUI both filter on `is_complete`, so it is pinned here rather than
    /// in either consumer: `ts_start < ts_stop` when the layout has both timestamps, true
    /// when it doesn't.
    #[test]
    fn is_complete_reads_ts_start_lt_ts_stop_when_the_layout_has_both() {
        let timed = seq_layout(&["ts_start", "ts_stop"]);
        let complete = |ts_start, ts_stop| {
            GcStat::from_fields(
                0,
                0,
                1,
                timed,
                &[("ts_start", ts_start), ("ts_stop", ts_stop)],
            )
            .is_complete()
        };

        assert!(complete(100, 150), "a finished collection");
        assert!(!complete(100, 0), "in flight: ts_stop not written yet");
        assert!(!complete(900, 400), "a previous occupant's stale ts_stop");
        assert!(!complete(100, 100), "zero-width counts as unfinished");
        assert!(!complete(0, 0), "an untouched entry");

        // No timestamps in the layout (3.8–3.14 inline builds): both accessors fall back to
        // zero, which must not read as permanently in-flight.
        let s = GcStat::from_fields(0, 0, 1, *REGULAR, &[("collections", 5)]);
        assert!(!s.has("ts_stop"));
        assert!(s.is_complete());
        // Same when only the start timestamp exists.
        let start_only = seq_layout(&["ts_start"]);
        assert!(GcStat::from_fields(0, 0, 1, start_only, &[("ts_start", 100)]).is_complete());
    }

    /// The extended print path reads each `+inc` field by name via `get`. A view over an
    /// extended entry must decode every one of them (not fall back to zero the way a core-only
    /// layout does), while the always-present core stays readable and `iter_fields` yields the
    /// full extended set in layout order.
    #[test]
    fn an_extended_entry_decodes_its_plus_inc_fields() {
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

        // The build is recognized as extended, and every `+inc` field decodes to its set
        // value — Some(v), never None or a zero fallback.
        assert!(s.has("increment_size"));
        assert_eq!(s.get("increment_size"), Some(100));
        assert_eq!(s.get("alive_size"), Some(200));
        assert_eq!(s.get("finalized_garbage_count"), Some(3));
        assert_eq!(s.get("clear_weakrefs_count"), Some(4));
        assert_eq!(s.get("deleted_garbage_count"), Some(5));
        assert_eq!(s.get("ts_mark_alive_start"), Some(999));

        // The core accessors still work on the same entry.
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
}
