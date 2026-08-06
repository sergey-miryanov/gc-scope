//! Which Records a poll has already seen, keyed on CPython's cumulative `collections`.
//!
//! One accumulator per ring, where a ring is one `(pid, interpreter, generation)`. Each holds
//! what its ring did against what the Observer read of it, so the difference between the two
//! is recoverable later as Loss.

use std::collections::HashMap;

use crate::remote_debugging::gc_stats::GcStat;

/// One ring: the Entries one generation of one interpreter publishes its Records through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RingKey {
    pub pid: u32,
    pub interpreter: i64,
    pub generation: u32,
}

/// What one ring did, against what the Observer saw of it.
///
/// The `_counter` fields come from CPython and describe the ring; the rest describe the
/// reading. Every Loss figure is the difference between the two, so nothing derived is stored
/// here. Ticket 06 computes exact count, exact pause, Coverage and scale factor from these.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RingObservation {
    first_counter: i64,
    last_counter: i64,
    first_duration: f64,
    last_duration: f64,
    sampled: u64,
    measured_pause_ns: i64,
}

impl RingObservation {
    /// Whether any Record from this ring has been read. An empty one means nothing is known
    /// about the ring, rather than that the ring collected nothing.
    pub fn is_empty(&self) -> bool {
        self.sampled == 0
    }

    /// The cumulative `collections` CPython reported on the first Record read.
    pub fn first_counter(&self) -> i64 {
        self.first_counter
    }

    /// The cumulative `collections` on the most recent Record read.
    pub fn last_counter(&self) -> i64 {
        self.last_counter
    }

    /// The cumulative pause total, in seconds, on the first Record read.
    pub fn first_duration(&self) -> f64 {
        self.first_duration
    }

    /// The cumulative pause total, in seconds, on the most recent Record read.
    pub fn last_duration(&self) -> f64 {
        self.last_duration
    }

    /// How many Records were read. Against the counter span, this is Coverage.
    pub fn sampled(&self) -> u64 {
        self.sampled
    }

    /// Pause time summed over the Records read, in nanoseconds. Builds with no timestamps
    /// contribute nothing, so it stays zero there.
    pub fn measured_pause_ns(&self) -> i64 {
        self.measured_pause_ns
    }

    /// Whether `counter` is past this ring's cursor. The first Record of a ring is always
    /// past it; afterwards the counter must have advanced.
    fn admits(&self, counter: i64) -> bool {
        self.is_empty() || counter > self.last_counter
    }

    fn fold(&mut self, record: &GcStat) {
        if self.is_empty() {
            self.first_counter = record.collections();
            self.first_duration = record.duration();
        }
        self.last_counter = record.collections();
        self.last_duration = record.duration();
        self.sampled += 1;
        self.measured_pause_ns += pause_ns(record);
    }
}

/// One Record's own pause. Floored at zero because a build with no timestamps reports both
/// ends as zero, and because a torn read can invert them.
fn pause_ns(record: &GcStat) -> i64 {
    (record.ts_stop() - record.ts_start()).max(0)
}

/// Which Records of a process tree have already been reported, and what their rings did.
///
/// Replaces a per-`(generation, Entry)` high-water mark on `ts_start`, which could not work
/// below 3.15 (no timestamps are published there) and could not detect a gap: a mark on one
/// Entry says nothing about how many Collections passed through it between two polls.
#[derive(Debug, Default)]
pub struct Cursor {
    rings: HashMap<RingKey, RingObservation>,
    /// Per `(pid, interpreter)`, the latest `ts_start` on a Collection still running.
    /// Collections serialize within an interpreter, so one that had started is later evidence
    /// than the newest finished Record, and so the strongest bound on when the Observer last
    /// had certainty.
    last_certainty: HashMap<(u32, i64), i64>,
}

