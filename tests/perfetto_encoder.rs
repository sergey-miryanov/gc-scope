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
use gcscope::monitor::exporters::EventsExporter;
use gcscope::monitor::exporters::perfetto::PerfettoExporter;
use gcscope::monitor::exporters::perfetto_proto::{
    ENUM_VALUES, FIELDS, trace, trace_packet, track_descriptor, track_event, track_event_type,
};
use gcscope::monitor::exporters::protobuf::{
    put_double, put_int32, put_int64, put_message, put_string, put_uint64,
};
use gcscope::monitor::trace_event::TraceEvent;

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

fn export(events: &[TraceEvent], tag: &str) -> PathBuf {
    let path = trace_path(tag);
    let mut exporter = PerfettoExporter::new();
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
    assert_eq!(packets.len(), 2, "the root descriptor and one process");

    // The root: uuid 0 and the two ordering hints, and no name, process or counter — a
    // descriptor with any of those becomes a track row in the UI, which this must not.
    let root = &packets[0]["track_descriptor"];
    assert_eq!(keys(root), ["process_ordering", "thread_ordering", "uuid"]);
    assert_eq!(
        root["uuid"], "0",
        "uuid is 64-bit, so it arrives as a string"
    );
    assert_eq!(root["process_ordering"], "PROCESS_ORDERING_EXPLICIT");
    assert_eq!(root["thread_ordering"], "THREAD_ORDERING_EXPLICIT");

    // The process track. Its name and its ordering rank are a later step's; what this asserts
    // is that the descriptor exists, sits on a uuid the root does not hold, and names the pid.
    let process = &packets[1]["track_descriptor"];
    assert_eq!(keys(process), ["process", "uuid"]);
    assert_ne!(process["uuid"], "0");
    assert_eq!(process["process"]["pid"], 4242);

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
