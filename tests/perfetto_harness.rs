//! The Rust half of the Perfetto verification harness: the seam every later Perfetto test
//! asserts through, proven here against traces gcscope did not write.
//!
//! Nothing in this file knows what a Collection is. It checks that a document crosses the
//! process boundary intact and that both views answer, so a test about *gcscope's* trace can
//! be written about the trace instead of about subprocesses and parsing.
//!
//! `#[ignore]`d, per the live-matrix convention: the default `cargo test` stays green on a
//! machine with no `perfetto` package. Run with
//!   cargo test --test perfetto_harness -- --ignored
//! after `pip install -r tests/harness/requirements.txt`.

mod common;

use std::path::PathBuf;

use common::perfetto::{Dump, dump, dump_wire_only, harness_python_or_skip, keys, reference_trace};

fn trace_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "gcscope_harness_{}_{}.perfetto-trace",
        std::process::id(),
        tag
    ))
}

#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn the_wire_view_reaches_rust_with_the_packets_the_trace_carried() {
    let Some(python) = harness_python_or_skip("perfetto wire view") else {
        return;
    };
    let path = trace_path("reference");
    reference_trace(&python, "reference", &path);

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());
    assert_eq!(document.exit_code, 0);

    let packets = document.packets();
    assert_eq!(packets.len(), 9, "three descriptors and six events");

    // A track descriptor, whole: this is the assertion a text dump cannot support, since it
    // fails on a key nobody expected as well as on a value that changed.
    let first = &packets[0]["track_descriptor"];
    assert_eq!(
        keys(first),
        ["name", "process", "sibling_order_rank", "uuid"]
    );
    assert_eq!(first["process"]["pid"], 4242);
    assert_eq!(
        first["sibling_order_rank"], 0,
        "a rank written as zero is not the same as one nobody wrote"
    );
}

/// 64-bit fields arrive as strings, and the value here is one a double would move.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn a_nanosecond_timestamp_survives_the_trip_exactly() {
    let Some(python) = harness_python_or_skip("perfetto timestamps") else {
        return;
    };
    let path = trace_path("timestamps");
    reference_trace(&python, "reference", &path);

    let document = dump_wire_only(&python, &path);
    std::fs::remove_file(&path).ok();

    let stamps: Vec<i64> = document
        .packets()
        .iter()
        .filter_map(|p| p["timestamp"].as_str())
        .map(|s| s.parse().expect("a 64-bit field renders as a string"))
        .collect();

    assert!(stamps.contains(&1_690_000_000_000_000_037));
    assert_eq!(
        1_690_000_000_000_000_037_f64 as i64, 1_690_000_000_000_000_000,
        "the rounding the string rendering avoids"
    );
}

/// The half that makes the harness an oracle rather than a mirror: shown a broken trace, it
/// says so, names the check, and points at the packet.
#[test]
#[ignore = "needs the `perfetto` package — run with --ignored"]
fn a_trace_that_does_not_hold_together_comes_back_rejected() {
    let Some(python) = harness_python_or_skip("perfetto rejection") else {
        return;
    };

    for (kind, expected) in [
        ("unmatched-begin", "slice_pairing"),
        ("undeclared-track", "track_uuid"),
        ("no-sequence-id", "sequence_id"),
    ] {
        let path = trace_path(kind);
        reference_trace(&python, kind, &path);
        let document = dump_wire_only(&python, &path);
        std::fs::remove_file(&path).ok();

        assert!(!document.is_valid(), "{kind} was accepted");
        assert_eq!(document.exit_code, 1, "{kind}");

        let problems = document.document["validity"]["problems"]
            .as_array()
            .expect("problems are a list");
        let checks: Vec<&str> = problems
            .iter()
            .filter_map(|p| p["check"].as_str())
            .collect();
        assert_eq!(checks, [expected], "{kind}: {}", document.problems());
    }
}

/// The semantic view, over the same trace. Slower — it starts the query engine the Perfetto UI
/// runs, downloading it on first use — and it is the only half that says a wire-valid trace
/// *means* what it should.
#[test]
#[ignore = "needs the `perfetto` package and downloads the query engine — run with --ignored"]
fn the_query_engine_view_reaches_rust_too() {
    let Some(python) = harness_python_or_skip("perfetto trace processor") else {
        return;
    };
    let path = trace_path("engine");
    reference_trace(&python, "reference", &path);

    let document: Dump = dump(&python, &path);
    std::fs::remove_file(&path).ok();

    assert!(document.is_valid(), "{}", document.problems());

    let pids: Vec<i64> = document
        .rows("process")
        .iter()
        .filter_map(|row| row["pid"].as_i64())
        .collect();
    assert!(pids.contains(&4242), "{pids:?}");

    let mut names: Vec<&str> = document
        .rows("slice")
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Detail", "Work"]);

    // Nesting is a property only this view reports: the wire carries begins and ends, and the
    // engine is what turns them into a tree.
    let inner = document
        .rows("slice")
        .iter()
        .find(|row| row["name"] == "Detail")
        .expect("the inner slice");
    assert_eq!(inner["depth"], 1);
}
