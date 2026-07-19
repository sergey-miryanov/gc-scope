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
