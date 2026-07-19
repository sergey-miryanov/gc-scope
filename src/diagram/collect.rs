#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{Context, Result};
use crate::remote_debugging::offsets::{self, VersionedOffsets};
use crate::remote_debugging::session::{PySession, Resolved};

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
    /// Authoritative per-slot size from the version's `gc_layout` (not re-derived from a
    /// magic `stats_size` formula) — 24 for inline 3.13/3.14, the ring struct size for 3.15+.
    pub item_size: usize,
    /// Per-generation slot counts from the version's layout (`offset_table.gc_slots_per_gen`):
    /// `[1, 1, 1]` for inline 3.13/3.14 and free-threaded rings, `[11, 3, 3]` for GIL rings.
    /// Captured here so renderers read it rather than assuming a GIL layout.
    pub slots_per_gen: [u64; 3],
    /// Whether the version's slot layout carries GC-pause timing (`ts_start`/`ts_stop`).
    /// Gates the collections-rate summary: false (e.g. inline 3.13/3.14) -> rate renders
    /// "n/a" instead of a fake 0.
    pub has_timestamps: bool,
    /// Whether the slot layout carries the `duration` field. Gates the avg-collection-time
    /// summary: false -> "n/a" (unrecoverable without the field — an external sampler can't
    /// observe an internal GC pause).
    pub has_duration: bool,
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
    /// Shared layout from the attached [`PySession`]. Present for every tier;
    /// [`CollectedData::offsets`] returns `None` for pre-3.13 (`Legacy`), which has
    /// no `_Py_DebugOffsets` struct.
    pub resolved: Arc<Resolved>,
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

pub fn collect_data(session: &PySession) -> Result<CollectedData> {
    let t0 = Instant::now();
    let pid = session.pid();
    let runtime_addr = session.runtime_addr();

    // The `_Py_DebugOffsets` struct dump (diagram sections 1–2) needs a 3.13+ tier.
    // Pre-3.13 (`Legacy`) has no such struct: navigate via the flat `OffsetTable`
    // instead and render a focused GC-generation-stats view. The renderers gate the
    // debug-offsets panels on `offsets()` being present.
    let off_opt = session.resolved().offsets();
    // The table was built once in `attach`; reuse it for navigation on both tiers.
    let offset_table = session.resolved().table().clone();

    // `_Py_DebugOffsets` bytes for the struct panels — 3.13+ only; empty for Legacy.
    let (debug_offsets_size, runtime_raw_bytes) = match off_opt {
        Some(off) => {
            let sz = off.debug_offsets_total_size();
            let raw = session
                .read(runtime_addr, (sz as usize) * 2)
                .context("Failed to read _Py_DebugOffsets + _PyRuntime memory")?;
            (sz, raw)
        }
        None => (0u64, Vec::new()),
    };

    // Navigation offsets come from the flat table — identical values to the bindgen
    // accessors on 3.13+, and the only source available pre-3.13.
    let head_addr = session
        .read_u64(runtime_addr + offset_table.runtime_interpreters_head())
        .context("Failed to read interpreters_head pointer")?;

    let gc_offset = offset_table.interp_gc.unwrap_or(0);
    // Exact `gc` sub-struct span on 3.13+; on Legacy synthesize the inline stats region
    // (only used for the section-2 hexdump, which Legacy skips).
    let gc_size = match off_opt {
        Some(off) => off.gc_size(),
        None => offset_table.gc_stats_inline_off + gc_stats_total_bytes(&offset_table) as u64,
    };
    let next_addr = session.read_u64(head_addr + offset_table.interp_next())?;
    let id = session.read_i64(head_addr + offset_table.interp_id())?;

    // Read a reasonable chunk of interpreter state (first 256 bytes) for hex dump
    let interp_raw = session
        .read(head_addr, 256)
        .context("Failed to read interpreter state start")?;

    // Read GC sub-struct at its actual offset within the interpreter
    let gc_raw = session
        .read(head_addr + gc_offset, gc_size as usize)
        .context("Failed to read GC state")?;

    // Resolve the GC generation-stats region by its version-specific shape — same logic as
    // `gc_stats.rs::read_gc_stats`. `InlineArray` (pre-3.13, 3.13/3.14) stores the stats
    // inline in `_gc_runtime_state` at a fixed offset; `RingBuffer` (3.15+, always a 3.13+
    // tier) reaches a ring buffer through the `gc.generation_stats` pointer.
    let item_size = offset_table.gc_item_size.unwrap_or(0) as usize;
    let slots_per_gen = offset_table.gc_slots_per_gen.unwrap_or([0, 0, 0]);
    let (stats_addr, stats_total) = match offset_table.gc_stats_kind {
        offsets::offset_table::GcStatsKind::None => (0u64, 0usize),
        offsets::offset_table::GcStatsKind::InlineArray => {
            let addr = head_addr + gc_offset + offset_table.gc_stats_inline_off;
            (addr, gc_stats_total_bytes(&offset_table))
        }
        offsets::offset_table::GcStatsKind::RingBuffer => {
            let gen_stats_field_off = off_opt.map(|o| o.gc_generation_stats()).unwrap_or(0);
            let ptr = if gen_stats_field_off == 0 {
                0
            } else {
                session
                    .read_u64(head_addr + gc_offset + gen_stats_field_off)
                    .context("Failed to read generation_stats pointer")?
            };
            let size = off_opt.map(|o| o.gc_generation_stats_size()).unwrap_or(0) as usize;
            (ptr, size)
        }
    };

    let (raw_stats_bytes, slots) = if stats_addr != 0 && stats_total > 0 {
        let raw = session
            .read(stats_addr, stats_total)
            .context("Failed to read GC stats buffer")?;
        let parsed = parse_gc_slots(&raw, &offset_table);
        (raw, parsed)
    } else {
        (Vec::new(), Vec::new())
    };

    // Field presence is a property of the version's slot layout (a GcSlot's absent fields
    // are indistinguishable zeros), so capture it once here alongside the geometry.
    let (has_timestamps, has_duration) = match offset_table.gc_layout {
        Some(l) => (l.has_field("ts_start") && l.has_field("ts_stop"), l.has_field("duration")),
        None => (false, false),
    };

    let gc = GcSubState {
        raw_bytes: gc_raw,
        generation_stats: GcStatsSnapshot {
            stats_addr,
            stats_size: stats_total as u64,
            item_size,
            slots_per_gen,
            has_timestamps,
            has_duration,
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
        runtime_version: session.stored_hex().unwrap_or(offset_table.version_hex),
        runtime_raw_bytes,
        debug_offsets_size,
        resolved: session.resolved_arc(),
        interpreter,
        collect_duration: t0.elapsed(),
    })
}

