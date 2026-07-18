use crate::remote_debugging::offsets::offset_table::OffsetTable;

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
        gc_stats_addr: None,  // filled by caller using GC_STATS_INLINE_OFF + gc_state_addr
        gc_item_size: Some(GC_ITEM_SIZE),
        gc_slots_per_gen: Some(GC_SLOTS),
        gc_gen_base_offsets: Some(GC_BASES),
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

/// Python 3.8: GC is global in `_PyRuntime`, not per-interpreter.
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
