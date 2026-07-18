pub mod chrome;

use std::path::Path;
use crate::remote_debugging::gc_stats::GcStat;

pub enum ProcessLifecycle {
    Started,
    Died,
}

pub trait EventsExporter {
    fn open(&mut self, path: &Path) -> std::io::Result<()>;
    fn add_event(&mut self, pid: u32, event: &GcStat);
    fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: i64);
    fn close(&mut self) -> std::io::Result<()>;
}
