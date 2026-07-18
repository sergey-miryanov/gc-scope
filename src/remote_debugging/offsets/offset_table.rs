#![allow(dead_code)]

use crate::memory::reader;
use crate::remote_debugging::gc_stats::GcStat;

#[derive(Debug, Clone)]
pub struct OffsetTable {
    pub version_hex: u64,
    // _PyRuntime → interpreters.head
    pub runtime_interpreters_head: u64,
    // _PyRuntime → gc (3.8 only, otherwise per-interpreter)
    pub runtime_gc: Option<u64>,
    // PyInterpreterState fields
    pub interp_next: u64,
    pub interp_id: u64,
    pub interp_threads_head: u64,
    pub interp_gc: Option<u64>,
    // PyThreadState fields
    pub thread_interp: u64,
    // _gc_runtime_state fields (within the struct)
    pub gc_generations: u64,
    pub gc_collecting: u64,
    // 3.15+ only
    pub gc_frame: Option<u64>,
    // GC generation stats metadata (None = not available for this version)
    pub gc_stats_addr: Option<u64>,
    pub gc_item_size: Option<u64>,
    pub gc_slots_per_gen: Option<[u64; 3]>,
    pub gc_gen_base_offsets: Option<[u64; 3]>,
    /// If true, `gc_stats_addr` is relative to each interpreter's gc_state
    /// and must be recomputed per-interpreter. If false, it's an absolute
    /// address (dereferenced ring buffer pointer) valid for all interpreters.
    pub gc_stats_addr_is_per_interp: bool,
}

impl OffsetTable {
    pub fn runtime_interpreters_head(&self) -> u64 { self.runtime_interpreters_head }
    pub fn runtime_gc(&self) -> Option<u64> { self.runtime_gc }
    pub fn interp_next(&self) -> u64 { self.interp_next }
    pub fn interp_id(&self) -> u64 { self.interp_id }
    pub fn interp_threads_head(&self) -> u64 { self.interp_threads_head }
    pub fn interp_gc(&self) -> Option<u64> { self.interp_gc }
    pub fn thread_interp(&self) -> u64 { self.thread_interp }
    pub fn gc_generations(&self) -> u64 { self.gc_generations }
    pub fn gc_collecting(&self) -> u64 { self.gc_collecting }
    pub fn gc_frame(&self) -> Option<u64> { self.gc_frame }

    /// Panics if `interp_gc` is `None` (i.e. on Python 3.8).
    pub fn interp_gc_unwrap(&self) -> u64 {
        self.interp_gc.expect("interp_gc is not available on Python 3.8")
    }

    /// Panics if `runtime_gc` is `None` (i.e. on Python 3.9+).
    pub fn runtime_gc_unwrap(&self) -> u64 {
        self.runtime_gc.expect("runtime_gc is only available on Python 3.8")
    }

    /// Read GC generation stats from the target process.
    /// Returns empty vec if GC stats are not available for this version.
    pub fn read_gc_stats(&self, pid: u32, iid: i64) -> Vec<GcStat> {
        let addr = match self.gc_stats_addr {
            Some(a) => a,
            None => return vec![],
        };
        let item_size = self.gc_item_size.unwrap_or(0) as usize;
        if item_size == 0 { return vec![]; }
        let slots = match self.gc_slots_per_gen {
            Some(s) => s,
            None => return vec![],
        };
        let bases = match self.gc_gen_base_offsets {
            Some(b) => b,
            None => return vec![],
        };

        // total data = last gen's base + its slots
        let total = (bases[2] as usize) + (slots[2] as usize) * item_size;
        let raw = match reader::read_memory(pid, addr, total) {
            Ok(b) => b,
            Err(_) => return vec![],
        };

        let layout = match crate::remote_debugging::offsets::resolve_gc_item_layout(item_size) {
            Some(l) => l,
            None => return vec![],
        };

        let mut stats = Vec::new();
        for gidx in 0..3u32 {
            let base = bases[gidx as usize] as usize;
            let n = slots[gidx as usize] as usize;
            for slot in 0..n {
                let off = base + slot * item_size;
                macro_rules! opt {
                    ($name:expr) => {
                        layout.field_offset($name).map(|o| raw_i64(&raw, off + o))
                    };
                }
                stats.push(GcStat {
                    generation: gidx, slot, interpreter_id: iid,
                    ts_start: opt!("ts_start").unwrap_or(0),
                    ts_stop: opt!("ts_stop").unwrap_or(0),
                    collections: raw_i64(&raw, off + layout.field_offset("collections").unwrap()),
                    collected: raw_i64(&raw, off + layout.field_offset("collected").unwrap()),
                    uncollectable: raw_i64(&raw, off + layout.field_offset("uncollectable").unwrap()),
                    candidates: opt!("candidates").unwrap_or(0),
                    duration: layout.field_offset("duration").map(|o| raw_f64(&raw, off + o)).unwrap_or(0.0),
                    heap_size: opt!("heap_size").unwrap_or(0),
                    increment_size: opt!("increment_size"),
                    alive_size: opt!("alive_size"),
                    finalized_garbage_count: opt!("finalized_garbage_count"),
                    clear_weakrefs_count: opt!("clear_weakrefs_count"),
                    deleted_garbage_count: opt!("deleted_garbage_count"),
                    ts_mark_alive_start: opt!("ts_mark_alive_start"),
                    ts_mark_alive_stop: opt!("ts_mark_alive_stop"),
                    ts_fill_increment_start: opt!("ts_fill_increment_start"),
                    ts_fill_increment_stop: opt!("ts_fill_increment_stop"),
                    ts_deduce_unreachable_start: opt!("ts_deduce_unreachable_start"),
                    ts_deduce_unreachable_stop: opt!("ts_deduce_unreachable_stop"),
                    ts_handle_weakref_callbacks_start: opt!("ts_handle_weakref_callbacks_start"),
                    ts_handle_weakref_callbacks_stop: opt!("ts_handle_weakref_callbacks_stop"),
                    ts_finalize_garbage_stop: opt!("ts_finalize_garbage_stop"),
                    ts_handle_resurrected_stop: opt!("ts_handle_resurrected_stop"),
                    ts_clear_weakrefs_stop: opt!("ts_clear_weakrefs_stop"),
                    ts_delete_garbage_start: opt!("ts_delete_garbage_start"),
                    ts_delete_garbage_stop: opt!("ts_delete_garbage_stop"),
                });
            }
        }
        stats
    }
}

