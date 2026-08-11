//! The hand-rolled Perfetto encoder, read back by a decoder gcscope did not write.
//!
//! This is the half of [ADR 0021](../docs/adr/0021-hand-rolled-perfetto-encoder.md) that makes
//! the other half safe. A hand-rolled protobuf writer fails **open**: a wrong field number or a
//! truncated varint produces a file that parses *to something else*, and nothing in gcscope's
//! own tests can catch it, because its writer and any reader it owns agree by construction.
//! Everything here goes out through `monitor::exporters` and comes back through the `perfetto`
//! package.
//!
//! Three questions, three tests:
//!
//! 1. does the field-number table say what the schema says (`proto_constants.py`),
//! 2. does each primitive decode to the value that was written (the wire view),
//! 3. does a trace the exporter wrote hold together and *mean* what it should (both views).
//!
//! `#[ignore]`d, per the live-matrix convention: the default `cargo test` stays green on a
//! machine with no `perfetto` package. Run with
//!   cargo test --test perfetto_encoder -- --ignored
//! after `pip install -r tests/harness/requirements.txt`.

mod common;

use std::path::PathBuf;

use common::perfetto::{check_constants, dump, dump_wire_only, harness_python_or_skip, keys};
use gcscope::monitor::clock::RunClock;
use gcscope::monitor::exporters::perfetto::PerfettoExporter;
use gcscope::monitor::exporters::perfetto_proto::{
    ENUM_VALUES, FIELDS, builtin_clock, trace, trace_packet, track_descriptor, track_event,
    track_event_type,
};
use gcscope::monitor::exporters::protobuf::{
    put_double, put_int32, put_int64, put_message, put_string, put_uint64,
};
use gcscope::monitor::exporters::{EventsExporter, ProcessLifecycle};
use gcscope::monitor::trace_event::{ArgValue, TraceEvent};

fn trace_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "gcscope_encoder_{}_{}.perfetto-trace",
        std::process::id(),
        tag
    ))
}

// ---------------------------------------------------------------------------------
// 1. The field-number table
// ---------------------------------------------------------------------------------

/// The claims document `proto_constants.py` reads, built from the table the encoder writes
/// with. Hand-written, as the rest of gcscope's JSON is: every string in it is a `&'static
/// str` from that table, so there is nothing here a target could put a quote in.
fn claims() -> String {
    let fields: Vec<String> = FIELDS
        .iter()
        .map(|(message, field, number)| format!("[\"{message}\",\"{field}\",{number}]"))
        .collect();
    let enums: Vec<String> = ENUM_VALUES
        .iter()
        .map(|(message, name, value, number)| {
            format!("[\"{message}\",\"{name}\",\"{value}\",{number}]")
        })
        .collect();
    format!(
        "{{\"fields\":[{}],\"enums\":[{}]}}",
        fields.join(","),
        enums.join(",")
    )
}

/// Every field number and enum value gcscope writes with, re-derived from the official
/// descriptors. This is what makes a transcribed table safe to keep: the numbers are not
/// trusted, they are checked, on every run of this leg.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn every_constant_the_encoder_writes_with_matches_the_schema() {
    let Some(python) = harness_python_or_skip("perfetto constants") else {
        return;
    };

    let verdict = check_constants(&python, &claims());

    assert_eq!(
        verdict["ok"], true,
        "the table disagrees with the schema: {}",
        verdict["problems"]
    );
    // Told how many claims were checked, not only that none failed: an empty table would pass
    // every check in it, and this is the difference between the two.
    assert_eq!(verdict["checked"]["fields"], FIELDS.len());
    assert_eq!(verdict["checked"]["enums"], ENUM_VALUES.len());
    assert!(!FIELDS.is_empty() && !ENUM_VALUES.is_empty());
}

// ---------------------------------------------------------------------------------
// 2. The wire primitives
// ---------------------------------------------------------------------------------

