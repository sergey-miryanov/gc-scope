//! Output formats. Each one encodes the [`TraceEvent`]s it is handed and nothing more —
//! what a Collection *is* in a trace is decided once, in [`crate::monitor::convert`].

pub mod chrome;
pub mod timing;

use crate::monitor::trace_event::TraceEvent;
use std::path::Path;

pub enum ProcessLifecycle {
    Started,
    Died,
}

pub trait EventsExporter {
    fn open(&mut self, path: &Path) -> std::io::Result<()>;
    /// Write one batch of events, in the order given — the ordering guarantees an encoder
    /// may rely on are on [`TraceEvent`]. A batch is whatever the producer converted in one
    /// step, so an encoder must not treat batch boundaries as meaningful.
    fn add_events(&mut self, events: &[TraceEvent]);
    fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: i64);
    fn close(&mut self) -> std::io::Result<()>;
}
