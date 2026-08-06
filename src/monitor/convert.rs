//! Records to trace events: the one conversion every output format shares. A Collection
//! acquires its slice name, category, arguments and sub-phases here and nowhere else. See
//! [`super::trace_event`] for why the split exists and what ordering the events carry.
//!
//! Everything version-dependent here keys on field presence
//! (`docs/adr/0007-gcstat-layout-driven-view.md`): a build has the sub-phases whose fields its
//! Entry layout defines, and sits in the tier its timestamps put it in — spans where they
//! exist, counter samples where they do not
//! (`docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md`). Adding a version adds a
//! layout and nothing else.

use crate::monitor::trace_event::{Arg, TraceEvent};
use crate::remote_debugging::gc_stats::GcStat;

/// How a GC sub-step's start timestamp is found.
enum Start {
    /// An explicit start field; the phase emits only if both this and `stop` are present.
    Explicit(&'static str),
    /// No own start field — begin where a previous phase ended: the first present candidate
    /// (else the pause start). Emits whenever `stop` is present. Mirrors how CPython's later
    /// GC phases chain onto the preceding one.
    Chained(&'static [&'static str]),
}

/// One intra-pause GC sub-step, keyed entirely by layout field names so a build's presence or
/// absence of a phase falls out of whether its fields exist.
struct Phase {
    label: &'static str,
    cat: &'static str,
    start: Start,
    stop: &'static str,
    /// Extra args beyond the always-present `generation`/`iid`: `(arg_key, field_name)`.
    args: &'static [(&'static str, &'static str)],
}

/// The sub-steps in emission order. Adding a well-behaved phase (own start+stop) is a data-only
/// change; the irregular chained-start phases are expressed as [`Start::Chained`].
static PHASES: &[Phase] = &[
    Phase {
        label: "Mark Alive",
        cat: "gc.mark.alive",
        start: Start::Explicit("ts_mark_alive_start"),
        stop: "ts_mark_alive_stop",
        args: &[("alive_size", "alive_size")],
    },
    Phase {
        label: "Fill increment",
        cat: "gc.increment",
        start: Start::Explicit("ts_fill_increment_start"),
        stop: "ts_fill_increment_stop",
        args: &[("increment_size", "increment_size")],
    },
    Phase {
        label: "Deduce Unreachable",
        cat: "gc.deduce",
        start: Start::Explicit("ts_deduce_unreachable_start"),
        stop: "ts_deduce_unreachable_stop",
        args: &[("candidates", "candidates")],
    },
    Phase {
        label: "Handle Weakrefs Callbacks",
        cat: "gc.weakrefs",
        start: Start::Explicit("ts_handle_weakref_callbacks_start"),
        stop: "ts_handle_weakref_callbacks_stop",
        args: &[],
    },
    Phase {
        label: "Finalize Garbage",
        cat: "gc.finalize",
        start: Start::Chained(&[
            "ts_handle_weakref_callbacks_stop",
            "ts_deduce_unreachable_stop",
        ]),
        stop: "ts_finalize_garbage_stop",
        args: &[("finalized_garbage_count", "finalized_garbage_count")],
    },
    Phase {
        label: "Handle Resurrected",
        cat: "gc.resurrect",
        start: Start::Chained(&["ts_finalize_garbage_stop"]),
        stop: "ts_handle_resurrected_stop",
        args: &[],
    },
    Phase {
        label: "Clear Weakrefs",
        cat: "gc.clear_weakrefs",
        start: Start::Chained(&["ts_handle_resurrected_stop"]),
        stop: "ts_clear_weakrefs_stop",
        args: &[("clear_weakrefs_count", "clear_weakrefs_count")],
    },
    Phase {
        label: "Delete Garbage",
        cat: "gc.delete",
        start: Start::Explicit("ts_delete_garbage_start"),
        stop: "ts_delete_garbage_stop",
        args: &[("deleted_garbage_count", "deleted_garbage_count")],
    },
];

