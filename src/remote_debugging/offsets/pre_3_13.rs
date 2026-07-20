use crate::remote_debugging::offsets::offset_table::{GcItemLayout, GcStatsKind, OffsetTable};

/// Offset of `generation_stats` within `_gc_runtime_state` (identical 3.8–3.13).
pub const GC_STATS_INLINE_OFF: u64 = 0x80;
/// Size of a single `gc_generation_stats` struct (3 × Py_ssize_t = 24 bytes).
pub const GC_ITEM_SIZE: u64 = 24;
/// Slot count for inline array: one per generation, no ring buffer.
pub const GC_SLOTS: [u64; 3] = [1, 1, 1];
/// Base offsets for each generation in the inline array.
pub const GC_BASES: [u64; 3] = [0, 24, 48];
/// Offset of `collecting` within `_gc_runtime_state`.
pub const GC_COLLECTING: u64 = 0xC8;

/// Field layout of one `gc_generation_stats` item — identical across 3.8–3.13
/// (`collections`, `collected`, `uncollectable`, each a `Py_ssize_t`). Hand-written
/// because pre-3.13 has no bindgen struct to derive it from via `offset_of!`; this
/// mirrors the generated `GC_LAYOUT` in the `v_*` modules and lets the shared
/// `InlineArray` decode path in `PySession::gc_stats` handle Legacy interpreters.
pub static LEGACY_GC_LAYOUT: GcItemLayout = GcItemLayout {
    item_size: GC_ITEM_SIZE as usize,
    fields: &[
        ("collections", 0),
        ("collected", 8),
        ("uncollectable", 16),
    ],
};

// A private constructor for the hand-extracted legacy tables: it fills the ~20 constant
// `OffsetTable` fields (identical across 3.8–3.12) so the per-version call sites below
// only supply the nine that vary — each one raw byte offset from a CPython header.
//
// The nine are passed positionally, kept as a compact table of magic numbers. The honest
// tradeoff of the `#[allow]`: a named-field struct would make the trailing `// label`
// comments compiler-checked against transposition — a real gain for correctness-critical
// offsets. It is not taken because (a) any transposition is caught end-to-end by the
// 3.8–3.12 live-smoke legs, and (b) the aligned-comment table is the clearer form for
// hand-extracted offsets. If the live coverage ever narrows, revisit this.
#[allow(clippy::too_many_arguments)]
fn table(version_hex: u64, runtime_ih: u64, interp_next: u64, interp_id: u64,
         interp_ts_head: u64, interp_gc: Option<u64>, thread_interp: u64,
         gc_gen: u64, runtime_gc: Option<u64>) -> OffsetTable {
    OffsetTable {
        version_hex,
        runtime_interpreters_head: runtime_ih,
        runtime_gc,
        interp_next,
        interp_id,
        interp_threads_head: interp_ts_head,
        interp_gc,
        thread_interp,
        gc_generations: gc_gen,
        gc_collecting: GC_COLLECTING,
        gc_frame: None,
        // The `gc_generation_stats` item and the inline `generation_stats[]` position
        // are identical to 3.13, so pre-3.13 decodes through the same `InlineArray`
        // path in `PySession::gc_stats`. (3.8 keeps GC global in `_PyRuntime` rather
        // than per-interpreter; the stats loop's global-GC branch resolves that from
        // `runtime_gc`, so 3.8 decodes through this same `InlineArray` layout too.)
        gc_stats_kind: GcStatsKind::InlineArray,
        gc_layout: Some(&LEGACY_GC_LAYOUT),
        gc_stats_addr: None,  // filled per-interpreter by the stats loop (gc_state + GC_STATS_INLINE_OFF)
        gc_item_size: Some(GC_ITEM_SIZE),
        gc_slots_per_gen: Some(GC_SLOTS),
        gc_gen_base_offsets: Some(GC_BASES),
        gc_stats_inline_off: GC_STATS_INLINE_OFF,
        gc_stats_addr_is_per_interp: true,
    }
}

/// Try to resolve a pre-3.13 `OffsetTable` from the (major, minor) version.
/// Returns `None` for unsupported versions.
pub fn table_for_version(major: u8, minor: u8) -> Option<OffsetTable> {
    let version_hex = (major as u64) << 24 | (minor as u64) << 16;
    match (major, minor) {
        (3, 8)  => Some(v3_8(version_hex)),
        (3, 9)  => Some(v3_9(version_hex)),
        (3, 10) => Some(v3_10(version_hex)),
        (3, 11) => Some(v3_11(version_hex)),
        (3, 12) => Some(v3_12(version_hex)),
        _       => None,
    }
}

// ── Per-version tables ────────────────────────────────────────────

/// Python 3.8: GC is global in `_PyRuntime` (`runtime_gc`), not per-interpreter.
/// The stats loop's global-GC branch resolves the stats region from
/// `runtime_addr + runtime_gc + gc_stats_inline_off`, so the shared `InlineArray`
/// decode applies unchanged.
fn v3_8(version_hex: u64) -> OffsetTable {
    table(
        version_hex,
        0x20,    // runtime_interpreters_head
        0x00,    // interp_next
        0x10,    // interp_id
        0x08,    // interp_tstate_head
        None,    // interp_gc (global GC)
        0x10,    // thread_interp
        0x18,    // gc_generations
        Some(0x158), // runtime_gc
    )
}

