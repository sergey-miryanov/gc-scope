//! Reconstructing the Collections a poll never got to read.
//!
//! An interpreter collecting faster than gcscope polls overwrites Records unread, and what
//! survives is a biased sample: a long Collection holds its Entry longer and is likelier to be
//! caught. `collections` counts what was missed and `duration` prices it, both cumulative, so
//! a total stays exact under any amount of Loss and only the distribution stays sampled.
//!
//! The accounted span runs from a ring's first Record read to its last. What ran before it is
//! outside the account and is not Loss: nothing in an Entry tells "ran before gcscope arrived"
//! from "overwritten unread". ADR 0019 holds the rest of the reasoning.

use crate::monitor::cursor::RingObservation;

/// What one ring did over its accounted span, against what the Observer read of it.
///
/// Derived from a [`RingObservation`] rather than stored, so no figure can drift from the
/// accumulator it came out of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossAccount {
    /// Collections that ran over the span, from CPython's own counter.
    pub exact_collections: i64,
    /// Collections the Observer holds a Record for. Zero on the counter-only tier, where an
    /// Entry is a snapshot of running totals and describes no Collection of its own.
    pub observed_collections: i64,
    /// Collections the Observer holds no Record for. Overwritten unread on the ring tier, and
    /// every Collection in the span on the counter-only tier.
    pub lost_collections: i64,
    /// The observed share of the span, in `[0, 1]`. It says whether a sampled figure beside it
    /// is the distribution or the tail of a biased one.
    pub coverage: f64,
    /// Pause over the span, from the target's cumulative accumulator. `None` without a
    /// `duration` to difference: absent, never zero (ADR 0017).
    pub exact_pause_ns: Option<i64>,
    /// Pause summed over the Records read. `None` where no Entry bounds a Collection. Under
    /// Loss it sits below `exact_pause_ns`.
    pub measured_pause_ns: Option<i64>,
    /// Pause belonging to Collections nobody read. Never negative.
    pub lost_pause_ns: Option<i64>,
    /// The multiplier taking a measured pause sum to its exact counterpart, at least `1.0`.
    ///
    /// It corrects figures that partition the pause. Sub-phase totals have no cumulative
    /// counterpart in CPython but add up to the pause, so scaling their measured sum estimates
    /// the whole. A percentile describes a distribution's shape rather than its total, and
    /// stays sampled with `coverage` beside it.
    pub scale_factor: Option<f64>,
}

/// The account for a ring nothing has been read from. Coverage is `1.0`: it lost none of the
/// nothing it covers, and every call site would otherwise guard a division.
const UNOBSERVED: LossAccount = LossAccount {
    exact_collections: 0,
    observed_collections: 0,
    lost_collections: 0,
    coverage: 1.0,
    exact_pause_ns: None,
    measured_pause_ns: None,
    lost_pause_ns: None,
    scale_factor: None,
};

