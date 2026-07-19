use std::collections::{HashMap, HashSet};

use crate::exporters::{EventsExporter, ProcessLifecycle};
use crate::monitor_loop::PollStatus;
use crate::remote_debugging::session::{PySession, Revalidated};

/// Per-process polling context.
///
/// Owns the exporter and, per PID, an attached [`PySession`] (resolved once and
/// reused every tick) plus lifecycle/last-timestamp state. All per-PID state is
/// evicted together in [`MonitorContext::mark_died`] — the single death path
/// `monitor_loop::run_loop` funnels every give-up through (C7).
pub struct MonitorContext<'a> {
    exporter: &'a mut dyn EventsExporter,
    /// Resolved session per PID. Attached lazily on first `poll`; a failed attach
    /// is NOT cached (so a not-yet-ready process is retried per the `WaitPolicy`).
    sessions: HashMap<u32, PySession>,
    /// Per-PID, per-(generation, slot) timestamp high-water mark for event dedup
    /// (C4). `read_gc_stats` yields slots in generation-major order, not timestamp
    /// order, so a single per-PID mark would drop a fresh event in one slot after
    /// a higher timestamp was seen in another (across generations, or across a ring
    /// wrap within a generation). Tracking freshness per slot fixes that; each ring
    /// slot's `ts_start` only ever increases as it is overwritten.
    seen: HashMap<u32, HashMap<(u32, usize), i64>>,
    alive_pids: HashSet<u32>,
}

impl<'a> MonitorContext<'a> {
    pub fn new(exporter: &'a mut dyn EventsExporter) -> Self {
        MonitorContext {
            exporter,
            sessions: HashMap::new(),
            seen: HashMap::new(),
            alive_pids: HashSet::new(),
        }
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
                        // session AND its freshness marks so the next tick
                        // re-attaches from scratch and dedups against a clean slate.
                        self.sessions.remove(&pid);
                        self.seen.remove(&pid);
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

        // Select events fresher than the last seen ts for their OWN slot, then
        // emit them in timestamp order so the trace stays ordered regardless of
        // the generation-major order the slots arrive in. `ts_start == 0` means an
        // untouched slot (never collected) — never emitted (the initial mark is 0).
        let seen = self.seen.entry(pid).or_default();
        let mut fresh: Vec<&_> = Vec::new();
        for stat in &stats {
            let mark = seen.entry((stat.generation, stat.slot)).or_insert(0);
            if stat.ts_start > *mark {
                *mark = stat.ts_start;
                fresh.push(stat);
            }
        }
        fresh.sort_by_key(|s| s.ts_start);
        for stat in fresh {
            self.exporter.add_event(pid, stat);
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
    /// session + timestamp cache here means no per-PID state can leak or go stale
    /// across a reused PID. No lifecycle event if the PID was never reported as
    /// started or was already marked dead.
    pub fn mark_died(&mut self, pid: u32) {
        self.sessions.remove(&pid);
        self.seen.remove(&pid);
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