/// Python 3.9: GC is per-interpreter at offset 0x268.
fn v3_9(version_hex: u64) -> OffsetTable {
    table(
        version_hex,
        0x20,    // runtime_interpreters_head
        0x00,    // interp_next
        0x18,    // interp_id
        0x08,    // interp_tstate_head
        Some(0x268), // interp_gc
        0x10,    // thread_interp
        0x18,    // gc_generations
        None,    // runtime_gc
    )
}

/// Python 3.10: same layout as 3.9.
fn v3_10(version_hex: u64) -> OffsetTable {
    v3_9(version_hex)
}

/// Python 3.11: `threads.head` at new offset.
fn v3_11(version_hex: u64) -> OffsetTable {
    table(
        version_hex,
        0x28,    // runtime_interpreters_head
        0x00,    // interp_next
        0x30,    // interp_id
        0x10,    // interp_threads_head (threads.head at offset 0x10)
        Some(0x288), // interp_gc
        0x10,    // thread_interp
        0x18,    // gc_generations
        None,    // runtime_gc
    )
}

/// Python 3.12: `threads.head` nested deeper, `id` at 0x08.
fn v3_12(version_hex: u64) -> OffsetTable {
    table(
        version_hex,
        0x28,    // runtime_interpreters_head
        0x00,    // interp_next
        0x08,    // interp_id
        0x48,    // interp_threads_head
        Some(0x70), // interp_gc
        0x10,    // thread_interp
        0x18,    // gc_generations
        None,    // runtime_gc
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::version::PythonVersion;

    const SUPPORTED: [(u8, u8); 5] = [(3, 8), (3, 9), (3, 10), (3, 11), (3, 12)];

    #[test]
    fn covers_exactly_3_8_through_3_12() {
        for (major, minor) in SUPPORTED {
            assert!(
                table_for_version(major, minor).is_some(),
                "{major}.{minor} should have a legacy table"
            );
        }
        // 3.7 predates support; 3.13+ goes through the bindgen `LAYOUTS` registry.
        for (major, minor) in [(3, 7), (3, 13), (3, 16), (4, 0), (2, 7)] {
            assert!(
                table_for_version(major, minor).is_none(),
                "{major}.{minor} must not resolve to a legacy table"
            );
        }
    }

    /// 3.8 keeps GC state global in `_PyRuntime`; 3.9+ moved it per-interpreter.
    /// This single bit routes the global-GC branch in `PySession::gc_stats`, and
    /// the two branches do different address math (ADR 0003) — so getting it wrong
    /// reads a valid-but-wrong address and returns garbage rather than failing.
    #[test]
    fn only_3_8_has_global_gc_state() {
        let t38 = table_for_version(3, 8).unwrap();
        assert!(t38.runtime_gc.is_some(), "3.8 GC state lives in _PyRuntime");
        assert!(t38.interp_gc.is_none(), "3.8 has no per-interpreter GC state");

        for (major, minor) in [(3, 9), (3, 10), (3, 11), (3, 12)] {
            let t = table_for_version(major, minor).unwrap();
            assert!(t.runtime_gc.is_none(), "{major}.{minor} must not use the global-GC branch");
            assert!(t.interp_gc.is_some(), "{major}.{minor} has per-interpreter GC state");
        }
    }

    /// Every legacy build decodes through the SAME inline `gc_generation_stats`
    /// layout as 3.13/3.14 — that shared path is why `pre_3_13.rs` needs no decode
    /// logic of its own.
    #[test]
    fn all_legacy_tables_share_the_inline_stats_shape() {
        for (major, minor) in SUPPORTED {
            let t = table_for_version(major, minor).unwrap();
            let at = format!("{major}.{minor}");
            assert_eq!(t.gc_stats_kind, GcStatsKind::InlineArray, "{at}");
            assert_eq!(t.gc_item_size, Some(24), "{at}");
            assert_eq!(t.gc_slots_per_gen, Some([1, 1, 1]), "{at}");
            assert_eq!(t.gc_gen_base_offsets, Some([0, 24, 48]), "{at}");
            assert_eq!(t.gc_stats_inline_off, 0x80, "{at}");
            assert!(t.gc_layout.is_some(), "{at}");
            assert!(t.gc_stats_addr_is_per_interp, "{at}");
            // Filled in per-interpreter by the stats loop, never baked into the table.
            assert_eq!(t.gc_stats_addr, None, "{at}");
            // 3.15-only field.
            assert_eq!(t.gc_frame, None, "{at}");
        }
    }

    #[test]
    fn version_hex_round_trips_to_the_requested_version() {
        for (major, minor) in SUPPORTED {
            let t = table_for_version(major, minor).unwrap();
            let parsed = PythonVersion::from_hex(t.version_hex).unwrap();
            assert_eq!((parsed.major, parsed.minor), (major, minor));
        }
    }

    #[test]
    fn legacy_layout_exposes_exactly_the_three_pre_3_13_fields() {
        assert_eq!(LEGACY_GC_LAYOUT.item_size, GC_ITEM_SIZE as usize);
        assert_eq!(LEGACY_GC_LAYOUT.field_offset("collections"), Some(0));
        assert_eq!(LEGACY_GC_LAYOUT.field_offset("collected"), Some(8));
        assert_eq!(LEGACY_GC_LAYOUT.field_offset("uncollectable"), Some(16));
        // Fields introduced in later builds must be absent, so the decoder leaves
        // their `Option`s as None rather than reading past the 24-byte item.
        for absent in ["ts_start", "duration", "heap_size", "increment_size"] {
            assert!(!LEGACY_GC_LAYOUT.has_field(absent), "{absent} must not be in the legacy layout");
        }
    }
}