/// A trace built to exercise one primitive per field, rather than to describe anything.
///
/// Each value is chosen where an encoder goes wrong: the varint boundaries, a negative that a
/// 32-bit mask would turn positive, a double whose bits are not its decimal, a string whose
/// byte length is not its character count, and a submessage with nothing in it.
fn primitive_probe() -> Vec<u8> {
    fn packet(out: &mut Vec<u8>, body: &[u8]) {
        put_message(out, trace::PACKET, body);
    }

    fn with_sequence(mut body: Vec<u8>) -> Vec<u8> {
        let mut framed = Vec::new();
        put_uint64(&mut framed, trace_packet::TRUSTED_PACKET_SEQUENCE_ID, 1);
        framed.append(&mut body);
        framed
    }

    let mut out = Vec::new();

    // A descriptor carrying a string, a negative int32 and an empty submessage.
    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, 1);
    put_string(&mut descriptor, track_descriptor::NAME, "héllo");
    put_int32(&mut descriptor, track_descriptor::SIBLING_ORDER_RANK, -1);
    put_message(&mut descriptor, track_descriptor::PROCESS, &[]);
    let mut body = Vec::new();
    put_message(&mut body, trace_packet::TRACK_DESCRIPTOR, &descriptor);
    packet(&mut out, &with_sequence(body));

    // The other end of the int32 range.
    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, 2);
    put_int32(
        &mut descriptor,
        track_descriptor::SIBLING_ORDER_RANK,
        i32::MIN,
    );
    let mut body = Vec::new();
    put_message(&mut body, trace_packet::TRACK_DESCRIPTOR, &descriptor);
    packet(&mut out, &with_sequence(body));

    // A fixed64 double, on a track that was declared, so the trace still holds together.
    let mut event = Vec::new();
    put_uint64(&mut event, track_event::TRACK_UUID, 1);
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::COUNTER),
    );
    put_double(&mut event, track_event::DOUBLE_COUNTER_VALUE, 0.1);
    let mut body = Vec::new();
    put_uint64(&mut body, trace_packet::TIMESTAMP, 0);
    put_message(&mut body, trace_packet::TRACK_EVENT, &event);
    packet(&mut out, &with_sequence(body));

    // A negative int64, in its own packet: `counter_value` and `double_counter_value` share a
    // oneof, so writing both would leave only the second. This is the value a writer that
    // masked to 32 bits gets wrong in a way a reader notices — an `int32` field decodes the
    // truncated form back to the same negative, an `int64` field does not.
    let mut event = Vec::new();
    put_uint64(&mut event, track_event::TRACK_UUID, 1);
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::COUNTER),
    );
    put_int64(&mut event, track_event::COUNTER_VALUE, i64::MIN);
    let mut body = Vec::new();
    put_uint64(&mut body, trace_packet::TIMESTAMP, 1);
    put_message(&mut body, trace_packet::TRACK_EVENT, &event);
    packet(&mut out, &with_sequence(body));

    // The far end of the varint range.
    let mut body = Vec::new();
    put_uint64(&mut body, trace_packet::TIMESTAMP, u64::MAX);
    packet(&mut out, &with_sequence(body));

    out
}

#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn every_primitive_decodes_to_the_value_that_was_written() {
    let Some(python) = harness_python_or_skip("perfetto primitives") else {
        return;
    };
    let path = trace_path("primitives");
    std::fs::write(&path, primitive_probe()).unwrap();

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());
    let packets = document.packets();
    assert_eq!(packets.len(), 5);

    // The whole descriptor, so a field written that nobody asked for fails here too.
    let first = &packets[0]["track_descriptor"];
    assert_eq!(
        keys(first),
        ["name", "process", "sibling_order_rank", "uuid"]
    );
    assert_eq!(first["name"], "héllo");
    assert_eq!(first["sibling_order_rank"], -1);
    // An empty submessage is a field that is present and says nothing — not an absent field.
    assert_eq!(keys(&first["process"]), Vec::<&str>::new());

    assert_eq!(
        packets[1]["track_descriptor"]["sibling_order_rank"],
        i32::MIN
    );

    let value = packets[2]["track_event"]["double_counter_value"]
        .as_f64()
        .expect("a double decodes as a number");
    assert_eq!(value, 0.1, "the fixed64 bits are not the decimal");

    // A negative that only a sign-extended varint survives. The ten-byte form the spec asks
    // for is pinned in the crate's own unit tests; what this asserts is the value a reader
    // gets, and a writer that truncated to 32 bits gives a different one here.
    assert_eq!(
        packets[3]["track_event"]["counter_value"],
        i64::MIN.to_string()
    );

    // Written as strings, because a double cannot hold the last of them.
    let stamps: Vec<&str> = packets
        .iter()
        .filter_map(|packet| packet["timestamp"].as_str())
        .collect();
    assert_eq!(stamps, ["0", "1", "18446744073709551615"]);
}

// ---------------------------------------------------------------------------------
// 3. A trace the exporter wrote
// ---------------------------------------------------------------------------------

/// An exporter anchored at the origin of both clocks.
///
/// A live anchor is the machine's monotonic counter, which is around 7×10¹⁴ after a week of
/// uptime — so a test writing a Collection at `ts_ns: 1_000` would be claiming one a microsecond
/// after boot, and trace-processor would convert it to a point before the trace began. Anchoring
/// at zero makes gcscope's domain and Perfetto's trace clock the same numbers, so a test can
/// write timestamps a human can read and still assert what the query engine did with them.
fn exporter() -> PerfettoExporter {
    PerfettoExporter::with_clock(RunClock::at(0, 0))
}

