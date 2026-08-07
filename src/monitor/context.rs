use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::monitor::convert::convert_record;
use crate::monitor::cursor::Cursor;
use crate::monitor::exporters::{EventsExporter, ProcessLifecycle};
use crate::monitor::run_loop::PollStatus;
use crate::monitor::statistics::{InterpreterSummary, summarize};
use crate::monitor::trace_event::TraceEvent;
use crate::remote_debugging::gc_stats::GcStat;
use crate::remote_debugging::session::{PySession, Revalidated};

/// Per-process polling context.
///
/// The multi-PID sibling of [`crate::snapshot::poller::SnapshotPoller`]: where that owns one
/// session and *returns* a full snapshot, this owns a `HashMap<u32, PySession>` and *emits*
/// deduped trace events into an `EventsExporter`. Both share the same `Fresh/Changed/Dead`
/// revalidate ladder (see [`poll`](Self::poll)).
///
/// Owns the exporter and, per PID, an attached [`PySession`] (resolved once and
/// reused every tick) plus lifecycle state and the read cursor. All per-PID state is
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
    /// When monitoring began: the origin of the Observer's clock, read only for builds that
    /// publish no timestamps of their own. See [`observed_at_ns`](Self::observed_at_ns).
    started: Instant,
}

impl<'a> MonitorContext<'a> {
    pub fn new(exporter: &'a mut dyn EventsExporter) -> Self {
        MonitorContext {
            exporter,
            sessions: HashMap::new(),
            cursor: Cursor::new(),
            alive_pids: HashSet::new(),
            started: Instant::now(),
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

        let events = self.events_for(pid, &stats, self.observed_at_ns());
        self.exporter.add_events(&events);
        PollStatus::Ok
    }

    /// The poll seam: one read's Records in, the events an exporter receives out.
    ///
    /// Which Records are new (the cursor) and what each looks like in a trace (the
    /// conversion) both happen here, so a test drives them with scripted Records and no live
    /// interpreter (`docs/adr/0005-testing-strategy.md`). Converting here rather than per
    /// format also hands a fan-out to two formats the same events.
    ///
    /// `observed_at_ns` is the Observer's clock for this poll, shared by every Record it
    /// returns. A build publishing no timestamps has nothing else to place a sample on the
    /// timeline with; one that does ignores it.
    fn events_for(&mut self, pid: u32, stats: &[GcStat], observed_at_ns: i64) -> Vec<TraceEvent> {
        self.cursor
            .admit(pid, stats)
            .into_iter()
            .flat_map(|record| convert_record(pid, record, observed_at_ns))
            .collect()
    }

    /// The Observer's own clock, in nanoseconds since monitoring began. Not the wall clock: a
    /// trace starting near zero reads more easily, and nothing correlates these samples with
    /// anything outside the run.
    fn observed_at_ns(&self) -> i64 {
        self.started
            .elapsed()
            .as_nanos()
            .try_into()
            .unwrap_or(i64::MAX)
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
    ///
    /// What the PID's rings *did* survives, retired inside the cursor. `run_loop` marks the
    /// target dead before the run ends, so without that every completed run would summarize
    /// to nothing.
    pub fn mark_died(&mut self, pid: u32) {
        self.sessions.remove(&pid);
        self.cursor.forget(pid);
        if self.alive_pids.remove(&pid) {
            self.exporter
                .mark_process_lifecycle(pid, ProcessLifecycle::Died, 0);
        }
    }

    /// What the run read, per interpreter per generation.
    ///
    /// Folded as the Records were polled, so this reads the accumulator rather than the trace
    /// it wrote: the same figures a replayed stream of Records gives.
    pub fn summary(&self) -> Vec<InterpreterSummary> {
        summarize(&self.cursor)
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
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A build that publishes the pause timestamps, so its Records describe spans.
    static TIMED: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
            "candidates",
            "duration",
            "heap_size",
        ])
    });

    /// A build that publishes cumulative counts and no timing at all. The two layouts are
    /// how the tiers are expressed here: no test names a Python version, because nothing in
    /// the monitor reads one.
    static COUNTS_ONLY: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["collections", "collected", "uncollectable"]));

    /// A Record from a build with timing.
    fn timed(generation: u32, counter: i64, ts_start: i64, ts_stop: i64) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            0,
            *TIMED,
            &[
                ("collections", counter),
                ("ts_start", ts_start),
                ("ts_stop", ts_stop),
            ],
        )
    }

    /// A Record from a build with no timing.
    fn counted(generation: u32, counter: i64, collected: i64) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            0,
            *COUNTS_ONLY,
            &[("collections", counter), ("collected", collected)],
        )
    }

    /// Run one poll's Records through the seam against an inert exporter: the events an
    /// output format would be handed.
    fn poll_events(records: &[GcStat], observed_at_ns: i64) -> Vec<TraceEvent> {
        let mut exporter = ChromeTraceExporter::new();
        let mut context = MonitorContext::new(&mut exporter);
        context.events_for(1, records, observed_at_ns)
    }

    fn kinds(events: &[TraceEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                TraceEvent::ProcessMeta { .. } | TraceEvent::ThreadMeta { .. } => "M",
                TraceEvent::Begin { .. } => "B",
                TraceEvent::End { .. } => "E",
                TraceEvent::Instant { .. } => "I",
                TraceEvent::Counter { .. } => "C",
            })
            .collect()
    }

    /// A build with no timing monitors as counter tracks: one sample per generation per poll,
    /// no spans. Such a target used to yield an empty trace, then a zero-width pause at the
    /// epoch reporting a pause it never published.
    #[test]
    fn a_build_without_timing_polls_to_counter_samples_only() {
        let poll = [counted(0, 40, 900), counted(1, 7, 30), counted(2, 2, 4)];
        let events = poll_events(&poll, 5_000);

        assert_eq!(
            kinds(&events),
            ["M", "M", "C", "M", "M", "C", "M", "M", "C"]
        );
        let series: Vec<(&str, i64)> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Counter { name, ts_ns, .. } => Some((name.as_str(), *ts_ns)),
                _ => None,
            })
            .collect();
        // One track per generation, every sample of the poll on the Observer's clock.
        assert_eq!(series, [("G0", 5_000), ("G1", 5_000), ("G2", 5_000)]);
    }

    /// The other tier through the same seam, unchanged: a build that publishes timestamps
    /// still produces a span per Collection, on the target's own clock.
    #[test]
    fn a_build_with_timing_polls_to_spans() {
        let events = poll_events(&[timed(0, 40, 1_000, 2_000)], 5_000);
        assert_eq!(kinds(&events), ["M", "M", "B", "E", "C", "C"]);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TraceEvent::Begin { ts_ns, .. } if *ts_ns == 1_000)),
            "the pause is placed on the target's clock: {events:?}"
        );
    }

    /// Tier selection is per Record, from its own layout — a process tree can hold both, and
    /// nothing consults a version to tell them apart.
    #[test]
    fn each_record_takes_the_tier_its_own_layout_puts_it_in() {
        let events = poll_events(&[counted(0, 40, 900), timed(1, 7, 1_000, 2_000)], 5_000);
        assert_eq!(
            kinds(&events),
            ["M", "M", "C", "M", "M", "B", "E", "C", "C"]
        );
    }

    /// Dedup by cumulative counter applies to both tiers alike: a poll re-reading the same
    /// Entries emits nothing, so a counter track steps only where the count moved rather than
    /// once per tick.
    #[test]
    fn a_repeated_poll_emits_nothing_on_either_tier() {
        let mut exporter = ChromeTraceExporter::new();
        let mut context = MonitorContext::new(&mut exporter);
        let poll = [counted(0, 40, 900), timed(1, 7, 1_000, 2_000)];

        assert!(!context.events_for(1, &poll, 5_000).is_empty());
        assert!(
            context.events_for(1, &poll, 6_000).is_empty(),
            "the ring has not moved, so there is nothing new to report"
        );
        // A Collection past the cursor is reported, on the clock of the poll that read it.
        let advanced = [counted(0, 41, 950)];
        let events = context.events_for(1, &advanced, 7_000);
        assert_eq!(kinds(&events), ["M", "M", "C"]);
    }

    /// The summary is folded from the same polls that fed the exporter, so a run's figures
    /// reach the seam without a live interpreter or a written trace.
    #[test]
    fn the_summary_reports_what_the_polls_admitted() {
        let mut exporter = ChromeTraceExporter::new();
        let mut context = MonitorContext::new(&mut exporter);

        context.events_for(1, &[counted(0, 10, 100), timed(1, 4, 1_000, 1_400)], 5_000);
        context.events_for(1, &[counted(0, 14, 180), timed(1, 5, 2_000, 2_600)], 6_000);

        let summary = context.summary();
        assert_eq!(summary.len(), 1);
        let generations = &summary[0].generations;
        // The counter's rise alone: this ring publishes no timestamps, so its Entries are
        // snapshots and the one that opened the span is no Collection of its own.
        assert_eq!(generations[0].collections, 4);
        assert_eq!(generations[0].collected, 80);
        // Tier follows each ring's own layout, so one interpreter holds both here even though
        // no real build does.
        assert_eq!(generations[0].pause_total_ns, None);
        assert_eq!(generations[1].pause_total_ns, Some(1_000));
    }

    /// Loss reaches the summary through the seam the exporter is fed from: an interpreter that
    /// collected a hundred times between two polls reports a hundred, not the two Records those
    /// polls caught.
    #[test]
    fn the_summary_reconstructs_the_collections_the_polls_could_not_read() {
        let mut exporter = ChromeTraceExporter::new();
        let mut context = MonitorContext::new(&mut exporter);

        context.events_for(1, &[timed(0, 1, 1_000, 1_400)], 5_000);
        context.events_for(1, &[timed(0, 100, 900_000, 900_600)], 6_000);

        let gen0 = &context.summary()[0].generations[0];
        assert_eq!(gen0.collections, 100);
        assert_eq!(gen0.records, 2);
        assert_eq!(gen0.lost, 98);
        assert!((gen0.coverage - 0.02).abs() < 1e-12, "{}", gen0.coverage);
    }

    /// A run that read nothing summarizes to nothing, which is what lets the CLI say so
    /// instead of printing an empty table.
    #[test]
    fn a_run_that_admitted_nothing_has_an_empty_summary() {
        let mut exporter = ChromeTraceExporter::new();
        let context = MonitorContext::new(&mut exporter);
        assert!(context.summary().is_empty());
    }

    /// The single eviction point drops the dead PID's cursor, so a PID the OS recycles cannot
    /// read its predecessor's `collections` counters as already-seen and swallow the new
    /// process's first Collections.
    ///
    /// `poll` fills the cursor in a real run and needs a live target (`tests/monitor.rs`).
    /// The eviction rule does not, so it is pinned here, with an unopened
    /// `ChromeTraceExporter` as an inert sink rather than a double whose methods this test
    /// would never call.
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