/// Account for what one ring did against what was read of it.
pub fn account(observation: &RingObservation) -> LossAccount {
    if observation.is_empty() {
        return UNOBSERVED;
    }

    let (first, last) = (observation.first(), observation.last());
    let timed = observation.has_timing();
    // A ring Entry *is* a Collection, so the Record that opened the span is one of them and
    // both ends of the counter count. An inline Entry is a snapshot, so only the rise does.
    let exact_collections = last.collections - first.collections + i64::from(timed);
    let observed_collections = if timed {
        observation.sampled() as i64
    } else {
        0
    };

    let coverage = if !timed {
        // The tier's constant, idle generations included. One whose counter never moved would
        // take `1.0` from the empty-accumulator rule, and a `1.000` under a column of `0.000`
        // siblings reads as the generation gcscope watched properly.
        0.0
    } else if exact_collections == 0 {
        // Guarding the division, not describing a case: `admits` keeps counters increasing, so
        // a timed span holds at least the Record that opened it.
        1.0
    } else {
        observed_collections as f64 / exact_collections as f64
    };

    let mut account = LossAccount {
        exact_collections,
        observed_collections,
        lost_collections: exact_collections - observed_collections,
        coverage,
        ..UNOBSERVED
    };

    if !timed {
        return account;
    }

    // Timestamps price the Collections that were read, whatever else the ring carries.
    let measured = observation.measured_pause_ns();
    account.measured_pause_ns = Some(measured);

    // Only a ring publishing `duration` can be differenced for the pause nobody watched.
    // Without it the reconstruction resolves to the measured sum and publishes it under a
    // figure meaning "what ran", the fail-open every layout question here avoids (ADR 0007).
    if observation.has_pause_total() {
        // The cumulative total at the first Record already covers that Record's own
        // Collection, so the delta starts after it and its pause goes back on.
        //
        // A `duration` resolved through a wrong offset arrives as arbitrary bits, which the
        // same-minor `LAYOUTS` fallback and a torn read both reach. `f64 as i64` saturates, so
        // it is the addition that overflows, and a delta that is not a number prices nothing.
        let delta = last.duration - first.duration;
        let spanned = if delta.is_finite() {
            secs_to_ns(delta).saturating_add(observation.first_pause_ns())
        } else {
            measured
        };
        // That total is a float of seconds against an integer-nanosecond sum, so a span
        // holding almost no pause subtracts to a hair under what gcscope watched directly.
        // Flooring at the measured figure is what keeps the lost pause off negative.
        let exact = spanned.max(measured);
        account.exact_pause_ns = Some(exact);
        account.lost_pause_ns = Some(exact - measured);
        account.scale_factor = Some(if measured == 0 {
            1.0
        } else {
            exact as f64 / measured as f64
        });
    }

    account
}

