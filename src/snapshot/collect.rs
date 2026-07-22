//! Reader-layer snapshot collector.
//!
//! [`collect_data`] walks an attached [`PySession`] once and returns a fully-owned
//! [`CollectedData`] — the interpreter/GC layout, raw struct bytes, and decoded generation
//! entries — for a consumer to render. It lives in the reader layer (not `tui/`) so it is
//! a single source of truth: it resolves the stats region through
//! [`PySession::gc_stats_region_addr`] and decodes entries through
//! [`crate::remote_debugging::offsets::offset_table::OffsetTable::decode_gc_stats`], the same
//! paths the monitor uses, and the `tui` renderer merely consumes its output.

#![allow(dead_code)]

use crate::remote_debugging::offsets::{self, VersionedOffsets};
use crate::remote_debugging::session::{PySession, Resolved};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which heavy payload layers of a snapshot a caller wants [`collect_data`] to read.
///
/// Each field gates one independent memory read; a skipped layer comes back as an empty
/// buffer (indistinguishable from a version that legitimately lacks it, which the renderers
/// already tolerate). The cheap navigation reads and layout scalars are always collected, so
/// a valid skeleton snapshot is produced regardless of the request.
#[derive(Debug, Clone, Copy)]
pub struct CollectRequest {
    /// The `_Py_DebugOffsets` + `_PyRuntime` struct dump (3.13+ only).
    pub debug_offsets: bool,
    /// The `gc` sub-struct raw bytes (for the GC-state hexdump).
    pub gc_state: bool,
    /// The GC generation stats — decoded `entries` AND `raw_stats_bytes` together. These two
    /// always travel as a unit: a renderer that has `entries` but no raw buffer would index a
    /// buffer it doesn't have, so the layer is atomic by construction.
    pub gc_stats: bool,
}

impl CollectRequest {
    /// Collect every layer — the historical `collect_data` behavior.
    pub fn all() -> Self {
        Self {
            debug_offsets: true,
            gc_state: true,
            gc_stats: true,
        }
    }

    /// Exactly the layers the `tui` renderer draws. Equal to [`all`](Self::all) today;
    /// kept distinct so a future focused view can narrow it, and so call sites read as
    /// "collect what the TUI needs".
    pub fn tui() -> Self {
        Self::all()
    }

    /// Only the GC generation stats — no struct dumps. For a stats-focused consumer.
    pub fn gc_stats_only() -> Self {
        Self {
            debug_offsets: false,
            gc_state: false,
            gc_stats: true,
        }
    }
}

#[derive(Debug)]
pub struct GcEntry {
    pub generation: u32,
    pub index: usize,
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
    /// Authoritative per-entry size from the version's `gc_layout` (not re-derived from a
    /// magic `stats_size` formula) — 24 for inline 3.13/3.14, the ring struct size for 3.15+.
    pub item_size: usize,
    /// Per-generation entry counts from the version's layout (`offset_table.gc_entries_per_gen`):
    /// `[1, 1, 1]` for inline 3.13/3.14 and free-threaded rings, `[11, 3, 3]` for GIL rings.
    /// Captured here so renderers read it rather than assuming a GIL layout.
    pub entries_per_gen: [u64; 3],
    /// Whether the version's entry layout carries GC-pause timing (`ts_start`/`ts_stop`).
    /// Gates the collections-rate summary: false (e.g. inline 3.13/3.14) -> rate renders
    /// "n/a" instead of a fake 0.
    pub has_timestamps: bool,
    /// Whether the entry layout carries the `duration` field. Gates the avg-collection-time
    /// summary: false -> "n/a" (unrecoverable without the field — an external sampler can't
    /// observe an internal GC pause).
    pub has_duration: bool,
    pub raw_stats_bytes: Vec<u8>,
    pub entries: Vec<GcEntry>,
}

#[derive(Debug)]
pub struct GcSubState {
    pub raw_bytes: Vec<u8>,
    pub generation_stats: GcStatsSnapshot,
}

#[derive(Debug)]
pub struct InterpreterSnapshot {
    pub addr: u64,
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
    pub value: u64, // the stored offset value
}