fn export(events: &[TraceEvent], tag: &str) -> PathBuf {
    let path = trace_path(tag);
    let mut exporter = exporter();
    exporter.open(&path).unwrap();
    exporter.add_events(events);
    exporter.close().unwrap();
    path
}

fn process_meta(pid: u32) -> TraceEvent {
    TraceEvent::ProcessMeta {
        pid,
        name: format!("python {pid}"),
    }
}

/// The first bytes gcscope writes in this format, read back by both halves of the harness. The
/// wire view settles that the root descriptor carries the ordering hints and nothing else; the
/// query engine settles that a wire-valid trace *means* a process, which is the only thing that
/// distinguishes a real trace from bytes that happen to parse.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn a_trace_the_exporter_wrote_holds_together_and_names_its_process() {
    let Some(python) = harness_python_or_skip("perfetto exporter") else {
        return;
    };
    let path = export(&[process_meta(4242)], "exported");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());
    assert_eq!(document.exit_code, 0);

    let packets = document.packets();
    assert_eq!(
        packets.len(),
        3,
        "the clock snapshot, the root descriptor and one process"
    );

    // The snapshot: one instant on two clocks, which is what every timestamp after it is read
    // through. Without a path from gcscope's domain to the one trace time is measured in,
    // trace-processor drops the events rather than guessing.
    let clocks = packets[0]["clock_snapshot"]["clocks"]
        .as_array()
        .expect("the trace opens with a clock snapshot");
    // `clock_id` is a bare `uint32` rather than the `BuiltinClock` enum, so both arrive as
    // numbers and 6 has to be read as BOOTTIME. That it is BOOTTIME and not MONOTONIC is the
    // whole claim: the other two leave trace-processor with no path to trace time, and it drops
    // every event rather than saying so.
    let domains: Vec<i64> = clocks
        .iter()
        .filter_map(|clock| clock["clock_id"].as_i64())
        .collect();
    assert_eq!(
        domains,
        [
            i64::from(builtin_clock::BOOTTIME),
            64, // gcscope's own, above the 63 Perfetto reserves for the clocks it knows
        ],
        "{clocks:?}"
    );

    // The root: uuid 0 and the two ordering hints, and no name, process or counter — a
    // descriptor with any of those becomes a track row in the UI, which this must not.
    let root = &packets[1]["track_descriptor"];
    assert_eq!(keys(root), ["process_ordering", "thread_ordering", "uuid"]);
    assert_eq!(
        root["uuid"], "0",
        "uuid is 64-bit, so it arrives as a string"
    );
    assert_eq!(root["process_ordering"], "PROCESS_ORDERING_EXPLICIT");
    assert_eq!(root["thread_ordering"], "THREAD_ORDERING_EXPLICIT");

    // The process track, whole. Asserted against what the `TraceEvent` said — `process_meta`
    // built the name above — rather than against a copy of what the encoder emitted last time.
    let process = &packets[2]["track_descriptor"];
    assert_eq!(
        keys(process),
        [
            "child_ordering",
            "name",
            "process",
            "sibling_order_rank",
            "uuid"
        ]
    );
    // `child_ordering` is what makes the ranks on the interpreter tracks *under* this one count:
    // Perfetto honours a child's `sibling_order_rank` only where its parent says the ordering is
    // explicit. Without it the ranks are written and inert.
    // Rendered as the enum's own spelling, so this also confirms 3 landed in a field typed
    // `ChildTracksOrdering` rather than in some numeric field that would take any value.
    assert_eq!(process["child_ordering"], "EXPLICIT");
    assert_ne!(process["uuid"], "0");
    assert_eq!(process["name"], "python 4242");
    assert_eq!(keys(&process["process"]), ["pid", "process_name"]);
    assert_eq!(process["process"]["pid"], 4242);
    assert_eq!(process["process"]["process_name"], "python 4242");

    // `sibling_order_rank` comes from the wire view because **trace-processor exposes no column
    // for it**: it is a UI ordering hint, not a property of a track the query engine models.
    // Reaching for some other column that happens to exist would assert something else.
    assert_eq!(process["sibling_order_rank"], 0);

    // And no `start_timestamp_ns`: it wants the moment the process began on the target's clock,
    // which gcscope has no reading of. The key-set assertion above is what enforces the
    // absence; this says why it is deliberate.
    assert!(process["process"].get("start_timestamp_ns").is_none());

    let pids: Vec<i64> = document
        .rows("process")
        .iter()
        .filter_map(|row| row["pid"].as_i64())
        .collect();
    assert!(
        pids.contains(&4242),
        "the query engine found no such process: {pids:?}"
    );
}

