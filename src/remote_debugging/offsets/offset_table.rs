#![allow(dead_code)]

use anyhow::{Context, Result};
use read_process_memory::ProcessHandle;

use crate::memory::reader;
use crate::remote_debugging::gc_stats::GcStat;

/// Shape of a version's GC generation-stats region.
///
/// Set explicitly per version in `to_offset_table` so consumers never have to
/// re-infer the shape from magic item sizes (`24`/`40` == inline, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcStatsKind {
    /// No readable generation stats (3.13.x, pre-3.13).
    None,
    /// One slot per generation, contiguous at a fixed offset from the gc state
    /// (3.13.x, 3.14.4).
    InlineArray,
    /// Ring buffer reached via the `gc.generation_stats` pointer (3.15.0a8+).
    RingBuffer,
}

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
    /// Shape of the generation-stats region for this version.
    pub gc_stats_kind: GcStatsKind,
    /// Per-slot field layout, keyed by version (not by item size).
    pub gc_layout: Option<&'static GcItemLayout>,
    pub gc_stats_addr: Option<u64>,
    pub gc_item_size: Option<u64>,
    pub gc_slots_per_gen: Option<[u64; 3]>,
    pub gc_gen_base_offsets: Option<[u64; 3]>,
    /// For `InlineArray` kind: byte offset of `generation_stats[]` within each
    /// interpreter's `_gc_runtime_state`. Version-specific (3.13 = 0x80, 3.14 = 0x78)
    /// — computed per build by `scripts/gen-offsets.py`, not hardcoded.
    /// 0 for non-inline kinds.
    pub gc_stats_inline_off: u64,
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

    /// Read GC generation stats for one interpreter through an already-open handle.
    ///
    /// Returns `Ok(vec![])` for the legitimate "this build exposes no decodable
    /// stats" cases (no stats address, zero item size, no slot/base/layout info) —
    /// those are shape facts, not failures. A failed *read* of the stats buffer is
    /// a real error and propagates as `Err` (C6): the caller has already decided,
    /// via a non-NULL `gc_stats_addr`, that stats should be there.
    pub fn read_gc_stats(&self, handle: &ProcessHandle, iid: i64) -> Result<Vec<GcStat>> {
        let addr = match self.gc_stats_addr {
            Some(a) => a,
            None => return Ok(vec![]),
        };
        let item_size = self.gc_item_size.unwrap_or(0) as usize;
        if item_size == 0 { return Ok(vec![]); }
        let slots = match self.gc_slots_per_gen {
            Some(s) => s,
            None => return Ok(vec![]),
        };
        let bases = match self.gc_gen_base_offsets {
            Some(b) => b,
            None => return Ok(vec![]),
        };

        // total data = last gen's base + its slots
        let total = (bases[2] as usize) + (slots[2] as usize) * item_size;
        let raw = reader::read_memory_h(handle, addr, total)
            .with_context(|| format!("Failed to read gc_stats buffer at {addr:#x} ({total} bytes)"))?;

        let layout = match self.gc_layout {
            Some(l) => l,
            None => return Ok(vec![]),
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
        Ok(stats)
    }
}

fn raw_i64(bytes: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
}

fn raw_f64(bytes: &[u8], off: usize) -> f64 {
    f64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
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