/// Seconds, as CPython publishes a cumulative pause total, in the nanoseconds everything
/// downstream counts in.
fn secs_to_ns(secs: f64) -> i64 {
    (secs * 1e9).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::cursor::{Cursor, RingKey};
    use crate::remote_debugging::gc_stats::GcStat;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A ring build: each Entry bounds one Collection and carries the cumulative pause total.
    static RING: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["ts_start", "ts_stop", "collections", "duration"]));

    /// An inline build: the Lifetime counters and nothing else. The two layouts express the
    /// tiers, so no test below names a Python version.
    static COUNTS_ONLY: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["collections", "collected", "uncollectable"]));

    /// A Record from a ring build: its own bounds, and the generation's running total.
    fn ring(counter: i64, ts_start: i64, ts_stop: i64, cumulative_secs: f64) -> GcStat {
        GcStat::from_fields(
            0,
            0,
            0,
            *RING,
            &[
                ("collections", counter),
                ("ts_start", ts_start),
                ("ts_stop", ts_stop),
                ("duration", f64::to_bits(cumulative_secs) as i64),
            ],
        )
    }

    /// A Record from a build with no timing.
    fn counted(counter: i64) -> GcStat {
        GcStat::from_fields(0, 0, 0, *COUNTS_ONLY, &[("collections", counter)])
    }

    const GEN0: RingKey = RingKey {
        pid: 1,
        interpreter: 0,
        generation: 0,
    };

    /// Account for what a ring did after scripted polls. Every case below goes through the
    /// cursor rather than building an accumulator by hand.
    fn observe(polls: &[&[GcStat]]) -> LossAccount {
        let mut cursor = Cursor::new();
        for poll in polls {
            cursor.admit(1, poll);
        }
        cursor.observation(GEN0).map_or(UNOBSERVED, account)
    }

    /// Polled fast enough to catch every Collection, the counter agrees with the Records read
    /// and the exact pause is the measured one.
    #[test]
    fn a_ring_read_end_to_end_reports_full_coverage_and_no_loss() {
        let a = observe(&[
            &[ring(10, 1_000, 1_400, 0.000_000_400)],
            &[ring(11, 2_000, 2_600, 0.000_001_000)],
            &[ring(12, 3_000, 3_100, 0.000_001_100)],
        ]);

        assert_eq!(a.exact_collections, 3);
        assert_eq!(a.observed_collections, 3);
        assert_eq!(a.lost_collections, 0);
        assert_eq!(a.coverage, 1.0);
        assert_eq!(a.exact_pause_ns, Some(1_100));
        assert_eq!(a.measured_pause_ns, Some(1_100));
        assert_eq!(a.lost_pause_ns, Some(0));
        assert_eq!(a.scale_factor, Some(1.0));
    }

    /// The point of the module. A ring that ran 100 Collections while gcscope read 2 reports
    /// 100, and prices the 98 nobody saw from the target's own accumulator.
    #[test]
    fn a_ring_that_outran_the_poll_reports_what_ran_not_what_was_read() {
        let a = observe(&[
            &[ring(1, 1_000, 1_400, 0.000_000_400)],
            &[ring(100, 900_000, 900_600, 0.000_500_000)],
        ]);

        assert_eq!(a.exact_collections, 100);
        assert_eq!(a.observed_collections, 2);
        assert_eq!(a.lost_collections, 98);
        assert!((a.coverage - 0.02).abs() < 1e-12, "{}", a.coverage);
        // 500 us over the span, of which 1 us was watched directly.
        assert_eq!(a.exact_pause_ns, Some(500_000));
        assert_eq!(a.measured_pause_ns, Some(1_000));
        assert_eq!(a.lost_pause_ns, Some(499_000));
        assert_eq!(a.scale_factor, Some(500.0));
    }

    /// Timestamps price the Collections that were read; only `duration` prices the ones that
    /// were not. A ring carrying the first and not the second reports what it measured and
    /// leaves the rest absent, rather than publishing the measured sum as what ran.
    #[test]
    fn a_ring_without_a_cumulative_total_reports_no_exact_pause() {
        static UNPRICED: LazyLock<&'static GcItemLayout> =
            LazyLock::new(|| seq_layout(&["ts_start", "ts_stop", "collections"]));
        let unpriced = |counter: i64, ts_start: i64, ts_stop: i64| {
            GcStat::from_fields(
                0,
                0,
                0,
                *UNPRICED,
                &[
                    ("collections", counter),
                    ("ts_start", ts_start),
                    ("ts_stop", ts_stop),
                ],
            )
        };

        let a = observe(&[
            &[unpriced(1, 1_000, 1_400)],
            &[unpriced(100, 900_000, 900_600)],
        ]);

        // The counts still reconstruct: they come from a counter this ring does publish.
        assert_eq!(a.exact_collections, 100);
        assert_eq!(a.lost_collections, 98);
        assert_eq!(a.measured_pause_ns, Some(1_000));
        assert_eq!(a.exact_pause_ns, None);
        assert_eq!(a.lost_pause_ns, None);
        assert_eq!(a.scale_factor, None);
    }

    /// Nothing an inline build publishes is per-Collection, so its counts stand alone with no
    /// distribution behind them and Coverage is `0` (spec 0011 §2, ADR 0017).
    #[test]
    fn the_counter_only_tier_covers_nothing_it_counts() {
        let a = observe(&[&[counted(10)], &[counted(30)]]);

        assert_eq!(a.exact_collections, 20);
        assert_eq!(a.observed_collections, 0);
        assert_eq!(a.lost_collections, 20);
        assert_eq!(a.coverage, 0.0);
        assert_eq!(a.exact_pause_ns, None);
        assert_eq!(a.lost_pause_ns, None);
        assert_eq!(a.scale_factor, None);
    }

    /// A ring nobody read has lost none of the nothing it covers: Coverage `1.0`, not `0/0`.
    #[test]
    fn an_unobserved_ring_reports_full_coverage_and_no_loss() {
        let a = observe(&[]);

        assert_eq!(a.exact_collections, 0);
        assert_eq!(a.lost_collections, 0);
        assert_eq!(a.coverage, 1.0);
        assert_eq!(a.exact_pause_ns, None);
    }

    /// An idle generation follows the tier, not the empty-accumulator rule. Attach to a
    /// process whose gen 2 collected once long ago and `1.0` puts a fully-covered row directly
    /// under its siblings' `0.000`.
    #[test]
    fn an_idle_generation_on_the_counter_only_tier_still_covers_nothing() {
        let a = observe(&[&[counted(10)], &[counted(10)]]);
        assert_eq!(a.exact_collections, 0);
        assert_eq!(a.lost_collections, 0);
        assert_eq!(a.coverage, 0.0);
    }

    /// A `duration` read through a wrong offset arrives as arbitrary bits, which the fallback
    /// paths in layout resolution reach. `f64 as i64` saturates, so the add-back is what
    /// overflows: a debug build panics, a release build wraps.
    #[test]
    fn a_nonsense_cumulative_total_prices_the_span_at_what_was_measured() {
        for bogus in [f64::INFINITY, f64::NAN, -f64::INFINITY, 1e300] {
            let a = observe(&[
                &[ring(10, 1_000, 1_400, 0.0)],
                &[ring(11, 2_000, 2_600, bogus)],
            ]);
            assert_eq!(a.measured_pause_ns, Some(1_000), "{bogus}");
            assert!(a.lost_pause_ns.unwrap() >= 0, "{bogus}");
            assert!(a.exact_pause_ns.unwrap() >= 1_000, "{bogus}");
        }
    }

    /// What ran before the first Record read is outside the span. A ring whose counter already
    /// stood at 900 has not lost 899 Collections; gcscope was not there.
    #[test]
    fn collections_from_before_the_first_record_are_outside_the_span() {
        let a = observe(&[
            &[ring(900, 1_000, 1_400, 90.0)],
            &[ring(901, 2_000, 2_600, 90.000_000_600)],
        ]);

        assert_eq!(a.exact_collections, 2);
        assert_eq!(a.lost_collections, 0);
        assert_eq!(a.coverage, 1.0);
        // Nor does the 90 seconds of pause behind the first Record land in the span.
        assert_eq!(a.exact_pause_ns, Some(1_000));
    }

    /// A float of seconds against an integer-nanosecond sum: a span holding almost no pause
    /// subtracts to a hair under what was measured, dragging the exact total below a figure
    /// gcscope watched directly.
    #[test]
    fn a_span_can_never_price_below_the_pause_actually_measured() {
        // A generation whose running total has grown until a Collection's own pause falls
        // below half an ulp of it, so CPython's own accumulator cannot record the rise.
        let history = 1e10f64;
        assert_eq!(
            history + 600e-9,
            history,
            "the hazard floored below is real"
        );

        let a = observe(&[
            &[ring(10, 1_000, 1_400, history)],
            &[ring(11, 2_000, 2_600, history + 600e-9)],
        ]);

        assert_eq!(a.measured_pause_ns, Some(1_000));
        assert_eq!(a.exact_pause_ns, Some(1_000));
        assert_eq!(a.lost_pause_ns, Some(0), "a lost pause is never negative");
        assert_eq!(a.scale_factor, Some(1.0));
    }

    // ── properties ───────────────────────────────────────────────────────────────────

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
        cumulative: f64,
    }

    /// A target collecting in bursts of up to `burst` between polls through a ring of
    /// `entries`, so a burst outgrowing the ring overwrites Records unread.
    ///
    /// Returns the polls and each Collection's own pause indexed by its counter, the ground
    /// truth the exact pause has to land on.
    fn drive(seed: u64, entries: usize, ticks: usize, burst: u64) -> (Vec<Vec<GcStat>>, Vec<i64>) {
        let mut rng = seed | 1;
        let mut buffer = vec![Entry::default(); entries];
        let mut counter = 0i64;
        let mut clock = 1_000i64;
        let mut cumulative = 0.0f64;
        let mut pauses = vec![0i64];
        let mut polls = Vec::new();

        for _ in 0..ticks {
            for _ in 0..(xorshift64(&mut rng) % (burst + 1)) {
                counter += 1;
                clock += 1 + (xorshift64(&mut rng) % 5_000) as i64;
                let ts_start = clock;
                let pause = 1 + (xorshift64(&mut rng) % 900_000) as i64;
                clock += pause;
                cumulative += pause as f64 / 1e9;
                pauses.push(pause);
                buffer[(counter as usize - 1) % entries] = Entry {
                    counter,
                    ts_start,
                    ts_stop: clock,
                    cumulative,
                };
            }
            polls.push(
                buffer
                    .iter()
                    .map(|e| ring(e.counter, e.ts_start, e.ts_stop, e.cumulative))
                    .collect(),
            );
        }
        (polls, pauses)
    }

    /// Run a simulated target past the cursor and return its account beside the truth.
    fn simulate(seed: u64, entries: usize, burst: u64) -> (LossAccount, Vec<i64>, (i64, i64)) {
        let (polls, pauses) = drive(seed, entries, 120, burst);
        let mut cursor = Cursor::new();
        for poll in &polls {
            cursor.admit(1, poll);
        }
        let observation = cursor.observation(GEN0).expect("the ring was read");
        let span = (
            observation.first().collections,
            observation.last().collections,
        );
        (account(observation), pauses, span)
    }

    /// The invariants, whatever the target did: the parts sum to the whole, Coverage is a
    /// share, and no pause goes missing in the negative direction.
    #[test]
    fn the_account_holds_together_under_any_amount_of_loss() {
        for seed in [0x9e37_79b9_7f4a_7c15u64, 0x0123_4567_89ab_cdef, 7, 42] {
            for burst in [1, 3, 40] {
                let (a, _, _) = simulate(seed, 3, burst);
                let case = format!("seed {seed:#x}, burst {burst}");

                assert_eq!(
                    a.exact_collections,
                    a.observed_collections + a.lost_collections,
                    "{case}"
                );
                assert!((0.0..=1.0).contains(&a.coverage), "{case}: {}", a.coverage);
                assert!(a.lost_pause_ns.unwrap() >= 0, "{case}");
                assert!(a.scale_factor.unwrap() >= 1.0, "{case}");
                assert_eq!(
                    a.exact_pause_ns.unwrap(),
                    a.measured_pause_ns.unwrap() + a.lost_pause_ns.unwrap(),
                    "{case}"
                );
            }
        }
    }

    /// The exact pause is what really ran over the span, not the share a poll caught. Checked
    /// against the simulator's own record of every Collection.
    #[test]
    fn the_exact_pause_matches_what_the_target_really_spent() {
        for seed in [0x9e37_79b9_7f4a_7c15u64, 0x0123_4567_89ab_cdef, 7, 42] {
            for burst in [1, 3, 40] {
                let (a, pauses, (first, last)) = simulate(seed, 3, burst);
                let case = format!("seed {seed:#x}, burst {burst}");

                let truth: i64 = pauses[first as usize..=last as usize].iter().sum();
                // Two conversions through a cumulative float of seconds, so a nanosecond or
                // two of rounding is expected and anything more is arithmetic.
                assert!(
                    (a.exact_pause_ns.unwrap() - truth).abs() <= 16,
                    "{case}: {} against {truth}",
                    a.exact_pause_ns.unwrap()
                );
                assert!(
                    a.measured_pause_ns.unwrap() <= truth,
                    "{case}: a poll cannot measure more than ran"
                );
            }
        }
    }

    /// A burst wider than the ring must actually lose Records, or the properties above hold
    /// over a no-Loss case and prove nothing.
    #[test]
    fn the_simulated_target_outruns_its_ring() {
        let (lossy, _, _) = simulate(7, 3, 40);
        assert!(lossy.lost_collections > 0, "{lossy:?}");
        assert!(lossy.coverage < 1.0, "{lossy:?}");

        // And a ring wide enough for the burst loses nothing, so Loss tracks the pressure
        // rather than appearing everywhere.
        let (clean, _, _) = simulate(7, 64, 3);
        assert_eq!(clean.lost_collections, 0, "{clean:?}");
        assert_eq!(clean.coverage, 1.0, "{clean:?}");
    }
}