/// User story 4, asserted rather than screenshotted: a process gcscope attached to but never
/// read a Record from is in the trace, drawn, and named by the query engine.
///
/// The marker is what makes it drawn — Perfetto's UI hides a track with no events on it — and
/// it is the one timestamp gcscope generates itself, so it says which clock it is on.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn a_process_that_collected_nothing_is_still_in_the_trace() {
    let Some(python) = harness_python_or_skip("perfetto silent process") else {
        return;
    };
    let path = trace_path("silent");
    let mut exporter = exporter();
    exporter.open(&path).unwrap();
    // Exactly what the monitor does for a process it attached to: announce it, then mark it
    // started. No Record ever arrives.
    exporter.add_events(&[process_meta(4242), process_meta(7)]);
    exporter.mark_process_lifecycle(4242, ProcessLifecycle::Started, Some(1_000));
    exporter.mark_process_lifecycle(7, ProcessLifecycle::Started, Some(2_000));
    exporter.close().unwrap();

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let mut pids: Vec<i64> = document
        .rows("process")
        .iter()
        .filter_map(|row| row["pid"].as_i64())
        .filter(|pid| *pid != 0)
        .collect();
    pids.sort_unstable();
    assert_eq!(pids, [7, 4242], "every process, not only the ones that ran");

    // Drawn: the marker is a zero-duration slice on each process's own track.
    let markers: Vec<(&str, i64)> = document
        .rows("slice")
        .iter()
        .filter_map(|row| Some((row["name"].as_str()?, row["dur"].as_i64()?)))
        .collect();
    assert_eq!(markers, [("Start Process", 0), ("Start Process", 0)]);

    // And each landed at the reading it was given, converted out of gcscope's clock domain
    // rather than dropped for want of a path out of it. The marks were 1000 ns and 2000 ns after
    // the run began, and this exporter's anchor puts the run's origin at the origin of trace
    // time, so those are the numbers the query engine reports.
    let stamps: Vec<i64> = document
        .rows("slice")
        .iter()
        .filter_map(|row| row["ts"].as_i64())
        .collect();
    assert_eq!(stamps, [1_000, 2_000]);
}

/// The order the tree reads in is the order gcscope met the processes, and it is stated rather
/// than left to the reader's heuristic. Ranks come from the wire view, per the note above.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn processes_are_ranked_in_the_order_they_were_met() {
    let Some(python) = harness_python_or_skip("perfetto process ranks") else {
        return;
    };
    let path = export(
        &[process_meta(4242), process_meta(7), process_meta(99)],
        "ranks",
    );

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let ranked: Vec<(i64, i64)> = document
        .packets()
        .iter()
        .filter_map(|packet| {
            let descriptor = &packet["track_descriptor"];
            Some((
                descriptor["process"]["pid"].as_i64()?,
                descriptor["sibling_order_rank"].as_i64()?,
            ))
        })
        .collect();
    assert_eq!(ranked, [(4242, 0), (7, 1), (99, 2)]);
}

// ---------------------------------------------------------------------------------
// 4. Collections
// ---------------------------------------------------------------------------------

fn thread_meta(pid: u32, tid: i64) -> TraceEvent {
    TraceEvent::ThreadMeta {
        pid,
        tid,
        name: format!("{pid}:{tid}"),
    }
}

