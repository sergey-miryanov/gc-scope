//! Output formats. Each encodes the [`TraceEvent`]s it is handed; what a Collection looks
//! like in a trace is decided once, in [`crate::monitor::convert`].

pub mod chrome;
pub mod combining;
pub mod perfetto;
pub mod perfetto_proto;
pub mod protobuf;
pub mod timing;

use crate::monitor::trace_event::TraceEvent;
use std::path::{Path, PathBuf};

/// `Copy` so a fan-out can hand the same mark to each of its members
/// ([`combining::CombiningExporter`]); the enum carries nothing to clone.
#[derive(Clone, Copy)]
pub enum ProcessLifecycle {
    Started,
    Died,
}

pub trait EventsExporter {
    /// Where this encoder writes, given the path the operator asked for. The default is that
    /// path unchanged; an encoder whose bytes are not what the extension claims resolves its
    /// own, so a fan-out writes one file per format instead of several into one.
    ///
    /// The resolution is the *composer's* to apply — [`combining::CombiningExporter::open`]
    /// calls this and hands each member the answer — so an encoder used on its own still
    /// writes exactly where it is told.
    fn output_path(&self, requested: &Path) -> PathBuf {
        requested.to_path_buf()
    }

    fn open(&mut self, path: &Path) -> std::io::Result<()>;
    /// Write one batch of events, in the order given; [`TraceEvent`] states what an encoder
    /// may rely on. A batch is whatever the producer converted in one step, so batch
    /// boundaries mean nothing.
    fn add_events(&mut self, events: &[TraceEvent]);
    /// A process gcscope started or stopped watching. This is gcscope talking about itself
    /// rather than the target talking about a Collection — but it is on **the same clock**, the
    /// machine's monotonic counter, which is the one CPython times a Collection with
    /// ([ADR 0023](../../../docs/adr/0023-one-clock-read-per-platform.md)). An encoder places
    /// this reading exactly as it places a Record's, and there is nothing to rebase.
    ///
    /// `ts_ns` is `None` when there is no reading to give. An encoder must then write no
    /// timestamp rather than a zero: a zero is a point on the timeline, and one at the epoch
    /// puts the mark decades from everything around it
    /// ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)).
    fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: Option<i64>);
    fn close(&mut self) -> std::io::Result<()>;
}
