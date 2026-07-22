use std::collections::{HashMap, HashSet};

use crate::monitor::exporters::{EventsExporter, ProcessLifecycle};
use crate::monitor::run_loop::PollStatus;
use crate::remote_debugging::gc_stats::GcStat;
use crate::remote_debugging::session::{PySession, Revalidated};

/// Per-process polling context.
///
/// The multi-PID sibling of [`crate::snapshot::poller::SnapshotPoller`]: where that owns one
/// session and *returns* a full snapshot, this owns a `HashMap<u32, PySession>` and *emits*
/// deduped event deltas into an `EventsExporter`. Both share the same `Fresh/Changed/Dead`
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
    /// Per-PID, per-(generation, entry) timestamp high-water mark for event dedup
    /// (C4). `read_gc_stats` yields entries in generation-major order, not timestamp
    /// order, so a single per-PID mark would drop a fresh event in one entry after
    /// a higher timestamp was seen in another (across generations, or across a ring
    /// wrap within a generation). Tracking freshness per entry fixes that; each ring
    /// entry's `ts_start` only ever increases as it is overwritten.
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
                        // session AND its freshness marks so the next tick
                        // re-attaches from scratch and dedups against a clean slate.
                        // NOTE: this is the one poll branch with no automated test — it
                        // needs a *different* program to reuse the exact same PID between
                        // ticks, which can't be reproduced deterministically. The Fresh,
                        // Dead, and give-up paths are covered in tests/monitor.rs.
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

        for stat in select_fresh(&stats, self.seen.entry(pid).or_default()) {
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

/// Select the stats fresher than the last seen ts for their OWN `(generation, entry)`,
/// returned in timestamp order so the trace stays ordered regardless of the
/// generation-major order the entries arrive in (C4). Advances `seen` to the new marks.
///
/// `ts_start == 0` means an untouched entry (never collected) and is never selected —
/// the initial mark is 0 and selection is strictly greater-than.
fn select_fresh<'s>(stats: &'s [GcStat], seen: &mut HashMap<(u32, usize), i64>) -> Vec<&'s GcStat> {
    let mut fresh: Vec<&GcStat> = Vec::new();
    for stat in stats {
        let mark = seen.entry((stat.generation, stat.index)).or_insert(0);
        if stat.ts_start() > *mark {
            *mark = stat.ts_start();
            fresh.push(stat);
        }
    }
    fresh.sort_by_key(|s| s.ts_start());
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// Minimal entry layout for the dedup tests — they only ever set/read `ts_start`
    /// (`generation`/`entry` are identity fields on the view, not layout fields).
    static TEST_LAYOUT: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start"]));

    fn stat(generation: u32, index: usize, ts_start: i64) -> GcStat {
        GcStat::from_fields(
            generation,
            index,
            0,
            *TEST_LAYOUT,
            &[("ts_start", ts_start)],
        )
    }

    fn fresh_ts(stats: &[GcStat], seen: &mut HashMap<(u32, usize), i64>) -> Vec<i64> {
        select_fresh(stats, seen)
            .into_iter()
            .map(|s| s.ts_start())
            .collect()
    }

    /// Entries that have never been collected read back as zero. The initial mark is
    /// also zero and selection is strictly greater-than, so they must never be
    /// emitted — otherwise every attach floods the trace with phantom events at t=0.
    #[test]
    fn untouched_entries_are_never_emitted() {
        let mut seen = HashMap::new();
        let stats = vec![stat(0, 0, 0), stat(1, 0, 0), stat(2, 0, 0)];
        assert!(fresh_ts(&stats, &mut seen).is_empty());
        // Still nothing on a second poll of the same untouched entries.
        assert!(fresh_ts(&stats, &mut seen).is_empty());
    }

    #[test]
    fn a_entry_is_emitted_once_per_new_timestamp() {
        let mut seen = HashMap::new();
        assert_eq!(fresh_ts(&[stat(0, 0, 100)], &mut seen), vec![100]);
        // Same reading on the next tick: already seen.
        assert!(fresh_ts(&[stat(0, 0, 100)], &mut seen).is_empty());
        // The entry was overwritten by a newer collection.
        assert_eq!(fresh_ts(&[stat(0, 0, 150)], &mut seen), vec![150]);
    }

    /// The C4 regression. `read_gc_stats` yields entries generation-major, not in
    /// timestamp order, so a single per-PID high-water mark lets a high-timestamped
    /// generation-2 event swallow a genuinely new — but older — generation-0 event.
    /// The mark is per `(generation, entry)` precisely to stop that.
    #[test]
    fn a_high_timestamp_in_one_generation_does_not_mask_another() {
        let mut seen = HashMap::new();
        // Tick 1: only generation 2 has run, and it ran late.
        assert_eq!(fresh_ts(&[stat(2, 0, 900)], &mut seen), vec![900]);
        // Tick 2: generation 0 collected at t=100 — older than the gen-2 mark, but
        // new for its own entry. A per-PID mark would drop it.
        assert_eq!(
            fresh_ts(&[stat(0, 0, 100), stat(2, 0, 900)], &mut seen),
            vec![100]
        );
    }

    /// Same hazard inside one generation: the ring's entries are overwritten in turn,
    /// so entry 7 can hold an older timestamp than entry 3 and still be unreported.
    #[test]
    fn entries_within_a_generation_are_tracked_independently() {
        let mut seen = HashMap::new();
        assert_eq!(fresh_ts(&[stat(0, 3, 500)], &mut seen), vec![500]);
        assert_eq!(
            fresh_ts(&[stat(0, 3, 500), stat(0, 7, 200)], &mut seen),
            vec![200]
        );
    }

    /// Entries arrive generation-major but the trace must be ordered by time, or
    /// Perfetto renders the begin/end pairs out of sequence.
    #[test]
    fn output_is_sorted_by_timestamp_regardless_of_input_order() {
        let mut seen = HashMap::new();
        let stats = vec![
            stat(0, 0, 300),
            stat(0, 1, 100),
            stat(1, 0, 400),
            stat(2, 0, 200),
        ];
        assert_eq!(fresh_ts(&stats, &mut seen), vec![100, 200, 300, 400]);
    }

    #[test]
    fn marks_advance_only_for_the_entries_that_were_selected() {
        let mut seen = HashMap::new();
        let _ = fresh_ts(&[stat(0, 0, 100), stat(1, 0, 0)], &mut seen);
        assert_eq!(seen.get(&(0, 0)), Some(&100));
        // The zero-timestamp entry was still recorded (at 0) but never emitted, so a
        // later real collection in it is not suppressed.
        assert_eq!(seen.get(&(1, 0)), Some(&0));
        assert_eq!(fresh_ts(&[stat(1, 0, 50)], &mut seen), vec![50]);
    }
}