fn begin(
    tid: i64,
    ts_ns: i64,
    name: &str,
    cat: &str,
    args: Vec<(&'static str, ArgValue)>,
) -> TraceEvent {
    TraceEvent::Begin {
        pid: 4242,
        tid,
        ts_ns,
        name: name.to_string(),
        cat: cat.to_string(),
        args,
    }
}

fn end(tid: i64, ts_ns: i64, name: &str, cat: &str) -> TraceEvent {
    TraceEvent::End {
        pid: 4242,
        tid,
        ts_ns,
        name: name.to_string(),
        cat: cat.to_string(),
    }
}

/// One Collection with a sub-phase inside it, as the conversion emits them.
fn a_collection() -> Vec<TraceEvent> {
    vec![
        process_meta(4242),
        thread_meta(4242, 0),
        begin(
            0,
            1_000,
            "GC Pause (gen=1)",
            "gc.pause(gen=1)",
            vec![
                ("generation", ArgValue::Int(1)),
                ("collected", ArgValue::Int(900)),
                ("duration", ArgValue::Float(0.00125)),
            ],
        ),
        begin(
            0,
            1_200,
            "Mark Alive (gen=1)",
            "gc.mark.alive(gen=1)",
            vec![("alive_size", ArgValue::Int(4096))],
        ),
        end(0, 1_800, "Mark Alive (gen=1)", "gc.mark.alive(gen=1)"),
        end(0, 3_000, "GC Pause (gen=1)", "gc.pause(gen=1)"),
    ]
}

/// The heart of this step, asserted through the engine the Perfetto UI runs: every slice the
/// events carried is in the trace with its name, its category, its start, its width and its
/// place in the nesting, and the sub-phase sits **inside** the Collection rather than beside it.
///
/// `depth` is what carries the nesting. A trace whose begins and ends paired wrongly would still
/// produce two slices with the right names and put them at depth 0 and 0, or 0 and 1 in the
/// wrong order — which is why the whole tuple is compared rather than the names alone.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn every_collection_and_sub_phase_becomes_a_slice_where_the_events_put_it() {
    let Some(python) = harness_python_or_skip("perfetto slices") else {
        return;
    };
    let path = export(&a_collection(), "slices");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let slices: Vec<(&str, &str, i64, i64, i64)> = document
        .rows("slice")
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap_or_default(),
                row["category"].as_str().unwrap_or("(none)"),
                row["ts"].as_i64().unwrap_or(-1),
                row["dur"].as_i64().unwrap_or(-1),
                row["depth"].as_i64().unwrap_or(-1),
            )
        })
        .collect();
    assert_eq!(
        slices,
        [
            ("GC Pause (gen=1)", "gc.pause(gen=1)", 1_000, 2_000, 0),
            ("Mark Alive (gen=1)", "gc.mark.alive(gen=1)", 1_200, 600, 1),
        ]
    );

    // Timestamps stay the nanoseconds CPython published. The Chrome exporter's microsecond
    // conversion is not reachable from here, and a trace that had been through it would report
    // this pause as 2 µs wide instead of 2 ms.
    let (_, _, ts, dur, _) = slices[0];
    assert_eq!((ts, dur), (1_000, 2_000), "nanoseconds, not microseconds");
}

/// Arguments arrive keyed as the conversion keyed them, and an integer and a float stay their
/// own kinds rather than both becoming strings. `duration` is a float of seconds; read back as a
/// string it cannot be plotted or summed, which is most of what a counter is for.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn arguments_reach_the_trace_keyed_and_typed_as_the_conversion_left_them() {
    let Some(python) = harness_python_or_skip("perfetto annotations") else {
        return;
    };
    let path = export(&a_collection(), "args");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let args: Vec<(&str, &str, Option<i64>, Option<f64>)> = document
        .rows("slice_args")
        .iter()
        .filter(|row| {
            row["key"]
                .as_str()
                .is_some_and(|key| key.starts_with("debug."))
        })
        .map(|row| {
            (
                row["key"].as_str().unwrap_or_default(),
                row["value_type"].as_str().unwrap_or_default(),
                row["int_value"].as_i64(),
                row["real_value"].as_f64(),
            )
        })
        .collect();
    // Ordered by slice and then by key, which is how the harness asks for them — so the pause's
    // three come first and the sub-phase's one after, rather than all four sorted together.
    assert_eq!(
        args,
        [
            ("debug.collected", "int", Some(900), None),
            ("debug.duration", "real", None, Some(0.00125)),
            ("debug.generation", "int", Some(1), None),
            ("debug.alive_size", "int", Some(4096), None),
        ],
        "the keys are the conversion's, the kinds survive the trip, and each slice keeps its own"
    );
}

/// Each interpreter has a track of its own under its process, named as the `ThreadMeta` event
/// names it, and a Collection lands on the track of the interpreter that ran it.
///
/// The track is **not** a thread: gcscope's `tid` is an interpreter id, so a `ThreadDescriptor`
/// would claim an OS thread of that number exists. That absence is asserted here against the
/// `thread` table, which stays empty of these ids.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn each_interpreter_has_its_own_track_under_its_process() {
    let Some(python) = harness_python_or_skip("perfetto interpreter tracks") else {
        return;
    };
    let mut events = a_collection();
    events.extend([
        thread_meta(4242, 3),
        begin(3, 5_000, "GC Pause (gen=0)", "gc.pause(gen=0)", vec![]),
        end(3, 6_000, "GC Pause (gen=0)", "gc.pause(gen=0)"),
    ]);
    let path = export(&events, "interpreters");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    // Which track each slice sits on, by the track's own name.
    let track_names: std::collections::HashMap<i64, &str> = document
        .rows("track")
        .iter()
        .filter_map(|row| Some((row["id"].as_i64()?, row["name"].as_str()?)))
        .collect();
    let placed: Vec<(&str, &str)> = document
        .rows("slice")
        .iter()
        .filter_map(|row| {
            let track = track_names.get(&row["track_id"].as_i64()?)?;
            Some((row["name"].as_str()?, *track))
        })
        .collect();
    assert_eq!(
        placed,
        [
            ("GC Pause (gen=1)", "4242:0"),
            ("Mark Alive (gen=1)", "4242:0"),
            ("GC Pause (gen=0)", "4242:3"),
        ],
        "each interpreter's Collections stay on its own row"
    );

    // And no OS thread was invented for either interpreter id.
    let tids: Vec<i64> = document
        .rows("thread")
        .iter()
        .filter_map(|row| row["tid"].as_i64())
        .collect();
    assert!(
        !tids.contains(&3),
        "interpreter 3 was described as an OS thread: {tids:?}"
    );
}

