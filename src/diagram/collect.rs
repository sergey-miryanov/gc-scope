#![allow(dead_code)]

use std::mem::offset_of;
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use crate::{memory::reader, memory::process, remote_debugging::offsets};
use crate::remote_debugging::version::PythonVersion;
use crate::remote_debugging::offsets::GcGenerationStatsSlot;

const YOUNG_COUNT: usize = 11;
const OLD_COUNT: usize = 3;

fn read_u64(pid: u32, addr: u64) -> Result<u64> {
    let bytes = reader::read_memory(pid, addr, 8)?;
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn read_i64(pid: u32, addr: u64) -> Result<i64> {
    let bytes = reader::read_memory(pid, addr, 8)?;
    Ok(i64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

#[derive(Debug)]
pub struct GcSlot {
    pub generation: u32,
    pub slot: usize,
    pub byte_offset: usize,
    pub start_ts: i64,
    pub stop_ts: i64,
    pub collections: i64,
    pub collected: i64,
    pub uncollectable: i64,
    pub candidates: i64,
    pub duration: f64,
    pub heap_size: i64,
}

#[derive(Debug)]
pub struct GcStatsSnapshot {
    pub stats_addr: u64,
    pub stats_size: u64,
    pub raw_stats_bytes: Vec<u8>,
    pub slots: Vec<GcSlot>,
}

#[derive(Debug)]
pub struct GcSubState {
    pub raw_bytes: Vec<u8>,
    pub generation_stats: GcStatsSnapshot,
}

#[derive(Debug)]
pub struct InterpreterSnapshot {
    pub addr: u64,
    pub raw_bytes: Vec<u8>,
    pub gc: GcSubState,
    pub gc_offset: u64,
    pub gc_size: u64,
    pub id: i64,
    pub next_addr: u64,
}

#[derive(Debug)]
pub struct CollectedData {
    pub pid: u32,
    pub runtime_addr: u64,
    pub runtime_version: u64,
    pub runtime_raw_bytes: Vec<u8>,
    pub debug_offsets_size: u64,
    pub offsets: offsets::VersionedOffsets,
    pub interpreter: InterpreterSnapshot,
    pub collect_duration: Duration,
}

/// A named field from _Py_DebugOffsets with its stored offset value.
/// This represents one entry in the debug offsets struct, e.g.
/// `interpreter_state.id = 7272` means the `id` field is at byte 7272
/// from the start of `_PyRuntime` in the target process.
#[derive(Debug)]
pub struct DebugOffsetField {
    pub name: &'static str,
    pub value: u64,       // the stored offset value
}

pub fn collect_data(pid: u32, version: &PythonVersion) -> Result<CollectedData> {
    let t0 = Instant::now();
    let runtime_addr = process::find_runtime(pid)?;
    let (_, runtime_version, off) = offsets::read_offsets(pid, version)?;

    let debug_offsets_size = off.debug_offsets_total_size();
    let total_read = (debug_offsets_size as usize) * 2;
    let runtime_raw_bytes = reader::read_memory(pid, runtime_addr, total_read)
        .context("Failed to read _Py_DebugOffsets + _PyRuntime memory")?;

    // Follow the same pattern as gc_stats.rs: use offset values from
    // _Py_DebugOffsets as byte offsets from runtime_addr (which IS _PyRuntime).
    let head_addr = read_u64(pid, runtime_addr + off.runtime_interpreters_head())
        .context("Failed to read interpreters_head pointer")?;

    let gc_offset = off.interpreter_state_gc();
    let gc_size = off.gc_size();
    let next_addr = read_u64(pid, head_addr + off.interpreter_state_next())?;
    let id = read_i64(pid, head_addr + off.interpreter_state_id())?;

    // Read a reasonable chunk of interpreter state (first 256 bytes) for hex dump
    let interp_raw = reader::read_memory(pid, head_addr, 256)
        .context("Failed to read interpreter state start")?;

    // Read GC sub-struct at its actual offset within the interpreter
    let gc_raw = reader::read_memory(pid, head_addr + gc_offset, gc_size as usize)
        .context("Failed to read GC state")?;

    let gen_stats_field_off = off.gc_generation_stats();
    let gen_stats_size = off.gc_generation_stats_size();
    // The generation_stats pointer is at gc_addr + gen_stats_field_off
    // where gen_stats_field_off is an offset WITHIN the GC sub-struct
    // (not an offset within _PyRuntime)
    let stats_ptr = read_u64(pid, head_addr + gc_offset + gen_stats_field_off)
        .context("Failed to read generation_stats pointer")?;

    let (raw_stats_bytes, slots) = if stats_ptr != 0 {
        let raw = reader::read_memory(pid, stats_ptr, gen_stats_size as usize)
            .context("Failed to read GC stats buffer")?;
        let parsed = parse_gc_slots(&raw, gen_stats_size);
        (raw, parsed)
    } else {
        (Vec::new(), Vec::new())
    };

    let gc = GcSubState {
        raw_bytes: gc_raw,
        generation_stats: GcStatsSnapshot {
            stats_addr: stats_ptr,
            stats_size: gen_stats_size,
            raw_stats_bytes,
            slots,
        },
    };

    let interpreter = InterpreterSnapshot {
        addr: head_addr,
        raw_bytes: interp_raw,
        gc,
        gc_offset,
        gc_size,
        id,
        next_addr,
    };

    Ok(CollectedData {
        pid,
        runtime_addr,
        runtime_version,
        runtime_raw_bytes,
        debug_offsets_size,
        offsets: off,
        interpreter,
        collect_duration: t0.elapsed(),
    })
}

/// Extract debug offset values for display.
impl CollectedData {
    pub fn runtime_offset_fields(&self) -> Vec<DebugOffsetField> {
        let off = &self.offsets;
        vec![
            DebugOffsetField { name: "runtime_state.finalizing", value: off.runtime_state_finalizing() },
            DebugOffsetField { name: "runtime_state.interpreters_head", value: off.runtime_interpreters_head() },
            DebugOffsetField { name: "interpreter_state.id", value: off.interpreter_state_id() },
            DebugOffsetField { name: "interpreter_state.next", value: off.interpreter_state_next() },
            DebugOffsetField { name: "interpreter_state.threads_head", value: off.interpreter_state_threads_head() },
            DebugOffsetField { name: "interpreter_state.threads_main", value: off.interpreter_state_threads_main() },
            DebugOffsetField { name: "interpreter_state.gc", value: off.interpreter_state_gc() },
            DebugOffsetField { name: "gc.collecting", value: off.gc_collecting() },
            DebugOffsetField { name: "gc.generation_stats", value: off.gc_generation_stats() },
            DebugOffsetField { name: "gc.generation_stats_size", value: off.gc_generation_stats_size() },
        ]
    }
}

// ── GC slot parsing (same logic as gc_stats.rs) ────────────────
fn parse_gc_slots(raw: &[u8], gen_stats_size: u64) -> Vec<GcSlot> {
    if raw.is_empty() || gen_stats_size < 64 {
        return Vec::new();
    }

    let size = gen_stats_size as usize;
    let item_size = (size - 24) / 17;
    if item_size < 64 {
        return Vec::new();
    }

    let mut slots = Vec::new();

    for i in 0..YOUNG_COUNT {
        let offset = i * item_size;
        if offset + item_size > size { break; }
        if let Some(s) = parse_slot(&raw[offset..], 0, i, offset) {
            slots.push(s);
        }
    }

    let old0_base = YOUNG_COUNT * item_size + 8;
    for i in 0..OLD_COUNT {
        let offset = old0_base + i * item_size;
        if offset + item_size > size { break; }
        if let Some(s) = parse_slot(&raw[offset..], 1, i, offset) {
            slots.push(s);
        }
    }

    let old1_base = old0_base + OLD_COUNT * item_size + 8;
    for i in 0..OLD_COUNT {
        let offset = old1_base + i * item_size;
        if offset + item_size > size { break; }
        if let Some(s) = parse_slot(&raw[offset..], 2, i, offset) {
            slots.push(s);
        }
    }

    slots
}

fn parse_slot(bytes: &[u8], generation: u32, slot: usize, byte_offset: usize) -> Option<GcSlot> {
    type S = GcGenerationStatsSlot;

    fn rdi(off: usize, b: &[u8]) -> i64 {
        i64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }
    fn rdf(off: usize, b: &[u8]) -> f64 {
        f64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    let stop_ts = rdi(offset_of!(S, ts_stop), bytes);
    let start_ts = rdi(offset_of!(S, ts_start), bytes);
    // Skip torn entries: if the ring buffer was being written concurrently,
    // stop_ts may be stale and less than start_ts.
    if stop_ts < start_ts {
        return None;
    }

    Some(GcSlot {
        generation,
        slot,
        byte_offset,
        start_ts,
        stop_ts,
        collections: rdi(offset_of!(S, collections), bytes),
        collected: rdi(offset_of!(S, collected), bytes),
        uncollectable: rdi(offset_of!(S, uncollectable), bytes),
        candidates: rdi(offset_of!(S, candidates), bytes),
        duration: rdf(offset_of!(S, duration), bytes),
        heap_size: rdi(offset_of!(S, heap_size), bytes),
    })
}

/// Compute average collection pause time per generation from a single snapshot.
/// Uses the full ring range: `(max.duration - min.duration) / (max.collections - min.collections)`.
pub fn avg_collection_time_per_gen(slots: &[GcSlot]) -> [f64; 3] {
    let mut gen_slots: [Vec<&GcSlot>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for slot in slots {
        let g = slot.generation as usize;
        if g < 3 {
            gen_slots[g].push(slot);
        }
    }

    let mut avgs = [0.0f64; 3];
    for (g, gslots) in gen_slots.iter().enumerate() {
        if gslots.len() < 2 {
            continue;
        }
        let min_coll = gslots.iter().min_by_key(|s| s.collections).unwrap();
        let max_coll = gslots.iter().max_by_key(|s| s.collections).unwrap();

        let coll_delta = max_coll.collections - min_coll.collections;
        let dur_delta = max_coll.duration - min_coll.duration;

        if coll_delta > 0 {
            avgs[g] = dur_delta / coll_delta as f64;
        }
    }
    avgs
}

/// Compute collections rate per second for each generation from a single snapshot.
/// Uses the full ring range: `(max.collections - min.collections) / ((max.stop_ts - min.start_ts) / 1e9)`.
pub fn collections_rate_from_slots(slots: &[GcSlot]) -> [f64; 3] {
    let mut gen_slots: [Vec<&GcSlot>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for slot in slots {
        let g = slot.generation as usize;
        if g < 3 {
            gen_slots[g].push(slot);
        }
    }

    let mut rates = [0.0f64; 3];
    for (g, gslots) in gen_slots.iter().enumerate() {
        if gslots.len() < 2 {
            continue;
        }
        let min_coll = gslots.iter().min_by_key(|s| s.collections).unwrap();
        let max_coll = gslots.iter().max_by_key(|s| s.collections).unwrap();
        let min_ts  = gslots.iter().min_by_key(|s| s.start_ts).unwrap();
        let max_ts  = gslots.iter().max_by_key(|s| s.stop_ts).unwrap();

        let coll_delta = max_coll.collections - min_coll.collections;
        let ts_delta_ns = max_ts.stop_ts - min_ts.start_ts;

        if ts_delta_ns > 0 && coll_delta > 0 {
            rates[g] = coll_delta as f64 / (ts_delta_ns as f64 / 1_000_000_000.0);
        }
    }
    rates
}
