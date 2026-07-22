//! Shared unit-test fixtures for the TUI module. The pre-3.13 (`Legacy`) tier is the only
//! one constructible without a live process (flat `OffsetTable`, no bindgen struct), so
//! `legacy_data` is the synthetic snapshot every section builder's tests render against.
//! Kept in one place so the `frame`/`layout`/`sections`/`gc_view` test modules can't drift.
use std::sync::Arc;
use std::time::Duration;

use ratatui::text::Line;

use crate::remote_debugging::offsets::pre_3_13;
use crate::remote_debugging::session::Resolved;
use crate::snapshot::collect::{
    CollectedData, GcEntry, GcStatsSnapshot, GcSubState, InterpreterSnapshot,
};

/// A synthetic Legacy (pre-3.13) snapshot — the only tier buildable off a live process.
pub(super) fn legacy_data(with_entries: bool) -> CollectedData {
    let table = pre_3_13::table_for_version(3, 12).unwrap();
    let entries = if with_entries {
        vec![GcEntry {
            generation: 0,
            index: 0,
            byte_offset: 0,
            start_ts: 0,
            stop_ts: 0,
            collections: 5,
            collected: 10,
            uncollectable: 0,
            candidates: 3,
            duration: 0.0,
            heap_size: 0,
        }]
    } else {
        Vec::new()
    };
    CollectedData {
        pid: 4321,
        runtime_addr: 0x5000,
        runtime_version: 0x030c0000,
        runtime_raw_bytes: Vec::new(),
        debug_offsets_size: 0,
        resolved: Arc::new(Resolved::Legacy { table }),
        interpreter: InterpreterSnapshot {
            addr: 0x6000,
            gc: GcSubState {
                raw_bytes: vec![0u8; 64],
                generation_stats: GcStatsSnapshot {
                    stats_addr: if with_entries { 0x7000 } else { 0 },
                    stats_size: 72,
                    item_size: 24,
                    entries_per_gen: [1, 1, 1],
                    has_timestamps: false,
                    has_duration: false,
                    raw_stats_bytes: vec![0u8; 72],
                    entries,
                },
            },
            gc_offset: 0x80,
            gc_size: 64,
            id: 0,
            next_addr: 0,
        },
        collect_duration: Duration::from_millis(1),
    }
}

pub(super) fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

pub(super) fn join_lines(lines: &[Line]) -> String {
    lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
}