/// A sub-span can start before the pause enclosing it, and it is written in the order it
/// arrived rather than sorted into shape.
///
/// The build shape that produces it: one publishing a phase's stop but not its start chains that
/// phase onto the preceding phase's stop, and a field its layout *defines* but which reads zero
/// puts the sub-span at the epoch. Reordering on timestamps would repair the picture by
/// inventing a pause the target never ran, so the raw numbers stay and the trace shows what was
/// published. trace-processor reports the sub-slice's width as negative rather than refusing it.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn a_sub_span_that_starts_before_its_parent_keeps_the_numbers_the_target_published() {
    let Some(python) = harness_python_or_skip("perfetto emission order") else {
        return;
    };
    let events = vec![
        process_meta(4242),
        thread_meta(4242, 0),
        begin(0, 60_000, "GC Pause (gen=0)", "gc.pause(gen=0)", vec![]),
        begin(
            0,
            0,
            "Finalize Garbage (gen=0)",
            "gc.finalize(gen=0)",
            vec![],
        ),
        end(0, 65_000, "Finalize Garbage (gen=0)", "gc.finalize(gen=0)"),
        end(0, 70_000, "GC Pause (gen=0)", "gc.pause(gen=0)"),
    ];
    let path = export(&events, "emission-order");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(
        document.is_valid(),
        "the pairs still nest even where the timestamps do not:\n{}",
        document.problems()
    );

    let starts: Vec<(&str, i64)> = document
        .rows("slice")
        .iter()
        .filter_map(|row| Some((row["name"].as_str()?, row["ts"].as_i64()?)))
        .collect();
    assert_eq!(
        starts,
        [
            ("Finalize Garbage (gen=0)", 0),
            ("GC Pause (gen=0)", 60_000)
        ],
        "the query engine orders by timestamp; the file has them the other way round"
    );
}

// ---------------------------------------------------------------------------------
// 5. Counters
// ---------------------------------------------------------------------------------