/// Turn one decoded Record into the events that describe it.
///
/// What a build produces follows from its Entry layout, not its version: one publishing the
/// pause timestamps describes each Collection as a span ([`pause_events`]), one without them
/// has only cumulative counts to report ([`count_events`]). No version is compared here or
/// downstream.
///
/// `observed_at_ns` is the Observer's clock at the poll that read this Record. Only a build
/// with no timestamps of its own uses it, having no other timeline.
///
/// Pure and stateless, so a fan-out to two formats converts once. Metadata leads every call:
/// deduplication is per output stream, so it belongs to the encoder.
pub fn convert_record(pid: u32, record: &GcStat, observed_at_ns: i64) -> Vec<TraceEvent> {
    let tid = record.interpreter_id;
    let body = if record.has_timing() {
        pause_events(pid, record)
    } else {
        count_events(pid, record, observed_at_ns)
    };
    if body.is_empty() {
        return Vec::new();
    }

    let mut events = vec![
        TraceEvent::ProcessMeta {
            pid,
            name: format!("python {}", pid),
        },
        TraceEvent::ThreadMeta {
            pid,
            tid,
            name: format!("{}:{}", pid, tid),
        },
    ];
    events.extend(body);
    events
}

/// The events for a Record from a build that publishes no timestamps: one counter sample
/// carrying the Lifetime totals CPython does publish, whose rise over a run is the GC rate.
/// The sample holds the generation's totals as of the read, not one Collection: an inline
/// Entry carries nothing per-Collection.
///
/// Nothing pause-derived appears, down to the zero-width span. A `duration` of `0` reads as
/// "this process spends no time in GC" when the truth is that the build cannot say (spec 0011
/// §2). The sample sits on the Observer's clock, the only timeline available.
fn count_events(pid: u32, record: &GcStat, observed_at_ns: i64) -> Vec<TraceEvent> {
    let mut counts: Vec<Arg> = Vec::new();
    for name in ["collections", "collected"] {
        if let Some(value) = record.get(name) {
            counts.push((name, value.into()));
        }
    }
    // `uncollectable` rides along only when non-zero, the same rule the timed series follows,
    // so an operator notices it when it appears.
    if record.uncollectable() > 0 {
        counts.push(("uncollectable", record.uncollectable().into()));
    }
    if counts.is_empty() {
        // A layout carrying none of the counts has nothing to draw a track from. No
        // registered build reaches here.
        return Vec::new();
    }

    vec![TraceEvent::Counter {
        pid,
        tid: record.interpreter_id,
        ts_ns: observed_at_ns,
        name: format!("G{}", record.generation),
        args: counts,
    }]
}