fn raw_i64(bytes: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}

fn raw_f64(bytes: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}

// ── DebugOffsetsLayout: positions within _Py_DebugOffsets ─────────

/// Maps field names to their byte positions WITHIN the `_Py_DebugOffsets` C struct.
/// Generated by `scripts/gen-offsets-table.py` and used at runtime to read actual values.
#[derive(Debug, Clone)]
pub struct DebugOffsetsLayout {
    pub version_hex: u64,
    pub runtime_interpreters_head: usize,
    pub interp_id: usize,
    pub interp_next: usize,
    pub interp_threads_head: usize,
    pub interp_gc: usize,
    pub thread_interp: usize,
    pub gc_size: usize,
    pub gc_collecting: usize,
    pub gc_frame: Option<usize>,
    pub gc_generation_stats_size: Option<usize>,
    pub gc_generation_stats: Option<usize>,
}

/// Read actual offset values from `_Py_DebugOffsets` in process memory,
/// using `layout` to know where each value is stored.
pub fn read_offset_table(pid: u32, runtime_addr: u64, layout: &DebugOffsetsLayout) -> OffsetTable {
    fn le_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    // Read enough of _Py_DebugOffsets to cover all needed fields
    let max_pos = [
        layout.gc_size,
        layout.gc_collecting,
        layout.runtime_interpreters_head,
        layout.interp_id,
        layout.interp_next,
        layout.interp_threads_head,
        layout.interp_gc,
        layout.thread_interp,
    ]
    .into_iter()
    .chain(layout.gc_frame.iter().copied())
    .chain(layout.gc_generation_stats.iter().copied())
    .chain(layout.gc_generation_stats_size.iter().copied())
    .max()
    .unwrap_or(576);

    let len = max_pos + 8;
    let raw = reader::read_memory(pid, runtime_addr, len).unwrap_or_else(|_| vec![0u8; len]);

    OffsetTable {
        version_hex: layout.version_hex,
        runtime_interpreters_head: le_u64(&raw, layout.runtime_interpreters_head),
        runtime_gc: None,
        interp_next: le_u64(&raw, layout.interp_next),
        interp_id: le_u64(&raw, layout.interp_id),
        interp_threads_head: le_u64(&raw, layout.interp_threads_head),
        interp_gc: Some(le_u64(&raw, layout.interp_gc)),
        thread_interp: le_u64(&raw, layout.thread_interp),
        gc_generations: 0x18,
        gc_collecting: le_u64(&raw, layout.gc_collecting),
        gc_frame: layout.gc_frame.map(|p| le_u64(&raw, p)),
        // GC stats not resolved here (the layout-based path is verify-only;
        // full GC stats go through VersionedOffsets bridge or table_for_version)
        gc_stats_addr: None,
        gc_item_size: None,
        gc_slots_per_gen: None,
        gc_gen_base_offsets: None,
        gc_stats_addr_is_per_interp: false,
    }
}

// ── GC generation stats item layout ─────────────────────────────

/// Describes the field layout of a single `gc_generation_stats` item.
/// Each generated bindgen file exports `GC_ITEM_SIZE` and `gc_field_names()`.
/// At runtime, the item size is computed from `generation_stats_size`,
/// and the matching layout is selected by size.
#[derive(Debug)]
pub struct GcItemLayout {
    pub item_size: usize,
    pub fields: &'static [(&'static str, usize)],
}

impl GcItemLayout {
    pub fn has_field(&self, name: &str) -> bool {
        self.fields.iter().any(|(n, _)| *n == name)
    }

    pub fn field_offset(&self, name: &str) -> Option<usize> {
        self.fields.iter().find(|(n, _)| *n == name).map(|(_, o)| *o)
    }
}

/// Compute gen_base_offsets for a ring-buffer GC stats layout.
/// `item_size` is bytes per slot, `slots` is `[young, old0, old1]`.
pub fn compute_ring_base_offsets(item_size: u64, slots: &[u64; 3]) -> [u64; 3] {
    [
        0,
        slots[0] * item_size + 8,
        slots[0] * item_size + 8 + slots[1] * item_size + 8,
    ]
}
