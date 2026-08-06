use std::collections::{HashMap, HashSet};

use crate::monitor::convert::convert_record;
use crate::monitor::cursor::Cursor;
use crate::monitor::exporters::{EventsExporter, ProcessLifecycle};
use crate::monitor::run_loop::PollStatus;
use crate::remote_debugging::session::{PySession, Revalidated};

/// Per-process polling context.
///
/// The multi-PID sibling of [`crate::snapshot::poller::SnapshotPoller`]: where that owns one
/// session and *returns* a full snapshot, this owns a `HashMap<u32, PySession>` and *emits*
/// deduped trace events into an `EventsExporter`. Both share the same `Fresh/Changed/Dead`
/// revalidate ladder (see [`poll`](Self::poll)).
///
/// Owns the exporter and, per PID, an attached [`PySession`] (resolved once and
/// reused every tick) plus lifecycle/last-timestamp state. All per-PID state is
/// evicted together in [`MonitorContext::mark_died`] — the single death path
/// `run_loop::run_loop` funnels every give-up through (C7).
pub struct MonitorContext<'a> {
    exporter: &'a mut dyn EventsExporter,
    /// Resolved session per PID. Attached lazily on first `poll`; a failed attach
    /// is NOT cached (so a not-yet-ready process is retried per the `WaitPolicy`).
    sessions: HashMap<u32, PySession>,
    /// Which Records have already been reported, keyed on CPython's cumulative
    /// `collections` per `(pid, interpreter, generation)`. It also carries what each ring
    /// did against what was read of it, which is what makes Loss recoverable.
    cursor: Cursor,
    alive_pids: HashSet<u32>,
}

impl<'a> MonitorContext<'a> {
    pub fn new(exporter: &'a mut dyn EventsExporter) -> Self {
        MonitorContext {
            exporter,
            sessions: HashMap::new(),
            cursor: Cursor::new(),
            alive_pids: HashSet::new(),
        }
    }

    /// Test hook: install a pre-built (and possibly fault-armed) session for `pid`
    /// so a test can drive [`poll`](Self::poll) against a known live session
    /// instead of one lazily attached inside `poll`. Compiled only under the
    /// `test-hooks` feature; not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn insert_session_for_test(&mut self, pid: u32, session: PySession) {
        self.sessions.insert(pid, session);
    }

    /// Read GC stats for `pid` and emit new events to the exporter.
    ///
    /// Returns `PollStatus::Ok` on success, `PollStatus::InvalidProcess`
    /// if the process cannot be attached or its stats cannot be read. The
    /// retry-vs-give-up decision belongs to the caller's `WaitPolicy`, not here.
    ///
    /// Manages lifecycle: emits `Started` on first successful poll,
    /// `Died` on first failure after success.
    pub fn poll(&mut self, pid: u32) -> PollStatus {
        // Attach-if-absent — resolve once, reuse every tick. A failed attach is
        // an invalid process for this tick and is deliberately not cached. (The
        // `entry` API can't express the early return on a fallible attach.)
        #[allow(clippy::map_entry)]
        if !self.sessions.contains_key(&pid) {
            match PySession::attach(pid) {
                Ok(session) => {
                    self.sessions.insert(pid, session);
                }
                Err(_) => return PollStatus::InvalidProcess,
            }
        }

        let stats = match self.sessions.get(&pid).unwrap().gc_stats(false) {
            Ok(stats) => stats,
            Err(_) => {
                // The read failed. Distinguish a stale/reused PID from a dead one
                // via revalidate; the WaitPolicy still owns retry-vs-give-up.
                match self.sessions.get_mut(&pid).unwrap().revalidate() {
                    Revalidated::Fresh => {
                        // Soft re-attached (fresh handle + runtime addr): retry once.
                        match self.sessions.get(&pid).unwrap().gc_stats(false) {
                            Ok(stats) => stats,
                            Err(_) => return self.on_invalid(pid),
                        }
                    }
                    Revalidated::Changed => {
                        // A different program holds this PID now: drop the stale
                        // session AND its cursor so the next tick re-attaches from
                        // scratch and reads the new occupant's counters as new.
                        // NOTE: this is the one poll branch with no automated test — it
                        // needs a *different* program to reuse the exact same PID between
                        // ticks, which can't be reproduced deterministically. The Fresh,
                        // Dead, and give-up paths are covered in tests/monitor.rs.
                        self.sessions.remove(&pid);
                        self.cursor.forget(pid);
                        return self.on_invalid(pid);
                    }
                    Revalidated::Dead => return self.on_invalid(pid),
                }
            }
        };

        if self.alive_pids.insert(pid) {
            self.exporter
                .mark_process_lifecycle(pid, ProcessLifecycle::Started, 0);
        }

        // Convert once here, so every output format is handed the same events instead of
        // deriving its own from the raw Record.
        for record in self.cursor.admit(pid, &stats) {
            self.exporter.add_events(&convert_record(pid, record));
        }
        PollStatus::Ok
    }

    /// Emit `Died` (once) if the PID was alive, and return `InvalidProcess`.
    /// Does not evict the session — that stays with `mark_died`, the single death
    /// path the `WaitPolicy` drives (§5.1). The one exception is a `Changed` PID,
    /// which `poll` evicts explicitly before calling this.
    fn on_invalid(&mut self, pid: u32) -> PollStatus {
        if self.alive_pids.remove(&pid) {
            self.exporter
                .mark_process_lifecycle(pid, ProcessLifecycle::Died, 0);
        }
        PollStatus::InvalidProcess
    }

    /// Mark a PID as died and evict all of its per-PID state.
    ///
    /// This is the single eviction point (C7): `run_loop` routes every give-up
    /// (vanished PID, policy-says-stop, shutdown) through here, so dropping the
    /// session + cursor here means no per-PID state can leak or go stale
    /// across a reused PID. No lifecycle event if the PID was never reported as
    /// started or was already marked dead.
    pub fn mark_died(&mut self, pid: u32) {
        self.sessions.remove(&pid);
        self.cursor.forget(pid);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::cursor::RingKey;
    use crate::monitor::exporters::chrome::ChromeTraceExporter;
    use crate::remote_debugging::gc_stats::GcStat;
    use crate::remote_debugging::offsets::offset_table::seq_layout;

    /// The single eviction point drops the dead PID's cursor, so a PID the OS recycles cannot
    /// read its predecessor's `collections` counters as already-seen and swallow the new
    /// process's first Collections.
    ///
    /// `poll` is what fills the cursor in a real run, and it needs a live target — see
    /// `tests/monitor.rs`. The eviction rule itself does not, so it is pinned here. An
    /// unopened `ChromeTraceExporter` stands in as an inert sink rather than a bespoke double
    /// whose methods this test would never call.
    #[test]
    fn marking_a_pid_dead_clears_its_cursor() {
        let mut exporter = ChromeTraceExporter::new();
        let mut context = MonitorContext::new(&mut exporter);

        let layout = seq_layout(&["collections"]);
        let record = GcStat::from_fields(0, 0, 0, layout, &[("collections", 9)]);
        context.cursor.admit(77, std::slice::from_ref(&record));

        let key = RingKey {
            pid: 77,
            interpreter: 0,
            generation: 0,
        };
        assert!(context.cursor.observation(key).is_some());

        context.mark_died(77);
        assert!(
            context.cursor.observation(key).is_none(),
            "a recycled PID must not inherit its predecessor's counters"
        );
    }
}