/// The events for a Record from a build that publishes the pause timestamps, in the order a
/// format writes them:
///
/// 1. the `Begin` of the GC pause;
/// 2. each sub-phase the build carries, as a `Begin`/`End` pair;
/// 3. the `End` of the GC pause;
/// 4. two counter samples, the per-generation metrics and the heap-size series.
///
/// A phase is skipped when this build's layout lacks its fields, and when it resolves to a
/// span no wider than a point. Nothing downstream repeats either decision.
fn pause_events(pid: u32, record: &GcStat) -> Vec<TraceEvent> {
    let tid = record.interpreter_id;
    let ts_start = record.ts_start();
    let ts_stop = record.ts_stop();
    let generation = record.generation;

    let pause_name = format!("GC Pause (gen={})", generation);
    let pause_cat = format!("gc.pause(gen={})", generation);

    // Carried by every event, so a viewer can tell tracks apart once a filter has thrown
    // the track names away.
    let identity: [Arg; 2] = [
        ("generation", i64::from(generation).into()),
        ("iid", tid.into()),
    ];

    let mut events = vec![TraceEvent::Begin {
        pid,
        tid,
        ts_ns: ts_start,
        name: pause_name.clone(),
        cat: pause_cat.clone(),
        args: identity
            .iter()
            .copied()
            .chain([
                ("collections", record.collections().into()),
                ("heap_size", record.heap_size().into()),
                ("collected", record.collected().into()),
                ("uncollectable", record.uncollectable().into()),
                ("candidates", record.candidates().into()),
            ])
            .collect(),
    }];

    for phase in PHASES {
        let Some(stop) = record.get(phase.stop) else {
            continue;
        };
        let start = match &phase.start {
            Start::Explicit(field) => match record.get(field) {
                Some(v) => v,
                None => continue,
            },
            Start::Chained(candidates) => candidates
                .iter()
                .find_map(|&c| record.get(c))
                .unwrap_or(ts_start),
        };
        if stop <= start {
            continue;
        }

        let name = format!("{} (gen={})", phase.label, generation);
        let cat = format!("{}(gen={})", phase.cat, generation);
        let args: Vec<Arg> = identity
            .iter()
            .copied()
            .chain(
                phase
                    .args
                    .iter()
                    .map(|&(key, field)| (key, record.get(field).unwrap_or(0).into())),
            )
            .collect();

        events.push(TraceEvent::Begin {
            pid,
            tid,
            ts_ns: start,
            name: name.clone(),
            cat: cat.clone(),
            args,
        });
        events.push(TraceEvent::End {
            pid,
            tid,
            ts_ns: stop,
            name,
            cat,
        });
    }

    events.push(TraceEvent::End {
        pid,
        tid,
        ts_ns: ts_stop,
        name: pause_name,
        cat: pause_cat,
    });

    // The per-generation series. `uncollectable` rides along only when non-zero, so an
    // operator notices it when it appears.
    let mut metrics: Vec<Arg> = vec![
        ("collected", record.collected().into()),
        ("candidates", record.candidates().into()),
        ("duration", record.duration().into()),
    ];
    if record.uncollectable() > 0 {
        metrics.push(("uncollectable", record.uncollectable().into()));
    }
    events.push(TraceEvent::Counter {
        pid,
        tid,
        ts_ns: ts_start,
        name: format!("G{}", generation),
        args: metrics,
    });

    // Heap size is a single-component series, so it takes its label from the argument key.
    events.push(TraceEvent::Counter {
        pid,
        tid,
        ts_ns: ts_start,
        name: String::new(),
        args: vec![("heap_size", record.heap_size().into())],
    });

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::trace_event::ArgValue;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A build carrying every field the conversion knows about, so a Record over it can set
    /// any phase's timestamps. A real regular build has only a subset.
    static FULL: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
            "candidates",
            "duration",
            "heap_size",
            "increment_size",
            "alive_size",
            "finalized_garbage_count",
            "clear_weakrefs_count",
            "deleted_garbage_count",
            "ts_mark_alive_start",
            "ts_mark_alive_stop",
            "ts_fill_increment_start",
            "ts_fill_increment_stop",
            "ts_deduce_unreachable_start",
            "ts_deduce_unreachable_stop",
            "ts_handle_weakref_callbacks_start",
            "ts_handle_weakref_callbacks_stop",
            "ts_finalize_garbage_stop",
            "ts_handle_resurrected_stop",
            "ts_clear_weakrefs_stop",
            "ts_delete_garbage_start",
            "ts_delete_garbage_stop",
        ])
    });

    /// A build with no timing at all: the cumulative counts and nothing else. This and
    /// [`REGULAR`] are the two tiers, each expressed as a layout rather than as a version.
    static COUNTS_ONLY: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["collections", "collected", "uncollectable"]));

    /// A standard build: the core counters and timestamps, none of the phase fields.
    static REGULAR: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
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

    /// The Observer's clock reading a test hands over when its value is not what the test is
    /// about. Distinctive, so that a build with timing consulting it — which would be the
    /// bug — shows up as this number in the output rather than as a plausible one.
    const OBSERVER_CLOCK: i64 = 777_000;

    fn bare() -> GcStat {
        GcStat::from_fields(0, 0, 1, *FULL, &[("ts_start", 1_000), ("ts_stop", 2_000)])
    }

    /// A Record from a build with no timing: cumulative counts, in `generation`.
    fn counted(generation: u32, collections: i64, collected: i64, uncollectable: i64) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            0,
            *COUNTS_ONLY,
            &[
                ("collections", collections),
                ("collected", collected),
                ("uncollectable", uncollectable),
            ],
        )
    }

    /// Every phase's timestamps set to a non-empty, increasing range, so all eight fire.
    fn full() -> GcStat {
        GcStat::from_fields(
            1,
            0,
            3,
            *FULL,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 11_000),
                ("ts_mark_alive_start", 2_000),
                ("ts_mark_alive_stop", 3_000),
                ("ts_fill_increment_start", 3_000),
                ("ts_fill_increment_stop", 4_000),
                ("ts_deduce_unreachable_start", 4_000),
                ("ts_deduce_unreachable_stop", 5_000),
                ("ts_handle_weakref_callbacks_start", 5_000),
                ("ts_handle_weakref_callbacks_stop", 6_000),
                ("ts_finalize_garbage_stop", 7_000),
                ("ts_handle_resurrected_stop", 8_000),
                ("ts_clear_weakrefs_stop", 9_000),
                ("ts_delete_garbage_start", 9_000),
                ("ts_delete_garbage_stop", 10_000),
            ],
        )
    }

    fn names(events: &[TraceEvent]) -> Vec<&str> {
        events
            .iter()
            .map(|e| match e {
                TraceEvent::ProcessMeta { .. } => "process_meta",
                TraceEvent::ThreadMeta { .. } => "thread_meta",
                TraceEvent::Begin { name, .. }
                | TraceEvent::End { name, .. }
                | TraceEvent::Instant { name, .. }
                | TraceEvent::Counter { name, .. } => name,
            })
            .collect()
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

    /// Every `Begin` as `(name, ts_ns)`, so a test can name the span it means instead of
    /// indexing the sequence.
    fn begins(events: &[TraceEvent]) -> Vec<(&str, i64)> {
        events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Begin { name, ts_ns, .. } => Some((name.as_str(), *ts_ns)),
                _ => None,
            })
            .collect()
    }

    /// Every `Counter` as `(name, args)`.
    fn counters(events: &[TraceEvent]) -> Vec<(&str, &[Arg])> {
        events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Counter { name, args, .. } => Some((name.as_str(), args.as_slice())),
                _ => None,
            })
            .collect()
    }

    /// `names` and `kinds` are the lens every test below reads events through, so a wrong
    /// label here would let an assertion pass against the wrong sequence. `Instant` has no
    /// producer yet, which makes this the only place it gets classified.
    #[test]
    fn the_event_lens_labels_every_variant() {
        let events = [
            TraceEvent::ProcessMeta {
                pid: 1,
                name: "python 1".to_string(),
            },
            TraceEvent::ThreadMeta {
                pid: 1,
                tid: 0,
                name: "1:0".to_string(),
            },
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 0,
                name: "begin".to_string(),
                cat: "cat".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 1,
                name: "end".to_string(),
                cat: "cat".to_string(),
            },
            TraceEvent::Instant {
                pid: 1,
                ts_ns: 2,
                name: "instant".to_string(),
            },
            TraceEvent::Counter {
                pid: 1,
                tid: 0,
                ts_ns: 3,
                name: "counter".to_string(),
                args: vec![],
            },
        ];

        assert_eq!(kinds(&events), ["M", "M", "B", "E", "I", "C"]);
        assert_eq!(
            names(&events),
            [
                "process_meta",
                "thread_meta",
                "begin",
                "end",
                "instant",
                "counter"
            ]
        );
    }

    /// The emission order for the simplest Record, pinned as one sequence. This is the
    /// contract [`TraceEvent`] documents and what every format depends on.
    #[test]
    fn a_pause_converts_to_metadata_then_a_span_then_its_counters() {
        let events = convert_record(42, &bare(), OBSERVER_CLOCK);
        assert_eq!(
            names(&events),
            [
                "process_meta",
                "thread_meta",
                "GC Pause (gen=0)",
                "GC Pause (gen=0)",
                "G0",
                "",
            ]
        );
        assert_eq!(kinds(&events), ["M", "M", "B", "E", "C", "C"]);
    }

    /// Sub-phases nest inside the pause in emission order: each pair complete, all of them
    /// between the pause's own `Begin` and `End`.
    #[test]
    fn sub_phases_are_emitted_as_nested_pairs_inside_the_pause() {
        let events = convert_record(1, &full(), OBSERVER_CLOCK);
        assert_eq!(
            names(&events),
            [
                "process_meta",
                "thread_meta",
                "GC Pause (gen=1)",
                "Mark Alive (gen=1)",
                "Mark Alive (gen=1)",
                "Fill increment (gen=1)",
                "Fill increment (gen=1)",
                "Deduce Unreachable (gen=1)",
                "Deduce Unreachable (gen=1)",
                "Handle Weakrefs Callbacks (gen=1)",
                "Handle Weakrefs Callbacks (gen=1)",
                "Finalize Garbage (gen=1)",
                "Finalize Garbage (gen=1)",
                "Handle Resurrected (gen=1)",
                "Handle Resurrected (gen=1)",
                "Clear Weakrefs (gen=1)",
                "Clear Weakrefs (gen=1)",
                "Delete Garbage (gen=1)",
                "Delete Garbage (gen=1)",
                "GC Pause (gen=1)",
                "G1",
                "",
            ]
        );
        assert_eq!(
            kinds(&events),
            [
                "M", "M", "B", "B", "E", "B", "E", "B", "E", "B", "E", "B", "E", "B", "E", "B",
                "E", "B", "E", "E", "C", "C",
            ]
        );
    }

    /// Metadata goes out on every call: two formats writing two streams each need their own
    /// copy, and only an encoder knows what it has already written.
    #[test]
    fn metadata_is_emitted_for_every_record_not_deduplicated_here() {
        for _ in 0..3 {
            let events = convert_record(42, &bare(), OBSERVER_CLOCK);
            assert!(matches!(events[0], TraceEvent::ProcessMeta { .. }));
            assert!(matches!(events[1], TraceEvent::ThreadMeta { .. }));
        }
    }

    /// A layout without the phase fields produces the pause and its counters, nothing else.
    /// `GcStat::get` returning `None` rather than `Some(0)` is what carries the distinction.
    #[test]
    fn a_build_without_phase_fields_produces_no_sub_phases() {
        let record = GcStat::from_fields(
            0,
            0,
            1,
            *REGULAR,
            &[("ts_start", 1_000), ("ts_stop", 9_000)],
        );
        assert_eq!(
            kinds(&convert_record(1, &record, OBSERVER_CLOCK)),
            ["M", "M", "B", "E", "C", "C"]
        );
    }

    /// The producer drops a phase whose start and stop coincide, so no format has to decide
    /// what a zero-width slice means.
    #[test]
    fn a_zero_width_phase_is_dropped() {
        let record = GcStat::from_fields(
            0,
            0,
            1,
            *FULL,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 2_000),
                ("ts_mark_alive_start", 5_000),
                ("ts_mark_alive_stop", 5_000),
            ],
        );
        let events = convert_record(1, &record, OBSERVER_CLOCK);
        assert!(!names(&events).iter().any(|n| n.starts_with("Mark Alive")));
        assert_eq!(kinds(&events), ["M", "M", "B", "E", "C", "C"]);
    }

    /// `duration` must reach the counter as a float. `Int` prints the same for whole seconds
    /// and differently everywhere else.
    #[test]
    fn the_metrics_counter_carries_duration_as_a_float() {
        let record = GcStat::from_fields(
            0,
            0,
            1,
            *FULL,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 2_000),
                ("duration", f64::to_bits(0.00125) as i64),
            ],
        );
        let events = convert_record(1, &record, OBSERVER_CLOCK);
        let (name, args) = counters(&events)[0];
        assert_eq!(name, "G0");
        assert!(
            args.contains(&("duration", ArgValue::Float(0.00125))),
            "{args:?}"
        );
    }

    /// `uncollectable` rides on the metrics counter only when there is something to report.
    #[test]
    fn the_metrics_counter_carries_uncollectable_only_when_non_zero() {
        let metrics_keys = |uncollectable: i64| -> Vec<&'static str> {
            let record = GcStat::from_fields(
                0,
                0,
                1,
                *FULL,
                &[
                    ("ts_start", 1_000),
                    ("ts_stop", 2_000),
                    ("uncollectable", uncollectable),
                ],
            );
            let events = convert_record(1, &record, OBSERVER_CLOCK);
            counters(&events)[0].1.iter().map(|&(k, _)| k).collect()
        };

        assert_eq!(metrics_keys(0), ["collected", "candidates", "duration"]);
        assert_eq!(
            metrics_keys(7),
            ["collected", "candidates", "duration", "uncollectable"]
        );
    }

    /// A chained phase begins where the previous one ended. With none of those fields in the
    /// build it falls back to the pause start rather than being dropped: the phase ran, only
    /// its start is unknown.
    #[test]
    fn a_chained_phase_falls_back_to_the_pause_start_when_its_predecessors_are_absent() {
        let layout = seq_layout(&["ts_start", "ts_stop", "ts_finalize_garbage_stop"]);
        let record = GcStat::from_fields(
            0,
            0,
            1,
            layout,
            &[
                ("ts_start", 60_000),
                ("ts_stop", 70_000),
                ("ts_finalize_garbage_stop", 65_000),
            ],
        );
        let events = convert_record(1, &record, OBSERVER_CLOCK);
        assert_eq!(
            begins(&events),
            [
                ("GC Pause (gen=0)", 60_000),
                ("Finalize Garbage (gen=0)", 60_000), // chained onto the pause start
            ]
        );
    }

    /// The other half of that branch: a phase carrying its own start field is skipped when
    /// the build lacks it, rather than chaining. Only an `Explicit` phase can be in that
    /// state, and only on a build that publishes the stop without the start.
    #[test]
    fn an_explicit_phase_is_skipped_when_the_build_lacks_its_start_field() {
        let layout = seq_layout(&["ts_start", "ts_stop", "ts_mark_alive_stop"]);
        let record = GcStat::from_fields(
            0,
            0,
            1,
            layout,
            &[
                ("ts_start", 60_000),
                ("ts_stop", 70_000),
                ("ts_mark_alive_stop", 65_000),
            ],
        );
        let events = convert_record(1, &record, OBSERVER_CLOCK);
        assert_eq!(begins(&events), [("GC Pause (gen=0)", 60_000)]);
    }

    /// Timestamps stay in the nanoseconds CPython published. Converting is the encoder's
    /// job; doing it twice puts a trace 1000× off.
    #[test]
    fn timestamps_are_carried_in_nanoseconds() {
        let events = convert_record(1, &bare(), OBSERVER_CLOCK);
        assert_eq!(begins(&events), [("GC Pause (gen=0)", 1_000)]);
        assert_eq!(counters(&events)[1], ("", &[("heap_size", 0.into())][..]));
    }

    // ── the tier a build's layout puts it in ──────────────────────────────────────────

    /// A build whose layout has no timestamps has no pause to draw. Its Records are
    /// cumulative counts, so they become counter samples: a span here would be zero-width at
    /// the epoch, reporting a pause the build never measured.
    #[test]
    fn a_build_without_timing_produces_counter_samples_and_no_spans() {
        let events = convert_record(9, &counted(1, 40, 900, 0), OBSERVER_CLOCK);
        assert_eq!(kinds(&events), ["M", "M", "C"]);
        assert_eq!(names(&events), ["process_meta", "thread_meta", "G1"]);
        assert_eq!(
            counters(&events)[0].1,
            [("collections", 40.into()), ("collected", 900.into())]
        );
    }

    /// Everything a pause would have supplied is absent rather than zero. An operator reading
    /// `duration: 0` off such a trace concludes the process spends no time in GC (spec 0011
    /// §2).
    #[test]
    fn a_build_without_timing_reports_no_pause_derived_value() {
        let events = convert_record(9, &counted(0, 3, 4, 5), OBSERVER_CLOCK);
        let keys: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Counter { args, .. } => Some(args),
                _ => None,
            })
            .flatten()
            .map(|&(key, _)| key)
            .collect();
        for absent in ["duration", "candidates", "heap_size"] {
            assert!(
                !keys.contains(&absent),
                "{absent} is not this build's to report"
            );
        }
    }

    /// Such a build publishes no clock, so its samples take the Observer's, the only timeline
    /// the trace has. Every sample from one poll shares it, which is what makes the counts
    /// read as a rate.
    #[test]
    fn counters_from_a_build_without_timing_take_the_observers_clock() {
        let events = convert_record(9, &counted(2, 7, 8, 0), 4_200_000);
        let times: Vec<i64> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::Counter { ts_ns, .. } => Some(*ts_ns),
                _ => None,
            })
            .collect();
        assert_eq!(times, [4_200_000]);
    }

    /// `uncollectable` rides along only when non-zero, the same rule the timed series
    /// follows, so an operator notices it when it appears.
    #[test]
    fn a_build_without_timing_carries_uncollectable_only_when_non_zero() {
        let keys = |uncollectable: i64| -> Vec<&'static str> {
            let events = convert_record(9, &counted(0, 1, 2, uncollectable), OBSERVER_CLOCK);
            counters(&events)[0].1.iter().map(|&(k, _)| k).collect()
        };
        assert_eq!(keys(0), ["collections", "collected"]);
        assert_eq!(keys(6), ["collections", "collected", "uncollectable"]);
    }

    /// The other tier is untouched: a build with its own timestamps places every event on
    /// them and never reads the Observer's clock.
    #[test]
    fn a_build_with_timing_never_consults_the_observers_clock() {
        let events = convert_record(1, &bare(), 4_200_000);
        assert_eq!(begins(&events), [("GC Pause (gen=0)", 1_000)]);
        assert!(
            events.iter().all(|e| !matches!(
                e,
                TraceEvent::Begin {
                    ts_ns: 4_200_000,
                    ..
                } | TraceEvent::End {
                    ts_ns: 4_200_000,
                    ..
                } | TraceEvent::Counter {
                    ts_ns: 4_200_000,
                    ..
                }
            )),
            "{events:?}"
        );
    }
}