impl Cursor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one poll's Records in and return the ones not yet reported, in the order an
    /// exporter should receive them.
    ///
    /// A poll hands over whole rings, so most of what arrives has been seen. Records fold in
    /// `(interpreter, generation, counter)` order, the order their rings produced them, and
    /// return in `ts_start` order, the order a trace wants. Within one ring the two agree,
    /// since a later Collection carries both a higher counter and a later start.
    ///
    /// Three kinds of Entry are refused: one still running, held for a later poll without
    /// advancing anything; one whose counter is not past its ring's cursor, already reported,
    /// which also covers CPython's copy of a Record into the next Entry ahead of overwriting
    /// it (same counter, and no timestamp tells the two apart); and one whose counter is
    /// zero, which has never held a Collection.
    pub fn admit<'r>(&mut self, pid: u32, records: &'r [GcStat]) -> Vec<&'r GcStat> {
        let mut candidates: Vec<&GcStat> = records.iter().collect();
        candidates.sort_by_key(|r| (r.interpreter_id, r.generation, r.collections()));

        let mut fresh: Vec<&GcStat> = Vec::new();
        for record in candidates {
            if !record.is_complete() {
                // An Entry that never held a Collection reads zero at both ends and so fails
                // the completeness test too. A start it never published is no evidence, and a
                // fresh attach reads a ring that is mostly these.
                if record.ts_start() > 0 {
                    let bound = self
                        .last_certainty
                        .entry((pid, record.interpreter_id))
                        .or_insert(i64::MIN);
                    *bound = (*bound).max(record.ts_start());
                }
                continue;
            }
            let counter = record.collections();
            if counter <= 0 {
                continue;
            }
            let observation = self
                .rings
                .entry(RingKey {
                    pid,
                    interpreter: record.interpreter_id,
                    generation: record.generation,
                })
                .or_default();
            if !observation.admits(counter) {
                continue;
            }
            observation.fold(record);
            fresh.push(record);
        }

        // Stable, so Records sharing a timestamp (every Record on a build that publishes
        // none) keep the ring order they were folded in.
        fresh.sort_by_key(|r| r.ts_start());
        fresh
    }

    /// Drop everything known about `pid`. The single eviction point for a process that died,
    /// so a recycled PID cannot read its predecessor's counters as already-seen.
    pub fn forget(&mut self, pid: u32) {
        self.rings.retain(|key, _| key.pid != pid);
        self.last_certainty.retain(|&(p, _), _| p != pid);
    }

    /// What one ring did against what was read of it, or `None` if no Record of it has been
    /// read.
    pub fn observation(&self, key: RingKey) -> Option<&RingObservation> {
        self.rings.get(&key)
    }

    /// The latest moment the Observer had certainty about this interpreter: the `ts_start` of
    /// the newest Collection it caught still running. `None` until it catches one.
    pub fn last_certainty(&self, pid: u32, interpreter: i64) -> Option<i64> {
        self.last_certainty.get(&(pid, interpreter)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A ring build's Entry layout: both timestamps, the cumulative collection counter, and
    /// the cumulative pause total.
    static RING: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start", "ts_stop", "collections", "duration"]));

    /// A pre-ring build's Entry layout: cumulative counters and nothing else. No timestamp
    /// exists to key on here, which is half of why the key had to move.
    static COUNTERS_ONLY: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["collections", "collected", "uncollectable"]));

    /// A Record on a ring build, at Entry `index` of `interpreter`'s generation `generation`.
    fn at(
        generation: u32,
        index: usize,
        interpreter: i64,
        counter: i64,
        ts_start: i64,
        ts_stop: i64,
    ) -> GcStat {
        GcStat::from_fields(
            generation,
            index,
            interpreter,
            *RING,
            &[
                ("collections", counter),
                ("ts_start", ts_start),
                ("ts_stop", ts_stop),
            ],
        )
    }

    /// A finished Record in generation 0 of interpreter 0, at Entry 0.
    fn done(counter: i64, ts_start: i64, ts_stop: i64) -> GcStat {
        at(0, 0, 0, counter, ts_start, ts_stop)
    }

    /// A Record whose Collection is still running: `ts_start` published, `ts_stop` not yet.
    fn running(counter: i64, ts_start: i64) -> GcStat {
        at(0, 0, 0, counter, ts_start, 0)
    }

    /// An Entry on a build with no timestamps at all.
    fn untimed(generation: u32, interpreter: i64, counter: i64) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            interpreter,
            *COUNTERS_ONLY,
            &[("collections", counter)],
        )
    }

    fn key(pid: u32, interpreter: i64, generation: u32) -> RingKey {
        RingKey {
            pid,
            interpreter,
            generation,
        }
    }

    /// The counters a poll admitted, in the order they were handed to the exporter.
    fn admitted(cursor: &mut Cursor, pid: u32, records: &[GcStat]) -> Vec<i64> {
        cursor
            .admit(pid, records)
            .into_iter()
            .map(|r| r.collections())
            .collect()
    }

    /// A Record is admitted once. Re-reading the same Entry on the next poll admits nothing,
    /// since the ring has not changed.
    #[test]
    fn a_record_is_admitted_once() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[done(1, 100, 150)]), [1]);
        assert_eq!(admitted(&mut c, 1, &[done(1, 100, 150)]), []);
    }

    /// The key is the counter. A Record that ran later but reports an earlier `ts_start` than
    /// one already admitted is still new, and a timestamp high-water mark drops it.
    #[test]
    fn selection_follows_the_counter_not_the_timestamp() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[done(7, 9_000, 9_500)]), [7]);
        // A newer Collection whose clock reading is lower: a monotonic clock read across a
        // suspend, or just a test pinning that the timestamp plays no part.
        assert_eq!(admitted(&mut c, 1, &[done(8, 100, 150)]), [8]);
        // And an older counter is refused however high its timestamp.
        assert_eq!(admitted(&mut c, 1, &[done(6, 99_000, 99_500)]), []);
    }

    /// CPython copies a Record into the next Entry before overwriting the old one, so one
    /// Collection can appear at two positions in a single poll. The counter is the only thing
    /// identifying them as one Collection; a per-Entry mark admits both and doubles it.
    #[test]
    fn the_same_collection_at_two_entries_is_admitted_once() {
        let mut c = Cursor::new();
        let poll = [at(0, 3, 0, 5, 100, 150), at(0, 4, 0, 5, 100, 150)];
        assert_eq!(admitted(&mut c, 1, &poll), [5]);
    }

    /// A poll hands over the whole ring every time, most of which was already reported. Only
    /// the Records past the cursor come out, and they come out in counter order.
    #[test]
    fn a_whole_ring_poll_yields_only_what_is_past_the_cursor() {
        let mut c = Cursor::new();
        let first = [done(1, 100, 110), done(2, 200, 210)];
        assert_eq!(admitted(&mut c, 1, &first), [1, 2]);

        // The next poll returns the same two Entries plus two newer ones, in ring order
        // rather than counter order.
        let second = [
            done(3, 300, 310),
            done(1, 100, 110),
            done(4, 400, 410),
            done(2, 200, 210),
        ];
        assert_eq!(admitted(&mut c, 1, &second), [3, 4]);
    }

    /// An Entry that has never held a Collection reads a counter of zero, and a fresh attach
    /// reads a mostly-empty ring. Only an explicit floor keeps those out of the trace.
    #[test]
    fn untouched_entries_are_never_admitted() {
        let mut c = Cursor::new();
        let poll = [done(0, 0, 0), done(0, 0, 0), done(1, 100, 150)];
        assert_eq!(admitted(&mut c, 1, &poll), [1]);
    }

    /// An Entry that never held a Collection reads zero at both ends, the same as one caught
    /// mid-Collection. A fresh attach reads a ring that is mostly these, so mistaking them
    /// fabricates a certainty bound at the epoch for every interpreter, which is the value
    /// ticket 06 would bound a Loss window with.
    #[test]
    fn untouched_entries_are_not_mistaken_for_a_running_collection() {
        let mut c = Cursor::new();
        c.admit(1, &[done(0, 0, 0), done(0, 0, 0), done(3, 100, 150)]);
        assert_eq!(c.last_certainty(1, 0), None);
    }

    /// An Entry whose timestamps landed but whose counter did not (a torn read of a ring
    /// being overwritten) has nothing to key on, so it is refused rather than admitted as
    /// collection zero.
    #[test]
    fn a_complete_entry_carrying_no_counter_is_refused() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[done(0, 100, 150)]), []);
        assert_eq!(c.observation(key(1, 0, 0)), None);
    }

    /// A Collection still running is not a Record. It must not reach an exporter, and it must
    /// not advance the cursor: advancing past it rejects the finished Record, which
    /// republishes the same counter, and loses it for good.
    #[test]
    fn a_running_collection_is_held_back_until_it_finishes() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[running(4, 100)]), []);
        assert_eq!(admitted(&mut c, 1, &[done(4, 100, 150)]), [4]);
        assert_eq!(admitted(&mut c, 1, &[done(4, 100, 150)]), []);
    }

    /// A running Collection is evidence. Its start is the latest moment the Observer had
    /// certainty about that interpreter, stronger than the newest finished Record, since a
    /// Collection that had started is later news than one that had ended. Ticket 06 bounds
    /// Loss windows with it.
    #[test]
    fn a_running_collection_records_when_certainty_was_last_held() {
        let mut c = Cursor::new();
        assert_eq!(c.last_certainty(1, 0), None);

        c.admit(1, &[running(4, 100)]);
        assert_eq!(c.last_certainty(1, 0), Some(100));

        // Only ever forward: an older in-flight reading does not walk the bound back.
        c.admit(1, &[running(9, 900)]);
        c.admit(1, &[running(5, 500)]);
        assert_eq!(c.last_certainty(1, 0), Some(900));

        // And it survives the Record never coming back.
        c.admit(1, &[done(20, 2_000, 2_100)]);
        assert_eq!(c.last_certainty(1, 0), Some(900));
    }

    /// Collections serialize within an interpreter and not across them, so one interpreter's
    /// in-flight Entry says nothing about another's.
    #[test]
    fn certainty_is_tracked_per_interpreter() {
        let mut c = Cursor::new();
        c.admit(1, &[at(0, 0, 0, 4, 100, 0), at(0, 0, 7, 4, 800, 0)]);
        assert_eq!(c.last_certainty(1, 0), Some(100));
        assert_eq!(c.last_certainty(1, 7), Some(800));
    }

    /// Generations collect at wildly different rates, so a generation-2 counter far ahead of
    /// generation 0 must not mask it. Each generation is its own ring with its own cursor.
    #[test]
    fn generations_keep_independent_cursors() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[at(2, 0, 0, 900, 100, 110)]), [900]);
        // Generation 0's counter is far lower and still new for its own ring.
        assert_eq!(admitted(&mut c, 1, &[at(0, 0, 0, 3, 200, 210)]), [3]);
    }

    /// Two interpreters in one process advance their counters independently, so neither may
    /// consume the other's cursor.
    #[test]
    fn interpreters_keep_independent_cursors() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[at(0, 0, 0, 50, 100, 110)]), [50]);
        assert_eq!(admitted(&mut c, 1, &[at(0, 0, 1, 4, 200, 210)]), [4]);
        // Each still refuses its own already-seen counter.
        assert_eq!(admitted(&mut c, 1, &[at(0, 0, 0, 50, 100, 110)]), []);
        assert_eq!(admitted(&mut c, 1, &[at(0, 0, 1, 4, 200, 210)]), []);
    }

    /// One `Cursor` serves a whole process tree, so a worker's counters must not be read
    /// against its sibling's.
    #[test]
    fn processes_keep_independent_cursors() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 100, &[done(50, 100, 110)]), [50]);
        assert_eq!(admitted(&mut c, 200, &[done(4, 200, 210)]), [4]);
    }

    /// A PID is reused the moment the OS wants it back. State kept past a process's death
    /// makes the new occupant's first Collections read as already-seen, so a dead PID's state
    /// goes when it does. Its siblings' stays.
    #[test]
    fn forgetting_a_pid_drops_its_state_and_leaves_its_siblings_alone() {
        let mut c = Cursor::new();
        c.admit(100, &[done(50, 100, 110)]);
        c.admit(100, &[running(51, 120)]);
        c.admit(200, &[done(50, 100, 110)]);

        c.forget(100);

        assert_eq!(c.observation(key(100, 0, 0)), None);
        assert_eq!(c.last_certainty(100, 0), None);
        // A new process on the recycled PID starts from nothing.
        assert_eq!(admitted(&mut c, 100, &[done(1, 10, 20)]), [1]);
        // The sibling is untouched.
        assert!(c.observation(key(200, 0, 0)).is_some());
        assert_eq!(admitted(&mut c, 200, &[done(50, 100, 110)]), []);
    }

    /// The accumulator holds what its ring did against what was read of it: cumulative counter
    /// and cumulative pause at the first and last Record seen, how many were sampled, and the
    /// pause measured across them. Every Loss figure comes from these six numbers.
    #[test]
    fn an_accumulator_records_the_span_it_observed() {
        let mut c = Cursor::new();
        let with_duration = |counter: i64, ts_start: i64, ts_stop: i64, cumulative: f64| {
            GcStat::from_fields(
                0,
                0,
                0,
                *RING,
                &[
                    ("collections", counter),
                    ("ts_start", ts_start),
                    ("ts_stop", ts_stop),
                    ("duration", f64::to_bits(cumulative) as i64),
                ],
            )
        };

        c.admit(1, &[with_duration(10, 1_000, 1_500, 0.25)]);
        c.admit(1, &[with_duration(14, 4_000, 4_300, 0.90)]);

        let obs = c.observation(key(1, 0, 0)).expect("an observed ring");
        assert!(!obs.is_empty());
        assert_eq!(obs.first_counter(), 10);
        assert_eq!(obs.last_counter(), 14);
        assert_eq!(obs.first_duration(), 0.25);
        assert_eq!(obs.last_duration(), 0.90);
        assert_eq!(obs.sampled(), 2);
        // 500 ns + 300 ns actually measured, against 4 collections the counter says ran.
        assert_eq!(obs.measured_pause_ns(), 800);
    }

    /// A ring nobody has read has no accumulator, which is what lets ticket 06 tell "covered
    /// nothing" from "covered everything".
    #[test]
    fn an_unobserved_ring_has_no_accumulator() {
        let c = Cursor::new();
        assert_eq!(c.observation(key(1, 0, 0)), None);
    }

    /// A build with no timestamp fields publishes nothing a timestamp key could order, so the
    /// old cursor admitted nothing and the trace came out empty against a process collecting
    /// constantly. The counter is all these builds have.
    #[test]
    fn a_build_without_timestamps_still_advances() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[untimed(0, 0, 12)]), [12]);
        assert_eq!(admitted(&mut c, 1, &[untimed(0, 0, 12)]), []);
        assert_eq!(admitted(&mut c, 1, &[untimed(0, 0, 30)]), [30]);
        // Its generations stay independent, same as a ring build's.
        assert_eq!(admitted(&mut c, 1, &[untimed(1, 0, 2)]), [2]);
    }

    /// Such a build has no `ts_stop` to fail a completeness check against, so its Entries must
    /// not all read as permanently in-flight. The gate is on the layout, not the values.
    #[test]
    fn a_build_without_timestamps_holds_nothing_back() {
        let mut c = Cursor::new();
        assert_eq!(admitted(&mut c, 1, &[untimed(0, 0, 5)]), [5]);
        assert_eq!(c.last_certainty(1, 0), None, "nothing was ever in flight");
    }

    /// The cursor this replaced, frozen as a test oracle: a `ts_start` high-water mark per
    /// `(generation, Entry)`, skipping an incomplete Entry without advancing its mark. The
    /// equivalence claim below cannot be checked any other way.
    fn select_fresh_by_timestamp<'s>(
        stats: &'s [GcStat],
        seen: &mut HashMap<(u32, usize), i64>,
    ) -> Vec<&'s GcStat> {
        let mut fresh: Vec<&GcStat> = Vec::new();
        for stat in stats {
            if !stat.is_complete() {
                continue;
            }
            let mark = seen.entry((stat.generation, stat.index)).or_insert(0);
            if stat.ts_start() > *mark {
                *mark = stat.ts_start();
                fresh.push(stat);
            }
        }
        fresh.sort_by_key(|s| s.ts_start());
        fresh
    }

    fn xorshift64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// One Entry's contents, so a simulated ring can be rebuilt into `GcStat`s each tick.
    #[derive(Clone, Copy, Default)]
    struct Entry {
        counter: i64,
        ts_start: i64,
        ts_stop: i64,
    }

    /// A ring build polled fast enough that nothing is overwritten unread: one Collection
    /// runs per tick, in a generation the seed picks, and the poll returns every Entry of
    /// every generation. Entry counts and generation-major ordering match a real read, and
    /// the ring wraps, so the equivalence covers Entry reuse.
    fn no_loss_polls(seed: u64, ticks: usize) -> Vec<Vec<GcStat>> {
        const ENTRIES: [usize; 3] = [11, 3, 3];
        let mut rng = seed | 1;
        let mut counters = [0i64; 3];
        let mut clock = 1_000i64;
        let mut ring: Vec<Vec<Entry>> =
            ENTRIES.iter().map(|&n| vec![Entry::default(); n]).collect();

        let mut polls = Vec::new();
        for _ in 0..ticks {
            let generation = (xorshift64(&mut rng) % 3) as usize;
            counters[generation] += 1;
            let index = (counters[generation] as usize - 1) % ENTRIES[generation];
            clock += 1 + (xorshift64(&mut rng) % 50) as i64;
            let ts_start = clock;
            clock += 1 + (xorshift64(&mut rng) % 20) as i64;
            ring[generation][index] = Entry {
                counter: counters[generation],
                ts_start,
                ts_stop: clock,
            };

            polls.push(
                ring.iter()
                    .enumerate()
                    .flat_map(|(g, entries)| {
                        entries
                            .iter()
                            .enumerate()
                            .map(move |(i, e)| at(g as u32, i, 0, e.counter, e.ts_start, e.ts_stop))
                    })
                    .collect(),
            );
        }
        polls
    }

    /// The equivalence this change has to preserve: against a ring polled fast enough that
    /// nothing was overwritten unread, the counter cursor admits what the timestamp cursor
    /// admitted, tick for tick. Past that the two are meant to disagree, since the timestamp
    /// cursor cannot see a gap, so the simulation stays inside the case where agreement is
    /// the requirement.
    #[test]
    fn a_no_loss_ring_admits_exactly_what_the_timestamp_cursor_did() {
        for seed in [0x9e37_79b9_7f4a_7c15u64, 0x0123_4567_89ab_cdef, 7] {
            let mut cursor = Cursor::new();
            let mut seen: HashMap<(u32, usize), i64> = HashMap::new();
            let mut admitted_any = false;

            for (tick, poll) in no_loss_polls(seed, 200).iter().enumerate() {
                let counter_keyed: Vec<(u32, i64)> = cursor
                    .admit(1, poll)
                    .into_iter()
                    .map(|r| (r.generation, r.ts_start()))
                    .collect();
                let timestamp_keyed: Vec<(u32, i64)> = select_fresh_by_timestamp(poll, &mut seen)
                    .into_iter()
                    .map(|r| (r.generation, r.ts_start()))
                    .collect();
                assert_eq!(
                    counter_keyed, timestamp_keyed,
                    "seed {seed:#x}, tick {tick}"
                );
                admitted_any |= !counter_keyed.is_empty();
            }
            assert!(
                admitted_any,
                "the simulation admitted nothing: seed {seed:#x}"
            );
        }
    }

    /// Records come out in `ts_start` order across every generation, not generation-major,
    /// because that is the order a trace wants and the order the timestamp cursor produced.
    #[test]
    fn records_reach_the_exporter_in_timestamp_order_across_generations() {
        let mut c = Cursor::new();
        let poll = [
            at(0, 0, 0, 10, 300, 310),
            at(0, 1, 0, 11, 100, 110),
            at(1, 0, 0, 4, 400, 410),
            at(2, 0, 0, 2, 200, 210),
        ];
        let order: Vec<i64> = c
            .admit(1, &poll)
            .into_iter()
            .map(|r| r.ts_start())
            .collect();
        assert_eq!(order, [100, 200, 300, 400]);
    }
}
