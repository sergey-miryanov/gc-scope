//! Records to trace events: the one conversion every output format shares.
//!
//! This is where a decoded [`GcStat`] becomes a trace — where a Collection acquires a slice
//! name, a category, the arguments it reports and the sub-phases it is broken into. No
//! output format repeats any of it; each one encodes the [`TraceEvent`]s it is handed. See
//! [`super::trace_event`] for why that split exists and for the ordering the events carry.
//!
//! Everything here is keyed on **field presence**, never on a version: a build's sub-phases
//! are exactly the ones whose fields its Entry layout defines (see
//! `docs/adr/0007-gcstat-layout-driven-view.md`). Adding a version adds a layout and nothing
//! else.

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

/// Turn one decoded Record into the events that describe it, in the order a format must
/// write them:
///
/// 1. process and thread metadata — **every time**, because deduplication is per output
///    stream and therefore the encoder's job, not this function's;
/// 2. the `Begin` of the GC pause;
/// 3. each intra-pause sub-phase the build carries, as a `Begin`/`End` pair;
/// 4. the `End` of the GC pause;
/// 5. the two counter samples: the per-generation metrics and the heap-size series.
///
/// A phase whose fields this build's layout lacks is skipped, and so is one that resolves to
/// a span no wider than a point — a zero-width slice is noise in every viewer. Both are the
/// producer's decisions; nothing downstream repeats them.
///
/// This function is pure and holds no state between calls, so a fan-out to two formats
/// converts once and hands both the same events.
pub fn convert_record(pid: u32, record: &GcStat) -> Vec<TraceEvent> {
    let tid = record.interpreter_id;
    let ts_start = record.ts_start();
    let ts_stop = record.ts_stop();
    let generation = record.generation;

    let pause_name = format!("GC Pause (gen={})", generation);
    let pause_cat = format!("gc.pause(gen={})", generation);

    // Every event a Record produces carries these two, so a viewer can tell tracks apart
    // after a filter has thrown the track names away.
    let identity: [Arg; 2] = [
        ("generation", i64::from(generation).into()),
        ("iid", tid.into()),
    ];

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
        TraceEvent::Begin {
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
        },
    ];

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

    // The per-generation series. `uncollectable` is carried only when non-zero: it is
    // almost always zero, and a series that is present only when it has something to say
    // is one an operator notices.
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

    fn bare() -> GcStat {
        GcStat::from_fields(0, 0, 1, *FULL, &[("ts_start", 1_000), ("ts_stop", 2_000)])
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
                TraceEvent::Begin { name, .. } => name,
                TraceEvent::End { name, .. } => name,
                TraceEvent::Instant { name, .. } => name,
                TraceEvent::Counter { name, .. } => name,
            })
            .collect()
    }

    fn kinds(events: &[TraceEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                TraceEvent::ProcessMeta { .. } => "M",
                TraceEvent::ThreadMeta { .. } => "M",
                TraceEvent::Begin { .. } => "B",
                TraceEvent::End { .. } => "E",
                TraceEvent::Instant { .. } => "I",
                TraceEvent::Counter { .. } => "C",
            })
            .collect()
    }

    /// The whole emission order for the simplest Record, pinned as one sequence: metadata,
    /// the pause, then its counters. This is the contract [`TraceEvent`] documents, and it
    /// is the part every format depends on.
    #[test]
    fn a_pause_converts_to_metadata_then_a_span_then_its_counters() {
        let events = convert_record(42, &bare());
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

    /// Sub-phases nest inside the pause in emission order — each pair complete, and all of
    /// them between the pause's own `Begin` and `End`.
    #[test]
    fn sub_phases_are_emitted_as_nested_pairs_inside_the_pause() {
        let events = convert_record(1, &full());
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

    /// Metadata is emitted on every call, deliberately: two formats writing two streams each
    /// need their own copy, so only an encoder knows what it has already written. A
    /// conversion that deduplicated would starve the second stream.
    #[test]
    fn metadata_is_emitted_for_every_record_not_deduplicated_here() {
        for _ in 0..3 {
            let events = convert_record(42, &bare());
            assert!(matches!(events[0], TraceEvent::ProcessMeta { .. }));
            assert!(matches!(events[1], TraceEvent::ThreadMeta { .. }));
        }
    }

    /// The tier is the build's field set, not its version: a layout without the phase fields
    /// produces the pause and its counters and nothing else. `GcStat::get` returning `None`
    /// (rather than `Some(0)`) is what carries that.
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
            kinds(&convert_record(1, &record)),
            ["M", "M", "B", "E", "C", "C"]
        );
    }

    /// A phase whose start and stop coincide is dropped by the producer, so no format has to
    /// decide what a zero-width slice means.
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
        let events = convert_record(1, &record);
        assert!(!names(&events).iter().any(|n| n.starts_with("Mark Alive")));
        assert_eq!(kinds(&events), ["M", "M", "B", "E", "C", "C"]);
    }

    /// `duration` is the one float a Record carries, and it must reach the counter as a
    /// float — `Int` would print the same for whole seconds and differently everywhere else.
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
        let events = convert_record(1, &record);
        let TraceEvent::Counter { args, .. } = &events[events.len() - 2] else {
            panic!("expected the metrics counter: {events:?}");
        };
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
            let events = convert_record(1, &record);
            let TraceEvent::Counter { args, .. } = &events[events.len() - 2] else {
                panic!("expected the metrics counter: {events:?}");
            };
            args.iter().map(|&(k, _)| k).collect()
        };

        assert_eq!(metrics_keys(0), ["collected", "candidates", "duration"]);
        assert_eq!(
            metrics_keys(7),
            ["collected", "candidates", "duration", "uncollectable"]
        );
    }

    /// A chained phase begins where the previous one ended. When the build defines none of
    /// the fields it chains onto, it falls back to the pause start rather than being
    /// dropped — the phase ran, and only its start is unknown.
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
        let events = convert_record(1, &record);
        let TraceEvent::Begin { ts_ns, name, .. } = &events[3] else {
            panic!("expected the phase begin: {events:?}");
        };
        assert_eq!(name, "Finalize Garbage (gen=0)");
        assert_eq!(*ts_ns, 60_000, "chained onto the pause start");
    }

    /// Timestamps stay in the nanoseconds CPython published. Converting to a format's own
    /// unit is the encoder's job, and doing it twice is how a trace ends up 1000× off.
    #[test]
    fn timestamps_are_carried_in_nanoseconds() {
        let events = convert_record(1, &bare());
        let TraceEvent::Begin { ts_ns, .. } = &events[2] else {
            panic!("expected the pause begin: {events:?}");
        };
        assert_eq!(*ts_ns, 1_000);
    }
}
