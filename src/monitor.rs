use std::collections::{HashMap, HashSet};

use crate::exporters::{EventsExporter, ProcessLifecycle};
use crate::monitor_loop::PollStatus;
use crate::remote_debugging::gc_stats;
use crate::remote_debugging::version::{self, PythonVersion};

/// Per-process polling context.
///
/// Owns the exporter and tracks per-PID lifecycle and last-observed timestamps.
pub struct MonitorContext<'a> {
    exporter: &'a mut dyn EventsExporter,
    last_ts: HashMap<u32, i64>,
    alive_pids: HashSet<u32>,
    versions: HashMap<u32, PythonVersion>,
}

impl<'a> MonitorContext<'a> {
    pub fn new(exporter: &'a mut dyn EventsExporter) -> Self {
        MonitorContext {
            exporter,
            last_ts: HashMap::new(),
            alive_pids: HashSet::new(),
            versions: HashMap::new(),
        }
    }

    /// Read GC stats for `pid` and emit new events to the exporter.
    ///
    /// Returns `PollStatus::Ok` on success, `PollStatus::InvalidProcess`
    /// if the process has no PyRuntime section or cannot be read.
    ///
    /// Manages lifecycle: emits `Started` on first successful poll,
    /// `Died` on first failure after success.
    pub fn poll(&mut self, pid: u32) -> PollStatus {
        let version = match self.versions.entry(pid) {
            std::collections::hash_map::Entry::Occupied(e) => *e.get(),
            std::collections::hash_map::Entry::Vacant(e) => match version::detect(pid) {
                Ok(v) => *e.insert(v),
                Err(_) => return PollStatus::InvalidProcess,
            },
        };

        match gc_stats::read_gc_stats(pid, &version, false) {
            Ok(stats) => {
                if self.alive_pids.insert(pid) {
                    self.exporter
                        .mark_process_lifecycle(pid, ProcessLifecycle::Started, 0);
                }
                let last = self.last_ts.entry(pid).or_insert(0);
                for stat in &stats {
                    if stat.ts_start > *last {
                        *last = stat.ts_start;
                        self.exporter.add_event(pid, stat);
                    }
                }
                PollStatus::Ok
            }
            Err(_) => {
                if self.alive_pids.remove(&pid) {
                    self.exporter
                        .mark_process_lifecycle(pid, ProcessLifecycle::Died, 0);
                }
                PollStatus::InvalidProcess
            }
        }
    }

    /// Mark a PID as died (e.g. vanished from children list).
    ///
    /// No-op if the PID was never reported as started or already marked dead.
    pub fn mark_died(&mut self, pid: u32) {
        if self.alive_pids.remove(&pid) {
            self.exporter
                .mark_process_lifecycle(pid, ProcessLifecycle::Died, 0);
        }
    }

    /// Close the underlying exporter.
    pub fn close(&mut self) -> std::io::Result<()> {
        self.exporter.close()
    }
}
