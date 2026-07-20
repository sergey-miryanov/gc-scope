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
    /// No readable generation stats — e.g. Python 3.8 (GC state is global, not yet
    /// decoded) or a build whose per-slot GC layout wasn't generated.
    None,
    /// One slot per generation, contiguous at a fixed offset from the gc state.
    /// The same inline layout spans pre-3.13 (3.9–3.12) and 3.13.x / 3.14.4.
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
        let total = match self.stats_buffer_len() {
            Some(t) => t,
            None => return Ok(vec![]),
        };
        let raw = reader::read_memory_h(handle, addr, total)
            .with_context(|| format!("Failed to read gc_stats buffer at {addr:#x} ({total} bytes)"))?;
        Ok(self.decode_gc_stats(&raw, iid))
    }

    /// Byte length of one interpreter's stats region — the last generation's base
    /// plus its slots. `None` when this build exposes no decodable stats (those are
    /// shape facts, not failures; see [`Self::read_gc_stats`]).
    fn stats_buffer_len(&self) -> Option<usize> {
        let item_size = self.gc_item_size? as usize;
        if item_size == 0 {
            return None;
        }
        let slots = self.gc_slots_per_gen?;
        let bases = self.gc_gen_base_offsets?;
        self.gc_layout?;
        Some((bases[2] as usize) + (slots[2] as usize) * item_size)
    }

    /// Decode an already-read stats buffer. Pure — no process access — so the
    /// per-version slot geometry and field offsets are testable without a target.
    ///
    /// Returns an empty vec for the same "no decodable stats" shapes as
    /// [`Self::read_gc_stats`], and for a `raw` shorter than the shape requires
    /// (a short read is a plausible teardown race, not a reason to panic).
    pub fn decode_gc_stats(&self, raw: &[u8], iid: i64) -> Vec<GcStat> {
        let total = match self.stats_buffer_len() {
            Some(t) => t,
            None => return vec![],
        };
        if raw.len() < total {
            return vec![];
        }
        let item_size = self.gc_item_size.unwrap_or(0) as usize;
        let slots = self.gc_slots_per_gen.unwrap_or([0; 3]);
        let bases = self.gc_gen_base_offsets.unwrap_or([0; 3]);
        let layout = match self.gc_layout {
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
                        layout.field_offset($name).map(|o| raw_i64(raw, off + o))
                    };
                }
                stats.push(GcStat {
                    generation: gidx, slot, interpreter_id: iid,
                    ts_start: opt!("ts_start").unwrap_or(0),
                    ts_stop: opt!("ts_stop").unwrap_or(0),
                    collections: raw_i64(raw, off + layout.field_offset("collections").unwrap()),
                    collected: raw_i64(raw, off + layout.field_offset("collected").unwrap()),
                    uncollectable: raw_i64(raw, off + layout.field_offset("uncollectable").unwrap()),
                    candidates: opt!("candidates").unwrap_or(0),
                    duration: layout.field_offset("duration").map(|o| raw_f64(raw, off + o)).unwrap_or(0.0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::{pre_3_13, set_ring};

    /// A synthetic ring-slot layout with an extended field, so the tests exercise
    /// both the required fields and the `Option` ones without pinning any real
    /// build's struct (those are covered by the registry tests in `offsets/mod.rs`).
    static RING_LAYOUT: GcItemLayout = GcItemLayout {
        item_size: 40,
        fields: &[
            ("ts_start", 0),
            ("collections", 8),
            ("collected", 16),
            ("uncollectable", 24),
            ("increment_size", 32),
        ],
    };

    fn put_i64(buf: &mut [u8], off: usize, v: i64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    // ── geometry ────────────────────────────────────────────────

    /// Generations are separated by an 8-byte gap (the ring's own write cursor), so
    /// a base is NOT simply `slots_so_far * item_size`. Dropping the pad shifts every
    /// generation after the first onto the wrong slot.
    #[test]
    fn ring_base_offsets_include_the_inter_generation_pad() {
        let bases = compute_ring_base_offsets(40, &[11, 3, 3]);
        assert_eq!(bases, [0, 11 * 40 + 8, 11 * 40 + 8 + 3 * 40 + 8]);
        assert!(bases[0] < bases[1] && bases[1] < bases[2]);

        // Free-threaded builds carry one slot per generation, same pad.
        let ft = compute_ring_base_offsets(40, &[1, 1, 1]);
        assert_eq!(ft, [0, 48, 96]);
    }

    #[test]
    fn field_offset_reports_presence_and_position() {
        assert_eq!(RING_LAYOUT.field_offset("collections"), Some(8));
        assert_eq!(RING_LAYOUT.field_offset("heap_size"), None);
        assert!(RING_LAYOUT.has_field("increment_size"));
        assert!(!RING_LAYOUT.has_field("heap_size"));
    }

    // ── inline decode (3.8-3.12 and 3.13/3.14) ──────────────────

    /// Three 24-byte slots back to back. Each generation must read from its own
    /// slot: an off-by-one base or stride silently reports generation N's counters
    /// under generation N-1.
    #[test]
    fn inline_decode_maps_each_generation_to_its_own_slot() {
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_addr = Some(0x1000); // any non-None value; decode never reads it

        let mut buf = vec![0u8; 72];
        for g in 0..3usize {
            let base = g * 24;
            put_i64(&mut buf, base, 100 * g as i64 + 1); // collections
            put_i64(&mut buf, base + 8, 100 * g as i64 + 2); // collected
            put_i64(&mut buf, base + 16, 100 * g as i64 + 3); // uncollectable
        }

        let stats = table.decode_gc_stats(&buf, 7);
        assert_eq!(stats.len(), 3, "one slot per generation");
        for (g, s) in stats.iter().enumerate() {
            assert_eq!(s.generation, g as u32);
            assert_eq!(s.slot, 0);
            assert_eq!(s.interpreter_id, 7);
            assert_eq!(s.collections, 100 * g as i64 + 1);
            assert_eq!(s.collected, 100 * g as i64 + 2);
            assert_eq!(s.uncollectable, 100 * g as i64 + 3);
        }
    }

    /// A field the build does not have must stay `None`, not become `Some(0)`.
    /// `gc_stats::print_stats` keys its whole column set on
    /// `increment_size.is_some()`, so blurring the two changes the CLI's output for
    /// every pre-3.13 target.
    #[test]
    fn absent_fields_decode_to_none_not_zero() {
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_addr = Some(0x1000);

        let stats = table.decode_gc_stats(&[0u8; 72], 0);
        let s = &stats[0];
        // The legacy layout has only collections/collected/uncollectable.
        assert_eq!(s.increment_size, None);
        assert_eq!(s.alive_size, None);
        assert_eq!(s.ts_mark_alive_start, None);
        // Non-Option fields with no layout entry fall back to zero.
        assert_eq!(s.ts_start, 0);
        assert_eq!(s.heap_size, 0);
        assert_eq!(s.duration, 0.0);
    }

    // ── ring-buffer decode (3.15.0a8+) ──────────────────────────

    fn ring_table(free_threaded: u64) -> OffsetTable {
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        set_ring(&mut table, RING_LAYOUT.item_size as u64, &RING_LAYOUT, free_threaded);
        table.gc_stats_addr = Some(0x1000);
        table
    }

    /// GIL builds keep 11 young slots and 3 per old generation; the decode must
    /// produce every one, indexed by its own generation and slot.
    #[test]
    fn ring_decode_walks_every_slot_of_every_generation() {
        let table = ring_table(0);
        let slots = table.gc_slots_per_gen.unwrap();
        let bases = table.gc_gen_base_offsets.unwrap();
        assert_eq!(slots, [11, 3, 3], "GIL ring geometry");

        let item = RING_LAYOUT.item_size;
        let mut buf = vec![0u8; bases[2] as usize + slots[2] as usize * item];
        for g in 0..3usize {
            for slot in 0..slots[g] as usize {
                let off = bases[g] as usize + slot * item;
                put_i64(&mut buf, off, 1000 * (g as i64 + 1) + slot as i64); // ts_start
                put_i64(&mut buf, off + 32, 10 * g as i64 + slot as i64); // increment_size
            }
        }

        let stats = table.decode_gc_stats(&buf, 3);
        assert_eq!(stats.len(), 11 + 3 + 3);
        for s in &stats {
            assert_eq!(
                s.ts_start,
                1000 * (s.generation as i64 + 1) + s.slot as i64,
                "generation {} slot {} read from the wrong offset",
                s.generation, s.slot
            );
            assert_eq!(s.increment_size, Some(10 * s.generation as i64 + s.slot as i64));
        }

        // Generation 1 starts one 8-byte pad past the end of generation 0's slots;
        // reading it at `11 * item` instead would land inside the pad and return 0.
        let gen1_first = stats.iter().find(|s| s.generation == 1 && s.slot == 0).unwrap();
        assert_eq!(gen1_first.ts_start, 2000);
    }

    #[test]
    fn free_threaded_ring_has_one_slot_per_generation() {
        let table = ring_table(1);
        assert_eq!(table.gc_slots_per_gen.unwrap(), [1, 1, 1]);

        let bases = table.gc_gen_base_offsets.unwrap();
        let buf = vec![0u8; bases[2] as usize + RING_LAYOUT.item_size];
        let stats = table.decode_gc_stats(&buf, 0);
        assert_eq!(stats.len(), 3);
        assert!(stats.iter().all(|s| s.slot == 0));
    }

    // ── shape guards ────────────────────────────────────────────

    /// A buffer shorter than the shape requires means the read was truncated (a
    /// plausible teardown race). Return nothing rather than index-panicking mid-walk.
    #[test]
    fn short_buffer_decodes_to_nothing() {
        let mut table = pre_3_13::table_for_version(3, 12).unwrap();
        table.gc_stats_addr = Some(0x1000);
        assert!(table.decode_gc_stats(&[], 0).is_empty());
        assert!(table.decode_gc_stats(&[0u8; 71], 0).is_empty(), "one byte short");
        assert_eq!(table.decode_gc_stats(&[0u8; 72], 0).len(), 3);
    }

    /// "This build exposes no decodable stats" is a shape fact, not a failure —
    /// each missing piece of the shape independently yields an empty result.
    #[test]
    fn missing_shape_information_decodes_to_nothing() {
        let base = pre_3_13::table_for_version(3, 12).unwrap();
        let buf = vec![0u8; 72];

        let mut no_layout = base.clone();
        no_layout.gc_layout = None;
        assert!(no_layout.decode_gc_stats(&buf, 0).is_empty());

        let mut no_item_size = base.clone();
        no_item_size.gc_item_size = None;
        assert!(no_item_size.decode_gc_stats(&buf, 0).is_empty());

        let mut zero_item_size = base.clone();
        zero_item_size.gc_item_size = Some(0);
        assert!(zero_item_size.decode_gc_stats(&buf, 0).is_empty());

        let mut no_slots = base.clone();
        no_slots.gc_slots_per_gen = None;
        assert!(no_slots.decode_gc_stats(&buf, 0).is_empty());

        let mut no_bases = base.clone();
        no_bases.gc_gen_base_offsets = None;
        assert!(no_bases.decode_gc_stats(&buf, 0).is_empty());
    }
}