/// Extract debug offset values for display.
impl CollectedData {
    /// The bindgen offsets, or `None` for pre-3.13 (`Legacy`), which has no
    /// `_Py_DebugOffsets` struct. Renderers gate the debug-offsets panels on this.
    pub fn offsets(&self) -> Option<&VersionedOffsets> {
        self.resolved.offsets()
    }

    /// Key `_Py_DebugOffsets` field values for the interpreter panel; empty pre-3.13.
    pub fn runtime_offset_fields(&self) -> Vec<DebugOffsetField> {
        let Some(off) = self.offsets() else { return Vec::new(); };
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

// ── GC slot parsing ────────────────────────────────────────────
/// Total bytes of the (inline) generation-stats region: `bases[last] + slots[last] *
/// item_size`. Used only for the `InlineArray` kind (ring buffers use the process-reported
/// `generation_stats_size` directly).
fn gc_stats_total_bytes(table: &offsets::offset_table::OffsetTable) -> usize {
    match (table.gc_item_size, table.gc_gen_base_offsets, table.gc_slots_per_gen) {
        (Some(item), Some(bases), Some(slots)) => (bases[2] + slots[2] * item) as usize,
        _ => 0,
    }
}

/// Parse GC slots from the raw region using the version's geometry (per-gen slot counts,
/// gen base offsets, per-slot item size) and per-slot field layout — the same source
/// `offset_table::read_gc_stats` uses. This handles both inline (3.13/3.14: 1 slot/gen,
/// 3 fields) and ring-buffer (3.15+: 11/3/3 slots, many fields) layouts uniformly.
fn parse_gc_slots(raw: &[u8], table: &offsets::offset_table::OffsetTable) -> Vec<GcSlot> {
    let (Some(item_size), Some(slots_per_gen), Some(bases), Some(layout)) = (
        table.gc_item_size.map(|v| v as usize),
        table.gc_slots_per_gen,
        table.gc_gen_base_offsets,
        table.gc_layout,
    ) else {
        return Vec::new();
    };
    if raw.is_empty() || item_size == 0 {
        return Vec::new();
    }

    let mut slots = Vec::new();
    for gen_idx in 0..3u32 {
        let n = slots_per_gen[gen_idx as usize] as usize;
        let base = bases[gen_idx as usize] as usize;
        for slot in 0..n {
            let offset = base + slot * item_size;
            if offset + item_size > raw.len() { break; }
            if let Some(s) = parse_slot(&raw[offset..offset + item_size], gen_idx, slot, offset, layout) {
                slots.push(s);
            }
        }
    }
    slots
}

fn parse_slot(
    bytes: &[u8],
    generation: u32,
    slot: usize,
    byte_offset: usize,
    layout: &offsets::offset_table::GcItemLayout,
) -> Option<GcSlot> {
    let rdi = |name: &str| -> i64 {
        layout.field_offset(name)
            .filter(|&o| o + 8 <= bytes.len())
            .map(|o| i64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()))
            .unwrap_or(0)
    };
    let rdf = |name: &str| -> f64 {
        layout.field_offset(name)
            .filter(|&o| o + 8 <= bytes.len())
            .map(|o| f64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()))
            .unwrap_or(0.0)
    };

