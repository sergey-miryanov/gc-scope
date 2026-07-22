//! Monitoring subsystem: attach to a live PID tree and stream deduped GC events.
//!
//! A consumer of `remote_debugging` (the runtime model), parallel to the `snapshot`
//! package: [`run_loop`] drives a [`MonitorContext`] across a discovered process tree,
//! polling each PID for new GC events and emitting them through the [`exporters`] layer.
//! CLI wiring lives in `crate::cli`, not here, so this stays a plain library with no
//! argument-parsing knowledge.

pub mod context;
pub mod exporters;
pub mod run_loop;

pub use context::MonitorContext;
pub use run_loop::{NoWaitPolicy, PollStatus, StartupTimeoutPolicy, WaitPolicy, run_loop};