pub fn collect_data(session: &PySession, request: &CollectRequest) -> Result<CollectedData> {
    let t0 = Instant::now();
    let pid = session.pid();
    let runtime_addr = session.runtime_addr();
    // `off_opt` is the 3.13+ bindgen view (`None` on pre-3.13 `Legacy`); the table serves both.
    let off_opt = session.resolved().offsets();
    let offset_table = session.resolved().table().clone();

    // Each heavy layer is one gated read (skipped → empty). The gating lives here, not inside
    // the helpers, so this reads as "collect X if requested". `debug_offsets_size` is a cheap
    // scalar renderers label the panel with, so it is always set; only its byte read is gated.
    let debug_offsets_size = off_opt
        .map(|off| off.debug_offsets_total_size())
        .unwrap_or(0);
    let runtime_raw_bytes = if request.debug_offsets && off_opt.is_some() {
        read_debug_offsets_dump(session, runtime_addr, debug_offsets_size)?
    } else {
        Vec::new()
    };

    let nav = resolve_interpreter_nav(session, &offset_table, off_opt, runtime_addr)?;

    let gc_raw = if request.gc_state {
        read_gc_state(session, nav.gc_addr, nav.gc_size)?
    } else {
        Vec::new()
    };

    let generation_stats = collect_gc_stats(
        session,
        &offset_table,
        off_opt,
        nav.gc_addr,
        request.gc_stats,
    )?;

    let interpreter = InterpreterSnapshot {
        addr: nav.head_addr,
        gc: GcSubState {
            raw_bytes: gc_raw,
            generation_stats,
        },
        gc_offset: nav.gc_offset,
        gc_size: nav.gc_size,
        id: nav.id,
        next_addr: nav.next_addr,
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

/// L1: the `_Py_DebugOffsets` + `_PyRuntime` struct dump (3.13+ only). The `size*2` span
/// covers the debug-offsets struct and the `_PyRuntime` that follows it, which the renderers
/// slice into their two struct panels.
fn read_debug_offsets_dump(session: &PySession, runtime_addr: u64, size: u64) -> Result<Vec<u8>> {
    session
        .read(runtime_addr, (size as usize) * 2)
        .context("Failed to read _Py_DebugOffsets + _PyRuntime memory")
}

/// The interpreter/GC navigation values — cheap structural reads resolved regardless of which
/// payload layers were requested, since every layer hangs off `gc_addr`.
struct InterpreterNav {
    head_addr: u64,
    gc_addr: u64,
    gc_size: u64,
    gc_offset: u64,
    id: i64,
    next_addr: u64,
}

fn resolve_interpreter_nav(
    session: &PySession,
    table: &offsets::offset_table::OffsetTable,
    off_opt: Option<&VersionedOffsets>,
    runtime_addr: u64,
) -> Result<InterpreterNav> {
    // Navigation offsets come from the flat table — identical values to the bindgen accessors
    // on 3.13+, and the only source available pre-3.13.
    let head_addr = session
        .read_u64(runtime_addr + table.runtime_interpreters_head())
        .context("Failed to read interpreters_head pointer")?;
    let gc_offset = table.interp_gc.unwrap_or(0);
    let gc_addr = table.gc_state_addr(runtime_addr, head_addr);
    // Exact `gc` sub-struct span on 3.13+; on Legacy synthesize the inline stats region
    // (only used for the section-2 hexdump, which Legacy skips).
    let gc_size = match off_opt {
        Some(off) => off.gc_size(),
        None => table.gc_stats_inline_off + table.stats_buffer_len().unwrap_or(0) as u64,
    };
    let next_addr = session.read_u64(head_addr + table.interp_next())?;
    let id = session.read_i64(head_addr + table.interp_id())?;
    Ok(InterpreterNav {
        head_addr,
        gc_addr,
        gc_size,
        gc_offset,
        id,
        next_addr,
    })
}

/// L3: the `gc` sub-struct bytes for the GC-state hexdump, read to exactly `gc_size` bytes.
fn read_gc_state(session: &PySession, gc_addr: u64, gc_size: u64) -> Result<Vec<u8>> {
    session
        .read(gc_addr, gc_size as usize)
        .context("Failed to read GC state")
}

/// L4: the GC generation stats chunk. The geometry/field-presence scalars and the region
/// address are always resolved (renderers use them for labels and the "not available" gate);
/// the heavy `raw_stats_bytes` + decoded `entries` are read only when `collect` is set. Those
/// two always travel together, so a caller never sees decoded entries without their raw bytes.
fn collect_gc_stats(
    session: &PySession,
    table: &offsets::offset_table::OffsetTable,
    off_opt: Option<&VersionedOffsets>,
    gc_addr: u64,
    collect: bool,
) -> Result<GcStatsSnapshot> {
    let item_size = table.gc_item_size.unwrap_or(0) as usize;
    let entries_per_gen = table.gc_entries_per_gen.unwrap_or([0, 0, 0]);
    let stats_addr = session.gc_stats_region_addr(gc_addr)?.unwrap_or(0);
    // How many raw bytes the buffer spans differs by kind: the exact inline span, or the
    // process-reported ring size (which includes the trailing per-generation cursors the
    // decoder skips).
    let stats_total = match table.gc_stats_kind {
        offsets::offset_table::GcStatsKind::None => 0usize,
        offsets::offset_table::GcStatsKind::InlineArray => table.stats_buffer_len().unwrap_or(0),
        offsets::offset_table::GcStatsKind::RingBuffer => {
            off_opt.map(|o| o.gc_generation_stats_size()).unwrap_or(0) as usize
        }
    };

    let (raw_stats_bytes, entries) = if collect && stats_addr != 0 && stats_total > 0 {
        let raw = session
            .read(stats_addr, stats_total)
            .context("Failed to read GC stats buffer")?;
        let parsed = parse_gc_entries(&raw, table);
        (raw, parsed)
    } else {
        (Vec::new(), Vec::new())
    };

    // Field presence is a property of the version's entry layout (a GcEntry's absent fields
    // are indistinguishable zeros), so capture it once here alongside the geometry.
    let (has_timestamps, has_duration) = match table.gc_layout {
        Some(l) => (
            l.has_field("ts_start") && l.has_field("ts_stop"),
            l.has_field("duration"),
        ),
        None => (false, false),
    };

    Ok(GcStatsSnapshot {
        stats_addr,
        stats_size: stats_total as u64,
        item_size,
        entries_per_gen,
        has_timestamps,
        has_duration,
        raw_stats_bytes,
        entries,
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
        let Some(off) = self.offsets() else {
            return Vec::new();
        };
        vec![
            DebugOffsetField {
                name: "runtime_state.finalizing",
                value: off.runtime_state_finalizing(),
            },
            DebugOffsetField {
                name: "runtime_state.interpreters_head",
                value: off.runtime_interpreters_head(),
            },
            DebugOffsetField {
                name: "interpreter_state.id",
                value: off.interpreter_state_id(),
            },
            DebugOffsetField {
                name: "interpreter_state.next",
                value: off.interpreter_state_next(),
            },
            DebugOffsetField {
                name: "interpreter_state.threads_head",
                value: off.interpreter_state_threads_head(),
            },
            DebugOffsetField {
                name: "interpreter_state.threads_main",
                value: off.interpreter_state_threads_main(),
            },
            DebugOffsetField {
                name: "interpreter_state.gc",
                value: off.interpreter_state_gc(),
            },
            DebugOffsetField {
                name: "gc.collecting",
                value: off.gc_collecting(),
            },
            DebugOffsetField {
                name: "gc.generation_stats",
                value: off.gc_generation_stats(),
            },
            DebugOffsetField {
                name: "gc.generation_stats_size",
                value: off.gc_generation_stats_size(),
            },
        ]
    }
}

// ── GC entry parsing ────────────────────────────────────────────
/// Build the TUI's per-entry view from the raw generation-stats region.
///
/// Decoding runs through the reader layer's single decoder
/// ([`OffsetTable::decode_gc_stats`]) — the exact path the monitor uses — then projects
/// each [`GcStat`](crate::remote_debugging::gc_stats::GcStat) onto the display-oriented
/// [`GcEntry`], recovering the raw-region `byte_offset` (for the hexdump highlight) from the
/// table geometry. The one TUI-only policy lives here: torn ring entries
/// (`stop_ts < start_ts`, a half-written concurrent update) are dropped, whereas the
/// monitor keeps every entry and dedups downstream. Inline layouts (3.13/3.14) carry no
/// timestamps, so `has_ts` is false and every entry is kept.
fn parse_gc_entries(raw: &[u8], table: &offsets::offset_table::OffsetTable) -> Vec<GcEntry> {
    let has_ts = table
        .gc_layout
        .is_some_and(|l| l.has_field("ts_start") && l.has_field("ts_stop"));

    table
        .decode_gc_stats(raw, 0)
        .into_iter()
        .filter(|s| !(has_ts && s.ts_stop() < s.ts_start()))
        .map(|s| GcEntry {
            generation: s.generation,
            index: s.index,
            byte_offset: table.entry_byte_offset(s.generation, s.index).unwrap_or(0),
            start_ts: s.ts_start(),
            stop_ts: s.ts_stop(),
            collections: s.collections(),
            collected: s.collected(),
            uncollectable: s.uncollectable(),
            candidates: s.candidates(),
            duration: s.duration(),
            heap_size: s.heap_size(),
        })
        .collect()
}

/// Bucket entries by generation, dropping ring positions that were never written.
///
/// A ring position CPython has not filled yet decodes as all-zeros, and `collections == 0` is
/// the reliable marker for it: the counter is incremented before the entry is published, so a
/// generation's first real entry always carries `collections == 1`. Keeping the zeros would let
/// `min_by_key` anchor the window at collection 0 and silently turn a ring-window statistic into
/// a process-lifetime one — plausible-looking output either way, which is exactly why it has to
/// be excluded here rather than eyeballed downstream.
///
/// Out-of-range generations (a layout/geometry mismatch can decode one) are dropped too, so the
/// caller can index the returned array unconditionally.
fn entries_by_generation(entries: &[GcEntry]) -> [Vec<&GcEntry>; 3] {
    let mut gen_entries: [Vec<&GcEntry>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for entry in entries {
        if entry.collections == 0 {
            continue;
        }
        let g = entry.generation as usize;
        if g < 3 {
            gen_entries[g].push(entry);
        }
    }
    gen_entries
}

/// Compute average collection pause time per generation from a single snapshot.
/// Uses the full ring range: `(max.duration - min.duration) / (max.collections - min.collections)`.
/// The min/max are taken **by `collections` value, never by entry index**: a ring wraps, so
/// within one generation storage order runs e.g. `10, 11, 12, 5, 6, 7` — the oldest entry sits
/// mid-buffer, and an index-order read would span the wrong pair (or a negative delta).
/// Returns `[None; 3]` when the entry layout has no `duration` field (e.g. inline 3.13/3.14):
/// the pause time is unrecoverable externally, so the summary renders "n/a" rather than a
/// fake 0. Gens with <2 entries stay `Some(0.0)` (formatted like before).
pub fn avg_collection_time_per_gen(entries: &[GcEntry], has_duration: bool) -> [Option<f64>; 3] {
    if !has_duration {
        return [None, None, None];
    }
    let gen_entries = entries_by_generation(entries);

    let mut avgs = [Some(0.0f64); 3];
    for (g, gentries) in gen_entries.iter().enumerate() {
        if gentries.len() < 2 {
            continue;
        }
        let min_coll = gentries.iter().min_by_key(|s| s.collections).unwrap();
        let max_coll = gentries.iter().max_by_key(|s| s.collections).unwrap();

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
/// As in [`avg_collection_time_per_gen`], every min/max is taken **by value, not by entry
/// index**, so a wrapped ring (`10, 11, 12, 5, 6, 7`) still yields the true window.
/// Returns `[None; 3]` when the entry layout has no `ts_start`/`ts_stop` fields (e.g. inline
/// 3.13/3.14): there is no time base in a single snapshot, so the summary renders "n/a"
/// rather than a fake 0. Gens with <2 entries stay `Some(0.0)` (formatted like before).
pub fn collections_rate_from_entries(
    entries: &[GcEntry],
    has_timestamps: bool,
) -> [Option<f64>; 3] {
    if !has_timestamps {
        return [None, None, None];
    }
    let gen_entries = entries_by_generation(entries);

    let mut rates = [Some(0.0f64); 3];
    for (g, gentries) in gen_entries.iter().enumerate() {
        if gentries.len() < 2 {
            continue;
        }
        let min_coll = gentries.iter().min_by_key(|s| s.collections).unwrap();
        let max_coll = gentries.iter().max_by_key(|s| s.collections).unwrap();
        let min_ts = gentries.iter().min_by_key(|s| s.start_ts).unwrap();
        let max_ts = gentries.iter().max_by_key(|s| s.stop_ts).unwrap();

        let coll_delta = max_coll.collections - min_coll.collections;
        let ts_delta_ns = max_ts.stop_ts - min_ts.start_ts;

        if ts_delta_ns > 0 && coll_delta > 0 {
            rates[g] = Some(coll_delta as f64 / (ts_delta_ns as f64 / 1_000_000_000.0));
        }
    }
    rates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::GcItemLayout;
    use crate::remote_debugging::offsets::pre_3_13;

    fn entry(
        generation: u32,
        collections: i64,
        duration: f64,
        start_ts: i64,
        stop_ts: i64,
    ) -> GcEntry {
        GcEntry {
            generation,
            index: 0,
            byte_offset: 0,
            start_ts,
            stop_ts,
            collections,
            collected: 0,
            uncollectable: 0,
            candidates: 0,
            duration,
            heap_size: 0,
        }
    }

    fn put_i64(buf: &mut [u8], off: usize, v: i64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    // ── CollectRequest presets ──────────────────────────────────

    /// `all` collects every layer; `tui` matches it today (the renderer draws all three);
    /// `gc_stats_only` collects just the stats layer and skips the two struct dumps.
    #[test]
    fn collect_request_presets_name_the_expected_layers() {
        let all = CollectRequest::all();
        assert!(all.debug_offsets && all.gc_state && all.gc_stats);

        let tui = CollectRequest::tui();
        assert!(tui.debug_offsets && tui.gc_state && tui.gc_stats);

        let lean = CollectRequest::gc_stats_only();
        assert!(!lean.debug_offsets && !lean.gc_state && lean.gc_stats);
    }

    // ── avg_collection_time_per_gen ─────────────────────────────

    /// Inline builds (3.13/3.14) carry no `duration` field, so the pause time is
    /// unrecoverable from a single external snapshot — every generation must report
    /// `None` (rendered "n/a"), never a fake 0.
    #[test]
    fn avg_collection_time_is_none_without_the_duration_field() {
        let entries = vec![entry(0, 1, 5.0, 0, 0), entry(0, 3, 15.0, 0, 0)];
        assert_eq!(
            avg_collection_time_per_gen(&entries, false),
            [None, None, None]
        );
    }

    /// With duration, the average is `Δduration / Δcollections` across the ring's
    /// min/max-collections entries; a generation with fewer than two entries stays 0.0.
    #[test]
    fn avg_collection_time_divides_duration_delta_by_collection_delta() {
        // gen0: collections 2..6 (Δ4), duration 10..30 (Δ20) → 5.0.
        let entries = vec![entry(0, 2, 10.0, 0, 0), entry(0, 6, 30.0, 0, 0)];
        let avg = avg_collection_time_per_gen(&entries, true);
        assert_eq!(avg[0], Some(5.0));
        assert_eq!(avg[1], Some(0.0), "gen1 has <2 entries");
        assert_eq!(avg[2], Some(0.0));
    }

    /// Generations do not come with equal entry counts: a GIL ring is `[11, 3, 3]`, and a
    /// young process leaves the older rings only partly written. Each generation must be
    /// averaged over its own entries alone — a populous gen0 must not leak into gen1/gen2,
    /// and a gen with a single entry stays 0.0 while its neighbours still compute.
    #[test]
    fn avg_collection_time_averages_each_generation_over_its_own_entries() {
        let entries = vec![
            // gen0: 4 entries, collections 1..7 (Δ6), duration 2..20 (Δ18) → 3.0.
            entry(0, 1, 2.0, 0, 0),
            entry(0, 3, 8.0, 0, 0),
            entry(0, 5, 14.0, 0, 0),
            entry(0, 7, 20.0, 0, 0),
            // gen1: 2 entries, Δcollections 2, Δduration 9.0 → 4.5.
            entry(1, 4, 1.0, 0, 0),
            entry(1, 6, 10.0, 0, 0),
            // gen2: 1 entry — no delta to take.
            entry(2, 9, 100.0, 0, 0),
        ];
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(3.0), Some(4.5), Some(0.0)]
        );
    }

    /// The decoder emits entries in ring order, not grouped by generation, and the sparse
    /// generation can be the *young* one (gen0 wrapped to a single live entry while the
    /// older rings hold several). Bucketing is by the `generation` field, so interleaving
    /// and an inverted population must not change any result.
    #[test]
    fn avg_collection_time_buckets_interleaved_entries_by_generation() {
        let entries = vec![
            entry(2, 10, 100.0, 0, 0),
            entry(1, 2, 4.0, 0, 0),
            entry(0, 5, 50.0, 0, 0), // gen0: the lone entry → 0.0
            entry(2, 14, 140.0, 0, 0),
            entry(1, 5, 13.0, 0, 0),
            entry(2, 12, 116.0, 0, 0),
        ];
        // gen1: Δcollections 3, Δduration 9.0 → 3.0. gen2: Δcollections 4, Δduration 40.0 → 10.0.
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(0.0), Some(3.0), Some(10.0)]
        );
    }

    /// `GcEntry::generation` is a raw decoded `u32`; a layout/geometry mismatch can hand us
    /// an out-of-range generation. Those entries are dropped, never indexed into the
    /// 3-element array, and the valid generations still compute.
    #[test]
    fn avg_collection_time_ignores_out_of_range_generations() {
        let entries = vec![
            entry(3, 1, 1.0, 0, 0),
            entry(7, 9, 9.0, 0, 0),
            entry(0, 2, 4.0, 0, 0),
            entry(0, 4, 12.0, 0, 0),
        ];
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(4.0), Some(0.0), Some(0.0)]
        );
    }

    /// Two entries but no new collections between them (Δcollections == 0) can't yield a
    /// meaningful average — it stays 0.0 rather than dividing by zero.
    #[test]
    fn avg_collection_time_is_zero_when_no_new_collections() {
        let entries = vec![entry(0, 5, 10.0, 0, 0), entry(0, 5, 30.0, 0, 0)];
        assert_eq!(avg_collection_time_per_gen(&entries, true)[0], Some(0.0));
    }

    /// A ring wraps, so within one generation `collections` is non-monotonic in storage
    /// order — position 0 can hold the newest entry while the oldest sits mid-buffer, e.g.
    /// `[10, 11, 12, 5, 6, 7, 8, 9]`. The window must therefore span the min/max
    /// *collections values*, not the first/last entries by index. Durations here are
    /// deliberately uneven so an index-order reading (6.0 / 13.0) would fail.
    #[test]
    fn avg_collection_time_spans_the_ring_across_a_wrap() {
        let entries = vec![
            // gen0: 8 entries, wrapped after 12 — collections 10,11,12 then 5..9.
            entry(0, 10, 50.0, 0, 0),
            entry(0, 11, 56.0, 0, 0),
            entry(0, 12, 63.0, 0, 0), // newest
            entry(0, 5, 7.0, 0, 0),   // oldest
            entry(0, 6, 14.0, 0, 0),
            entry(0, 7, 26.0, 0, 0),
            entry(0, 8, 38.0, 0, 0),
            entry(0, 9, 44.0, 0, 0),
            // gen1: 3 entries, wrapped after 21 — collections 21 then 19, 20.
            entry(1, 21, 70.0, 0, 0), // newest
            entry(1, 19, 51.0, 0, 0), // oldest
            entry(1, 20, 57.0, 0, 0),
            // gen2: 1 entry.
            entry(2, 3, 12.0, 0, 0),
        ];
        // gen0: Δcollections 12-5 = 7, Δduration 63.0-7.0 = 56.0 → 8.0.
        // gen1: Δcollections 21-19 = 2, Δduration 70.0-51.0 = 19.0 → 9.5.
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(8.0), Some(9.5), Some(0.0)]
        );
    }

    /// A ring is allocated full-size and filled in over time, so a young process leaves
    /// trailing positions all-zero. `collections == 0` never occurs for a real entry (the
    /// first one is always 1), so those positions are unwritten and must not anchor the
    /// window — otherwise the average silently becomes a process-lifetime one over a
    /// phantom collection 0. Each generation fills at its own pace, so the counts differ.
    #[test]
    fn avg_collection_time_ignores_unwritten_ring_positions() {
        let entries = vec![
            // gen0: 3 written of 6 — collections 4..8, duration 20..44 → Δ4/Δ24 → 6.0.
            entry(0, 4, 20.0, 0, 0),
            entry(0, 6, 30.0, 0, 0),
            entry(0, 8, 44.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
            // gen1: 2 written of 3 — Δcollections 1, Δduration 7.0 → 7.0.
            entry(1, 1, 9.0, 0, 0),
            entry(1, 2, 16.0, 0, 0),
            entry(1, 0, 0.0, 0, 0),
            // gen2: never collected — every position unwritten.
            entry(2, 0, 0.0, 0, 0),
            entry(2, 0, 0.0, 0, 0),
            entry(2, 0, 0.0, 0, 0),
        ];
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(6.0), Some(7.0), Some(0.0)]
        );
    }

    /// A single written entry beside unwritten positions is still a single entry — there is
    /// no second point to take a delta against, so it stays 0.0 rather than pairing with a
    /// phantom collection 0 at duration 0 (which would report the whole cumulative duration
    /// as one collection's pause).
    #[test]
    fn avg_collection_time_does_not_pair_a_lone_entry_with_an_unwritten_one() {
        let entries = vec![
            entry(0, 3, 90.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
        ];
        // Anchoring at the zero position would give 90.0/3 = 30.0.
        assert_eq!(avg_collection_time_per_gen(&entries, true)[0], Some(0.0));
    }

    /// The zero-Δcollections guard is per generation too. A gen whose entries all report
    /// the same `collections` has no denominator and stays 0.0 however many entries it
    /// holds and however far its `duration` spreads, while a sparser gen beside it still
    /// divides normally.
    #[test]
    fn avg_collection_time_is_zero_only_for_the_generation_with_no_new_collections() {
        let entries = vec![
            // gen0: 3 entries, duration spread 10..90 but collections pinned at 5 → Δ0.
            entry(0, 5, 10.0, 0, 0),
            entry(0, 5, 30.0, 0, 0),
            entry(0, 5, 90.0, 0, 0),
            // gen1: 2 entries, Δcollections 4, Δduration 20.0 → 5.0.
            entry(1, 2, 4.0, 0, 0),
            entry(1, 6, 24.0, 0, 0),
            // gen2: 1 entry — no delta to take.
            entry(2, 9, 100.0, 0, 0),
        ];
        assert_eq!(
            avg_collection_time_per_gen(&entries, true),
            [Some(0.0), Some(5.0), Some(0.0)]
        );
    }

    // ── collections_rate_from_entries ─────────────────────────────

    /// No timestamps in the entry layout → no time base in a single snapshot → `None`
    /// (rendered "n/a"), not a fabricated 0.
    #[test]
    fn collections_rate_is_none_without_timestamps() {
        let entries = vec![entry(0, 1, 0.0, 0, 100), entry(0, 5, 0.0, 0, 100)];
        assert_eq!(
            collections_rate_from_entries(&entries, false),
            [None, None, None]
        );
    }

    /// The rate is `Δcollections / seconds`, where seconds spans the min `start_ts` to
    /// the max `stop_ts`. 4 collections over 2s (2e9 ns) → 2.0/s.
    #[test]
    fn collections_rate_is_collections_over_elapsed_seconds() {
        let entries = vec![entry(0, 1, 0.0, 0, 0), entry(0, 5, 0.0, 0, 2_000_000_000)];
        let rate = collections_rate_from_entries(&entries, true);
        assert_eq!(rate[0], Some(2.0));
        assert_eq!(rate[1], Some(0.0), "gen1 has <2 entries");
    }

    /// Same uneven-population rule for the rate: each generation's window is its own
    /// min `start_ts` → max `stop_ts`, so a gen0 with many entries and a wide window must
    /// not set the denominator for a gen1 with two entries and a narrow one.
    #[test]
    fn collections_rate_uses_each_generations_own_window() {
        let entries = vec![
            // gen0: 3 entries, Δcollections 10 over 0..5s → 2.0/s.
            entry(0, 1, 0.0, 0, 1_000_000_000),
            entry(0, 7, 0.0, 2_000_000_000, 3_000_000_000),
            entry(0, 11, 0.0, 4_000_000_000, 5_000_000_000),
            // gen1: 2 entries, Δcollections 3 over 1s..4s → 1.0/s.
            entry(1, 1, 0.0, 1_000_000_000, 2_000_000_000),
            entry(1, 4, 0.0, 3_000_000_000, 4_000_000_000),
            // gen2: 1 entry — no window.
            entry(2, 8, 0.0, 0, 9_000_000_000),
        ];
        assert_eq!(
            collections_rate_from_entries(&entries, true),
            [Some(2.0), Some(1.0), Some(0.0)]
        );
    }

    /// Zero elapsed time (all timestamps equal) can't yield a rate — stays 0.0 rather
    /// than dividing by zero.
    #[test]
    fn collections_rate_is_zero_when_no_time_elapsed() {
        let entries = vec![entry(0, 1, 0.0, 5, 5), entry(0, 5, 0.0, 5, 5)];
        assert_eq!(collections_rate_from_entries(&entries, true)[0], Some(0.0));
    }

    /// Same wrap for the rate: timestamps go backwards mid-buffer along with `collections`,
    /// so the window is the min `start_ts` → max `stop_ts` *by value*. Reading the first and
    /// last entries by index would here give a negative span (gen0 starts at 4s and ends at
    /// 3.2s) and report 0.0.
    #[test]
    fn collections_rate_spans_the_ring_across_a_wrap() {
        let entries = vec![
            // gen0: 5 entries, wrapped after 9 — collections 8, 9 then 5, 6, 7.
            entry(0, 8, 0.0, 4_000_000_000, 4_200_000_000),
            entry(0, 9, 0.0, 4_800_000_000, 5_000_000_000), // newest
            entry(0, 5, 0.0, 1_000_000_000, 1_200_000_000), // oldest
            entry(0, 6, 0.0, 2_000_000_000, 2_200_000_000),
            entry(0, 7, 0.0, 3_000_000_000, 3_200_000_000),
            // gen1: 2 entries, wrapped — collections 12 then 11.
            entry(1, 12, 0.0, 1_500_000_000, 2_000_000_000), // newest
            entry(1, 11, 0.0, 0, 500_000_000),               // oldest
            // gen2: 1 entry.
            entry(2, 2, 0.0, 0, 9_000_000_000),
        ];
        // gen0: Δcollections 4 over 1s..5s → 1.0/s. gen1: Δcollections 1 over 0..2s → 0.5/s.
        assert_eq!(
            collections_rate_from_entries(&entries, true),
            [Some(1.0), Some(0.5), Some(0.0)]
        );
    }

    /// Unwritten positions carry `ts_start == ts_stop == 0` too, so leaving them in would
    /// stretch every window back to timestamp 0 and crush the rate toward zero. They are
    /// dropped by the same `collections == 0` marker, per generation.
    #[test]
    fn collections_rate_ignores_unwritten_ring_positions() {
        let entries = vec![
            // gen0: 3 written of 5 — Δcollections 6 over 4s..7s → 2.0/s.
            entry(0, 2, 0.0, 4_000_000_000, 4_500_000_000),
            entry(0, 5, 0.0, 5_000_000_000, 5_500_000_000),
            entry(0, 8, 0.0, 6_500_000_000, 7_000_000_000),
            entry(0, 0, 0.0, 0, 0),
            entry(0, 0, 0.0, 0, 0),
            // gen1: 2 written of 4 — Δcollections 1 over 4s..6s → 0.5/s.
            entry(1, 1, 0.0, 4_000_000_000, 4_500_000_000),
            entry(1, 2, 0.0, 5_500_000_000, 6_000_000_000),
            entry(1, 0, 0.0, 0, 0),
            entry(1, 0, 0.0, 0, 0),
            // gen2: never collected.
            entry(2, 0, 0.0, 0, 0),
        ];
        // With the zeros left in, gen0 would span 0..7s and report 8/7 ≈ 1.14/s.
        assert_eq!(
            collections_rate_from_entries(&entries, true),
            [Some(2.0), Some(0.5), Some(0.0)]
        );
    }

    /// The degenerate window is per generation too. A gen with several entries that all
    /// share one timestamp has no time base and stays 0.0 no matter how many collections
    /// it counted, while a *less* populated gen alongside it still reports a real rate.
    #[test]
    fn collections_rate_is_zero_only_for_the_generation_with_no_time_elapsed() {
        let entries = vec![
            // gen0: 2 entries, Δcollections 3 over 0..2s → 1.5/s.
            entry(0, 1, 0.0, 0, 1_000_000_000),
            entry(0, 4, 0.0, 1_000_000_000, 2_000_000_000),
            // gen1: 3 entries, Δcollections 8 but every timestamp identical → no window.
            entry(1, 1, 0.0, 7, 7),
            entry(1, 4, 0.0, 7, 7),
            entry(1, 9, 0.0, 7, 7),
            // gen2: 1 entry — a wide span, but a single entry is never a delta.
            entry(2, 5, 0.0, 0, 5_000_000_000),
        ];
        assert_eq!(
            collections_rate_from_entries(&entries, true),
            [Some(1.5), Some(0.0), Some(0.0)]
        );
    }

    // ── parse_gc_entries / parse_entry ─────────────────────────────

    /// A layout carrying timestamps, so the torn-entry guard is live. Built by hand
    /// (not `set_ring`, which is private to the offsets module) — three 1-entry gens
    /// with the standard 8-byte inter-generation pad.
    /// Carries `collected`/`uncollectable` because the shared decoder
    /// (`decode_gc_stats`) treats those as required core fields — every real GC layout
    /// has them; a synthetic one that omits them isn't representative.
    static TS_LAYOUT: GcItemLayout = GcItemLayout {
        item_size: 40,
        fields: &[
            ("ts_start", 0),
            ("ts_stop", 8),
            ("collections", 16),
            ("collected", 24),
            ("uncollectable", 32),
        ],
    };

    fn ts_ring_table() -> offsets::offset_table::OffsetTable {
        let mut t = pre_3_13::table_for_version(3, 12).unwrap();
        t.gc_layout = Some(&TS_LAYOUT);
        t.gc_item_size = Some(40);
        t.gc_entries_per_gen = Some([1, 1, 1]);
        t.gc_gen_base_offsets = Some([0, 48, 96]); // 40-byte entry + 8-byte cursor per gen
        t
    }

    /// A ring entry whose `stop_ts < start_ts` is a torn read (a concurrent writer left
    /// the entry half-updated) and must be dropped, not decoded into garbage numbers.
    /// Only that generation's entry disappears; the intact ones survive with their fields.
    #[test]
    fn parse_drops_torn_ring_entries_but_keeps_intact_ones() {
        let table = ts_ring_table();
        let bases = table.gc_gen_base_offsets.unwrap();
        let mut raw = vec![0u8; bases[2] as usize + 40];

        // gen0: torn — stop_ts (50) < start_ts (100).
        put_i64(&mut raw, bases[0] as usize, 100);
        put_i64(&mut raw, bases[0] as usize + 8, 50);
        // gen1: intact, collections = 7.
        put_i64(&mut raw, bases[1] as usize, 100);
        put_i64(&mut raw, bases[1] as usize + 8, 200);
        put_i64(&mut raw, bases[1] as usize + 16, 7);
        // gen2: intact.
        put_i64(&mut raw, bases[2] as usize, 300);
        put_i64(&mut raw, bases[2] as usize + 8, 400);

        let entries = parse_gc_entries(&raw, &table);
        assert_eq!(entries.len(), 2, "the torn gen0 entry must be dropped");
        assert!(entries.iter().all(|s| s.generation != 0));
        let g1 = entries.iter().find(|s| s.generation == 1).unwrap();
        assert_eq!(g1.collections, 7);
        assert_eq!((g1.start_ts, g1.stop_ts), (100, 200));
    }

    /// Inline layouts (3.8–3.14) carry no timestamps, so the torn guard is a no-op and
    /// every generation's entry is kept even from an all-zero buffer.
    #[test]
    fn parse_keeps_every_entry_when_the_layout_has_no_timestamps() {
        let table = pre_3_13::table_for_version(3, 12).unwrap();
        let bases = table.gc_gen_base_offsets.unwrap();
        let item = table.gc_item_size.unwrap() as usize;
        let raw = vec![0u8; bases[2] as usize + item];
        let entries = parse_gc_entries(&raw, &table);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|s| s.start_ts == 0 && s.stop_ts == 0));
    }

    /// A field the layout doesn't define reads back as 0, not a random offset — the
    /// legacy layout has no `heap_size`/`duration`, so those stay zero.
    #[test]
    fn parse_reads_zero_for_fields_absent_from_the_layout() {
        let table = pre_3_13::table_for_version(3, 12).unwrap();
        let bases = table.gc_gen_base_offsets.unwrap();
        let item = table.gc_item_size.unwrap() as usize;
        let raw = vec![0xffu8; bases[2] as usize + item]; // all-ones payload
        let entries = parse_gc_entries(&raw, &table);
        // collections IS in the legacy layout, so it reads the 0xff bytes; heap_size
        // and duration are NOT, so they stay at the zero default.
        assert_ne!(entries[0].collections, 0);
        assert_eq!(entries[0].heap_size, 0);
        assert_eq!(entries[0].duration, 0.0);
    }

    /// One source of truth: the diagram and the monitor decode the same bytes into the
    /// same numbers because they share one decoder (`OffsetTable::decode_gc_stats`). The
    /// only sanctioned divergence is the diagram's torn-entry drop; everything else agrees
    /// field-for-field, and the diagram's `byte_offset` matches the table geometry the
    /// decoder walked.
    #[test]
    fn diagram_and_monitor_decode_agree_except_for_torn_entries() {
        let table = ts_ring_table();
        let bases = table.gc_gen_base_offsets.unwrap();
        let item = table.gc_item_size.unwrap() as usize;
        let mut raw = vec![0u8; bases[2] as usize + item];

        // One intact entry per generation (stop_ts >= start_ts), distinct collections.
        for (g, &base) in bases.iter().enumerate() {
            put_i64(&mut raw, base as usize, 100 + g as i64); // ts_start
            put_i64(&mut raw, base as usize + 8, 200 + g as i64); // ts_stop
            put_i64(&mut raw, base as usize + 16, 10 + g as i64); // collections
        }

        let monitor = table.decode_gc_stats(&raw, 0); // Vec<GcStat>
        let diagram = parse_gc_entries(&raw, &table); // Vec<GcEntry>

        // No torn entries → identical population, field-for-field.
        assert_eq!(monitor.len(), diagram.len());
        for (m, d) in monitor.iter().zip(&diagram) {
            assert_eq!((m.generation, m.index), (d.generation, d.index));
            assert_eq!(m.ts_start(), d.start_ts);
            assert_eq!(m.ts_stop(), d.stop_ts);
            assert_eq!(m.collections(), d.collections);
            assert_eq!(m.collected(), d.collected);
            assert_eq!(m.uncollectable(), d.uncollectable);
            // The diagram recovers the exact raw-region offset the decoder walked.
            assert_eq!(
                d.byte_offset,
                table.entry_byte_offset(d.generation, d.index).unwrap()
            );
        }

        // Tear gen0's entry (stop_ts < start_ts). The monitor keeps every entry (it dedups
        // downstream); the diagram drops the torn one so it never renders garbage.
        put_i64(&mut raw, bases[0] as usize + 8, 0);
        let monitor_torn = table.decode_gc_stats(&raw, 0);
        let diagram_torn = parse_gc_entries(&raw, &table);
        assert_eq!(
            monitor_torn.len(),
            monitor.len(),
            "monitor keeps torn entries"
        );
        assert_eq!(
            diagram_torn.len(),
            diagram.len() - 1,
            "diagram drops the torn entry"
        );
        assert!(diagram_torn.iter().all(|s| s.generation != 0));
    }
}