fn counter(tid: i64, ts_ns: i64, name: &str, args: Vec<(&'static str, ArgValue)>) -> TraceEvent {
    TraceEvent::Counter {
        pid: 4242,
        tid,
        ts_ns,
        name: name.to_string(),
        args,
    }
}

/// What one Collection's counters look like, as the conversion emits them: the per-generation
/// series, and the heap as a series of one component carrying an empty name.
fn metrics(generation: u32, ts_ns: i64, collected: i64) -> Vec<TraceEvent> {
    vec![
        counter(
            0,
            ts_ns,
            &format!("G{generation}"),
            vec![
                ("collected", ArgValue::Int(collected)),
                ("candidates", ArgValue::Int(40)),
                ("duration", ArgValue::Float(0.00125)),
            ],
        ),
        counter(0, ts_ns, "", vec![("heap_size", ArgValue::Int(4096))]),
    ]
}

/// Every generation of one interpreter, each with its own metrics and the same heap.
fn three_generations() -> Vec<TraceEvent> {
    let mut events = vec![process_meta(4242), thread_meta(4242, 0)];
    for generation in 0..3u32 {
        events.extend(metrics(
            generation,
            1_000,
            900 - i64::from(generation) * 100,
        ));
    }
    events
}

/// Which track each track hangs off, by name, and what the query engine calls a counter.
///
/// `counter_track` carries a `parent_id` column of its own and it reads NULL for every row in
/// 0.57.2; the parenting is in `track`, whose ids are the same rows. So the group a counter sits
/// under is read there — the same track, not a different question asked of a column that happens
/// to have a value.
fn parents(document: &common::perfetto::Dump) -> std::collections::HashMap<String, String> {
    let by_id: std::collections::HashMap<i64, &str> = document
        .rows("track")
        .iter()
        .filter_map(|row| Some((row["id"].as_i64()?, row["name"].as_str()?)))
        .collect();
    document
        .rows("track")
        .iter()
        .filter_map(|row| {
            let name = row["name"].as_str()?.to_string();
            let parent = match row["parent_id"].as_i64() {
                Some(id) => (*by_id.get(&id)?).to_string(),
                // A track parented to a process descriptor is folded into the process by
                // trace-processor, which leaves this null rather than pointing at it.
                None => "(the process)".to_string(),
            };
            Some((name, parent))
        })
        .collect()
}

/// The operator's question from the ticket, asked of the query engine: are the three
/// generations' figures in one place, and is the heap where a whole-heap figure belongs.
///
/// Every counter track the events carried is here, with the samples that landed on it and the
/// heading it sits under. `heap_size` appears **once** — three generations published it and the
/// series they carried it on is unnamed, so it is one track rather than three — and it hangs
/// beside the group rather than inside it.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn every_counter_track_is_drawn_where_the_layout_puts_it() {
    let Some(python) = harness_python_or_skip("perfetto counter tracks") else {
        return;
    };
    let path = export(&three_generations(), "counters");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let tracks: Vec<&str> = document
        .rows("counter_track")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(
        tracks,
        [
            "G0 collected",
            "G0 candidates",
            "G0 duration",
            "heap_size",
            "G1 collected",
            "G1 candidates",
            "G1 duration",
            "G2 collected",
            "G2 candidates",
            "G2 duration",
        ],
        "one track per component of each series, and one heap between them"
    );

    let parents = parents(&document);
    for track in &tracks {
        let expected = if *track == "heap_size" {
            "4242:0"
        } else {
            "GC Metrics"
        };
        assert_eq!(
            parents.get(*track).map(String::as_str),
            Some(expected),
            "{track} hangs off the wrong track"
        );
    }
    // And the heading itself sits under the interpreter whose figures it holds.
    assert_eq!(
        parents.get("GC Metrics").map(String::as_str),
        Some("4242:0")
    );

    // The samples, by the track they landed on. `duration` is a float of seconds: written into
    // an integer counter it would read as 0 here, which is what every pause under a second
    // would come to.
    let by_id: std::collections::HashMap<i64, &str> = document
        .rows("counter_track")
        .iter()
        .filter_map(|row| Some((row["id"].as_i64()?, row["name"].as_str()?)))
        .collect();
    let mut samples: Vec<(&str, i64, f64)> = document
        .rows("counter")
        .iter()
        .filter_map(|row| {
            Some((
                *by_id.get(&row["track_id"].as_i64()?)?,
                row["ts"].as_i64()?,
                row["value"].as_f64()?,
            ))
        })
        .collect();
    samples.sort_by(|a, b| a.0.cmp(b.0));
    assert_eq!(
        samples,
        [
            ("G0 candidates", 1_000, 40.0),
            ("G0 collected", 1_000, 900.0),
            ("G0 duration", 1_000, 0.00125),
            ("G1 candidates", 1_000, 40.0),
            ("G1 collected", 1_000, 800.0),
            ("G1 duration", 1_000, 0.00125),
            ("G2 candidates", 1_000, 40.0),
            ("G2 collected", 1_000, 700.0),
            ("G2 duration", 1_000, 0.00125),
            ("heap_size", 1_000, 4096.0),
            ("heap_size", 1_000, 4096.0),
            ("heap_size", 1_000, 4096.0),
        ],
        "every sample on its own track, the heap sampled once per Record on one"
    );
}

/// The three generations' `collected` read against each other because they say so, and the
/// promoted metric says nothing — alone at its level it has nobody to share an axis with.
///
/// Asserted through the **wire view**, because `y_axis_share_key` is a rendering hint and
/// trace-processor exposes no column for it as of 0.57.2. The SQL sits beside it as an expected
/// failure that is not required to fail: the day the column arrives the values are checked too,
/// and until then reaching for some other column that happens to exist would assert something
/// else.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn the_generations_share_an_axis_and_the_promoted_metric_has_none() {
    let Some(python) = harness_python_or_skip("perfetto share keys") else {
        return;
    };
    let path = export(&three_generations(), "share-keys");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let keys: Vec<(&str, Option<&str>)> = document
        .packets()
        .iter()
        .filter_map(|packet| {
            let descriptor = &packet["track_descriptor"];
            let counter = descriptor.get("counter")?;
            Some((
                descriptor["name"].as_str()?,
                counter["y_axis_share_key"].as_str(),
            ))
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("G0 collected", Some("collected")),
            ("G0 candidates", Some("candidates")),
            ("G0 duration", Some("duration")),
            ("heap_size", None),
            ("G1 collected", Some("collected")),
            ("G1 candidates", Some("candidates")),
            ("G1 duration", Some("duration")),
            ("G2 collected", Some("collected")),
            ("G2 candidates", Some("candidates")),
            ("G2 duration", Some("duration")),
        ],
        "the share key is the metric, so one metric's tracks read against each other"
    );

    // The same claim asked of the query engine. An error is the answer this release gives.
    let sql = &document.document["trace_processor"]["counter_track_y_axis"];
    match sql.as_array() {
        Some(rows) => {
            let shared: Vec<(&str, Option<&str>)> = rows
                .iter()
                .filter_map(|row| Some((row["name"].as_str()?, row["y_axis_share_key"].as_str())))
                .collect();
            assert_eq!(shared.len(), keys.len());
            for (name, key) in shared {
                let expected = keys
                    .iter()
                    .find(|(track, _)| *track == name)
                    .map(|(_, k)| *k);
                assert_eq!(Some(key), expected, "{name} shares a different axis in SQL");
            }
        }
        None => eprintln!(
            "NOTE: trace-processor 0.57.2 has no `y_axis_share_key` column, so only the wire \
             view checked the keys. The query said: {sql}"
        ),
    }
}