    let start_ts = rdi("ts_start");
    let stop_ts = rdi("ts_stop");
    // Ring-buffer slots carry timestamps: skip torn entries (a concurrent write left
    // stop_ts stale and below start_ts). Inline layouts (3.13/3.14) have no timestamps —
    // both read as 0, so the check is a no-op and every slot is kept.
    if layout.has_field("ts_start") && layout.has_field("ts_stop") && stop_ts < start_ts {
        return None;
    }

    Some(GcSlot {
        generation,
        slot,
        byte_offset,
        start_ts,
        stop_ts,
        collections: rdi("collections"),
        collected: rdi("collected"),
        uncollectable: rdi("uncollectable"),
        candidates: rdi("candidates"),
        duration: rdf("duration"),
        heap_size: rdi("heap_size"),
    })
}

/// Compute average collection pause time per generation from a single snapshot.
/// Uses the full ring range: `(max.duration - min.duration) / (max.collections - min.collections)`.
/// Returns `[None; 3]` when the slot layout has no `duration` field (e.g. inline 3.13/3.14):
/// the pause time is unrecoverable externally, so the summary renders "n/a" rather than a
/// fake 0. Gens with <2 slots stay `Some(0.0)` (formatted like before).
pub fn avg_collection_time_per_gen(slots: &[GcSlot], has_duration: bool) -> [Option<f64>; 3] {
    if !has_duration {
        return [None, None, None];
    }
    let mut gen_slots: [Vec<&GcSlot>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for slot in slots {
        let g = slot.generation as usize;
        if g < 3 {
            gen_slots[g].push(slot);
        }
    }

    let mut avgs = [Some(0.0f64); 3];
    for (g, gslots) in gen_slots.iter().enumerate() {
        if gslots.len() < 2 {
            continue;
        }
        let min_coll = gslots.iter().min_by_key(|s| s.collections).unwrap();
        let max_coll = gslots.iter().max_by_key(|s| s.collections).unwrap();

        let coll_delta = max_coll.collections - min_coll.collections;
        let dur_delta = max_coll.duration - min_coll.duration;

        if coll_delta > 0 {
            avgs[g] = Some(dur_delta / coll_delta as f64);
        }
    }
    avgs
}

/// Compute collections rate per second for each generation from a single snapshot.
/// Uses the full ring range: `(max.collections - min.collections) / ((max.stop_ts - min.start_ts) / 1e9)`.
/// Returns `[None; 3]` when the slot layout has no `ts_start`/`ts_stop` fields (e.g. inline
/// 3.13/3.14): there is no time base in a single snapshot, so the summary renders "n/a"
/// rather than a fake 0. Gens with <2 slots stay `Some(0.0)` (formatted like before).
pub fn collections_rate_from_slots(slots: &[GcSlot], has_timestamps: bool) -> [Option<f64>; 3] {
    if !has_timestamps {
        return [None, None, None];
    }
    let mut gen_slots: [Vec<&GcSlot>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for slot in slots {
        let g = slot.generation as usize;
        if g < 3 {
            gen_slots[g].push(slot);
        }
    }

    let mut rates = [Some(0.0f64); 3];
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
            rates[g] = Some(coll_delta as f64 / (ts_delta_ns as f64 / 1_000_000_000.0));
        }
    }
    rates
}
