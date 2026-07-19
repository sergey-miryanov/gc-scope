use anyhow::Result;
use crate::memory::reader;
use crate::remote_debugging::offsets;
use crate::remote_debugging::offsets::offset_table::GcStatsKind;
use crate::remote_debugging::version::PythonVersion;

#[allow(dead_code)]
pub struct GcStat {
    pub generation: u32,
    pub slot: usize,
    pub interpreter_id: i64,
    pub ts_start: i64,
    pub ts_stop: i64,
    pub collections: i64,
    pub collected: i64,
    pub uncollectable: i64,
    pub candidates: i64,
    pub duration: f64,
    pub heap_size: i64,
    pub increment_size: Option<i64>,
    pub alive_size: Option<i64>,
    pub finalized_garbage_count: Option<i64>,
    pub clear_weakrefs_count: Option<i64>,
    pub deleted_garbage_count: Option<i64>,
    pub ts_mark_alive_start: Option<i64>,
    pub ts_mark_alive_stop: Option<i64>,
    pub ts_fill_increment_start: Option<i64>,
    pub ts_fill_increment_stop: Option<i64>,
    pub ts_deduce_unreachable_start: Option<i64>,
    pub ts_deduce_unreachable_stop: Option<i64>,
    pub ts_handle_weakref_callbacks_start: Option<i64>,
    pub ts_handle_weakref_callbacks_stop: Option<i64>,
    pub ts_finalize_garbage_stop: Option<i64>,
    pub ts_handle_resurrected_stop: Option<i64>,
    pub ts_clear_weakrefs_stop: Option<i64>,
    pub ts_delete_garbage_start: Option<i64>,
    pub ts_delete_garbage_stop: Option<i64>,
}

pub fn read_gc_stats(pid: u32, version: &PythonVersion, all_interpreters: bool) -> Result<Vec<GcStat>> {
    let (runtime_addr, _stored, vo) = offsets::read_offsets(pid, version)?;
    let _ = vo.validate();

    let table = vo.to_offset_table(pid, runtime_addr);

    // Catch-all guard for an unregistered build: the process's own
    // `_Py_DebugOffsets.gc.generation_stats_size` records the TOTAL byte size of the
    // ring-buffer region. `to_offset_table` has already SELECTED the best-matching GC
    // layout for this hex (`select_gc_shape` — including picking between a clean release
    // and a `+inc` build that share a hex), so for any recognized build the reconstructed
    // total below equals `reported` and this stays silent. It fires only when selection
    // fell through to the nav variant's default with no matching candidate — i.e. a build
    // whose ring layout gcscope doesn't know — emitting a regeneration hint. Only
    // ring-buffer versions (3.15.0a8+) expose this field; inline versions (3.8–3.14)
    // report 0 here (skipped). A warning, not a hard error, because the +8 trailing-index
    // model is validated only against GIL builds; confirm a free-threaded build first.
    if table.gc_stats_kind == GcStatsKind::RingBuffer {
        let reported = vo.gc_generation_stats_size();
        if let (Some(item), Some(bases), Some(slots)) =
            (table.gc_item_size, table.gc_gen_base_offsets, table.gc_slots_per_gen)
        {
            let expected = bases[2] + slots[2] * item + 8;
            if reported != 0 && reported != expected {
                eprintln!(
                    "warning: gc_generation_stats size mismatch for {:#010x}: the process \
                     reports {reported} bytes but gcscope's compiled layout expects {expected}. \
                     This build's GC ring layout may differ from the registered one — \
                     regenerate offsets with scripts/gen-offsets.py against this exact build.",
                    table.version_hex
                );
            }
        }
    }

    let head_addr = read_u64(pid, runtime_addr + table.runtime_interpreters_head())?;
    let next_off = table.interp_next();
    let id_off = table.interp_id();
    let gc_off = table.interp_gc.unwrap_or(0);

    let mut stats = Vec::new();
    let mut current = head_addr;
    let mut first = true;
    while current != 0 {
        let iid = read_i64(pid, current + id_off)?;
        let gc_addr = current + gc_off; // this interpreter's `_gc_runtime_state`

        // Resolve this interpreter's stats address by its region shape.
        let stats_addr = match table.gc_stats_kind {
            GcStatsKind::None => None,
            GcStatsKind::InlineArray => {
                // Data lives inline at a version-specific offset from the gc state
                // (3.13 = 0x80, 3.14 = 0x78), computed per build by gen-offsets.py
                Some(gc_addr + table.gc_stats_inline_off)
            }
            GcStatsKind::RingBuffer => {
                // Follow the `gc.generation_stats` pointer in the gc state.
                let gen_stats_off = vo.gc_generation_stats();
                if gen_stats_off == 0 {
                    None
                } else {
                    let ptr = read_u64(pid, gc_addr + gen_stats_off)?;
                    // A NULL pointer is a normal transient state (stats not yet
                    // allocated, or teardown): skip this interpreter, never hang.
                    (ptr != 0).then_some(ptr)
                }
            }
        };

        if let Some(addr) = stats_addr {
            let mut interp_table = table.clone();
            interp_table.gc_stats_addr = Some(addr);
            stats.extend(interp_table.read_gc_stats(pid, iid));
        }

        // Always advance — the walk must make progress even for an interpreter
        // with no readable stats (this is what previously hung on NULL pointers).
        current = read_u64(pid, current + next_off)?;
        if first && !all_interpreters {
            break;
        }
        first = false;
    }

    Ok(stats)
}

fn read_u64(pid: u32, addr: u64) -> Result<u64> {
    let bytes = reader::read_memory(pid, addr, 8)?;
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn read_i64(pid: u32, addr: u64) -> Result<i64> {
    let bytes = reader::read_memory(pid, addr, 8)?;
    Ok(i64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

pub fn print_stats(stats: &[GcStat]) {
    if stats.is_empty() {
        println!("No GC stats found.");
        return;
    }

    // Determine if we have extended fields
    let has_extended = stats.iter().any(|s| s.increment_size.is_some());

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
                s.collections, s.collected, s.uncollectable,
                s.candidates, s.heap_size, s.duration,
                s.increment_size.unwrap_or(0),
                s.alive_size.unwrap_or(0),
                s.finalized_garbage_count.unwrap_or(0),
                s.clear_weakrefs_count.unwrap_or(0),
                s.deleted_garbage_count.unwrap_or(0),
                s.ts_mark_alive_start.unwrap_or(0),
            );
        } else {
            println!(
                "{:>3} {:>4} {:>6} {:>14} {:>14} {:>14} {:>14} {:>14} {:>10.6}",
                s.generation, s.slot, s.interpreter_id,
                s.collections, s.collected, s.uncollectable,
                s.candidates, s.heap_size, s.duration,
            );
        }
    }
}
