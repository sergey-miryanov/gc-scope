//! Snapshot reader layer: walk an attached [`PySession`] once and hand a consumer a
//! fully-owned point-in-time picture of the runtime.
//!
//! A consumer of `remote_debugging` (the runtime model), parallel to the `monitor`
//! package: where `monitor` streams deduped event deltas, `snapshot` produces one
//! [`CollectedData`](collect::CollectedData) snapshot per call. The `tui` renderer
//! is the sole consumer today. Both siblings sit on the same decode primitives in
//! `remote_debugging` (`OffsetTable::decode_gc_stats`, `PySession::gc_stats_region_addr`),
//! so a fix in reading benefits both.
//!
//! [`PySession`]: crate::remote_debugging::session::PySession

pub mod collect;
pub mod poller;