/// The order the metric tracks read in is the one someone chose, and it is stated in the file.
///
/// From the wire view for the same reason the process ranks are: `sibling_order_rank` is not a
/// column. A rank counts only where its parent declares the ordering explicit, so the group and
/// the interpreter track above it both say so — and a rank written under a parent that does not
/// is inert, which is a thing this file has shipped once already.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn the_metric_tracks_carry_the_ranks_and_the_orderings_that_make_them_count() {
    let Some(python) = harness_python_or_skip("perfetto metric ranks") else {
        return;
    };
    let path = export(&three_generations(), "metric-ranks");

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let ranked: Vec<(&str, i64)> = document
        .packets()
        .iter()
        .filter_map(|packet| {
            let descriptor = &packet["track_descriptor"];
            Some((
                descriptor["name"].as_str()?,
                descriptor["sibling_order_rank"].as_i64()?,
            ))
        })
        .collect();
    assert_eq!(
        ranked,
        [
            ("python 4242", 0),
            ("4242:0", 0),
            ("GC Metrics", 1),
            ("G0 collected", 2),
            ("G0 candidates", 4),
            ("G0 duration", 5),
            ("heap_size", 0),
            ("G1 collected", 2),
            ("G1 candidates", 4),
            ("G1 duration", 5),
            ("G2 collected", 2),
            ("G2 candidates", 4),
            ("G2 duration", 5),
        ],
        "ranked by metric, so each generation's `collected` sits with the others'"
    );

    // Rendered as the enum's own spelling, so this confirms the value landed in a field typed to
    // take it. Both parents declare it: without the one on the interpreter track the group's own
    // rank is inert, and without the one on the group every metric rank above is.
    let explicit: Vec<&str> = document
        .packets()
        .iter()
        .filter_map(|packet| {
            let descriptor = &packet["track_descriptor"];
            descriptor.get("child_ordering")?;
            descriptor["name"].as_str()
        })
        .collect();
    assert_eq!(explicit, ["python 4242", "4242:0", "GC Metrics"]);
}

/// A build with no timing fields is counters or nothing
/// ([ADR 0017](../docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md)), so the tracks this
/// step draws are the whole of what such a target produces. It has no `heap_size` to promote and
/// no pause to draw, and the trace still holds a process, an interpreter and its figures.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn a_build_with_no_timing_still_draws_its_counter_tracks() {
    let Some(python) = harness_python_or_skip("perfetto untimed tier") else {
        return;
    };
    let events = vec![
        process_meta(4242),
        thread_meta(4242, 0),
        counter(
            0,
            5_000,
            "G0",
            vec![
                ("collections", ArgValue::Int(40)),
                ("collected", ArgValue::Int(900)),
            ],
        ),
    ];
    let path = export(&events, "untimed");

    let document = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let tracks: Vec<&str> = document
        .rows("counter_track")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert_eq!(tracks, ["G0 collections", "G0 collected"]);

    assert!(
        document.rows("slice").is_empty(),
        "a build with no timestamps published no pause, so nothing may draw one"
    );
    let pids: Vec<i64> = document
        .rows("process")
        .iter()
        .filter_map(|row| row["pid"].as_i64())
        .collect();
    assert!(pids.contains(&4242), "{pids:?}");
}

/// Two processes and a repeat, still one trace that holds together. The structural checks are
/// the point rather than the counts: a second descriptor reusing a uuid, or a packet losing its
/// sequence id, is caught here and by nothing in the crate.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn a_trace_of_several_processes_still_holds_together() {
    let Some(python) = harness_python_or_skip("perfetto exporter, several") else {
        return;
    };
    let path = export(
        &[process_meta(4242), process_meta(7), process_meta(4242)],
        "several",
    );

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let mut uuids: Vec<&str> = document
        .packets()
        .iter()
        .filter_map(|packet| packet["track_descriptor"]["uuid"].as_str())
        .collect();
    uuids.sort_unstable();
    assert_eq!(uuids, ["0", "1", "2"], "one track each, and the root");
}
