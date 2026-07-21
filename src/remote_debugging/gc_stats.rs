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
    /// region — e.g. the diagram building a view over one selected slot.
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
    /// bits)`. The offset feeds the diagram's hex-highlight; the caller formats the bits by
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

pub fn print_stats(stats: &[GcStat]) {
    if stats.is_empty() {
        println!("No GC stats found.");
        return;
    }

    let has_extended = has_extended(stats);

    let header = if has_extended {
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
    };

    println!("{}", header);
    println!("{}", "-".repeat(header.len()));

    for s in stats {
        if has_extended {
            println!(
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
            );
        } else {
            println!(
                "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10.6}",
                s.generation, s.slot, s.interpreter_id,
                s.collections(), s.collected(), s.uncollectable(),
                s.candidates(), s.heap_size(), s.duration(),
            );
        }
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
}
