//! Live coverage for the diagram collect→render pipeline. The pure pieces
//! (`parse_gc_slots`, the rate/avg summaries, the tree model, the Legacy ASCII
//! render) are unit-tested in-crate; this exercises `collect_data` against a real
//! interpreter and renders the **3.13+ (Full) tier** the synthetic Legacy unit test
//! can't reach.
//!
//! Attaches to a live process (ptrace/taskport), so it is `#[ignore]`d like
//! tests/lifecycle.rs — run with `cargo test -- --ignored` where attach is permitted.

mod common;

use common::{pid_alive, python_version, test_python, SpawnedPython};

use gcscope::remote_debugging::collect::{
    self, avg_collection_time_per_gen, collections_rate_from_slots, CollectRequest,
};
use gcscope::diagram::ascii;
use gcscope::remote_debugging::poller::SnapshotPoller;
use gcscope::remote_debugging::session::PySession;

/// `collect_data` gathers a coherent snapshot from a live interpreter, and
/// `render_ascii` turns it into a diagram without panicking. On 3.13+ the snapshot
/// carries the `_Py_DebugOffsets` struct, so the Full-tier render path runs and the
/// header names it (vs. the pre-3.13 "no _Py_DebugOffsets" note).
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn collect_and_render_ascii_on_a_live_interpreter() {
    let Some(python) = test_python() else {
        eprintln!("SKIP collect_and_render_ascii_on_a_live_interpreter: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let session = PySession::attach(pid).expect("attach to the live interpreter");
    let data = collect::collect_data(&session, &CollectRequest::all())
        .expect("collect a snapshot from a live interpreter");

    assert_eq!(data.pid, pid);
    assert_ne!(data.runtime_addr, 0, "_PyRuntime address must be non-zero");
    assert_ne!(data.interpreter.addr, 0, "interpreter head address must be non-zero");

    let stats = &data.interpreter.gc.generation_stats;
    let rate = collections_rate_from_slots(&stats.slots, stats.has_timestamps);
    let avg = avg_collection_time_per_gen(&stats.slots, stats.has_duration);
    let out = ascii::render_ascii(&data, rate, avg);

    assert!(out.contains(&format!("PID {pid}")), "header must name the PID:\n{out}");
    // The frame stays within its fixed width even on the wider Full-tier panels.
    assert!(
        out.lines().all(|line| line.chars().count() <= 200),
        "a rendered line blew past the frame width"
    );

    if python_version(&python).is_some_and(|v| v >= (3, 13)) {
        assert!(data.offsets().is_some(), "3.13+ must carry _Py_DebugOffsets");
        assert!(
            out.contains("_Py_DebugOffsets"),
            "the 3.13+ header must name _Py_DebugOffsets:\n{out}"
        );
    }
}

/// `SnapshotPoller` is the single-PID producer both the TUI and `ascii --watch` sit on. It
/// polls a healthy interpreter, and once that interpreter exits its next `poll` runs the
/// revalidate ladder's `Dead` arm and surfaces a contextualized error — the resilience the
/// snapshot loops inherit from the monitor. Exercised here without a terminal, which the
/// interactive TUI loop can't offer.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn poller_errors_after_the_target_process_exits() {
    use std::time::Duration;

    let Some(python) = test_python() else {
        eprintln!("SKIP poller_errors_after_the_target_process_exits: no Python found");
        return;
    };
    let mut proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let mut poller = SnapshotPoller::attach(pid).expect("attach to the live interpreter");
    poller.poll().expect("a healthy interpreter must poll Ok");

    // Kill the interpreter and wait until the OS has really reaped it, so the next poll
    // genuinely fails against a gone process rather than racing a live one.
    proc.kill();
    for _ in 0..100 {
        if !pid_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!pid_alive(pid), "the interpreter must be gone before the next poll");

    let err = poller
        .poll()
        .expect_err("a poll against an exited process must surface an error");
    assert!(
        err.to_string().contains("gone"),
        "the Dead-arm error must name the gone process: {err}"
    );
}

/// A `CollectRequest` narrows which heavy layers `collect_data` reads. A `gc_stats_only`
/// request must leave the struct-dump buffers empty (debug-offsets, gc sub-struct) while
/// still decoding the GC generation stats; an `all` request over the same live interpreter
/// fills every layer. This proves the layers are independently skippable end-to-end, not
/// just in the synthetic unit tests.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn collect_request_skips_only_the_unrequested_layers() {
    let Some(python) = test_python() else {
        eprintln!("SKIP collect_request_skips_only_the_unrequested_layers: no Python found");
        return;
    };
    // Only 3.13+ carries the `_Py_DebugOffsets` dump; on older builds that layer is always
    // empty regardless of the request, so the "all fills it" half wouldn't be meaningful.
    if !python_version(&python).is_some_and(|v| v >= (3, 13)) {
        eprintln!("SKIP collect_request_skips_only_the_unrequested_layers: needs Python 3.13+");
        return;
    }
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let session = PySession::attach(proc.pid()).expect("attach to the live interpreter");

    let lean = collect::collect_data(&session, &CollectRequest::gc_stats_only())
        .expect("a gc_stats_only snapshot");
    assert!(lean.runtime_raw_bytes.is_empty(), "debug-offsets layer must be skipped");
    assert!(
        lean.interpreter.gc.raw_bytes.is_empty(),
        "gc sub-struct layer must be skipped"
    );
    assert!(
        !lean.interpreter.gc.generation_stats.slots.is_empty(),
        "the requested gc-stats layer must still be decoded"
    );

    let full = collect::collect_data(&session, &CollectRequest::all())
        .expect("an all-layers snapshot");
    assert!(!full.runtime_raw_bytes.is_empty(), "3.13+ all must fill the debug-offsets layer");
    assert!(
        !full.interpreter.gc.raw_bytes.is_empty(),
        "all must fill the gc sub-struct layer"
    );
    assert!(!full.interpreter.gc.generation_stats.slots.is_empty());
}

/// The TUI body's **Full-tier** section builders (`section_debug_offsets`,
/// `section_interpreter`) only run against a real 3.13+ `_Py_DebugOffsets` struct, so the
/// synthetic-Legacy unit tests in `tui_v2.rs` can't reach them. This drives them over a
/// live snapshot through the `test-hooks` seam and checks the frame is coherent across the
/// tree/hex toggles. Gated on `test-hooks`, matching the CI integration-coverage leg.
#[cfg(feature = "test-hooks")]
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn tui_frame_renders_the_full_tier_sections_on_a_live_interpreter() {
    use gcscope::diagram::tui_v2::render_frame_for_test;

    let Some(python) = test_python() else {
        eprintln!("SKIP tui_frame_renders_the_full_tier_sections_on_a_live_interpreter: no Python found");
        return;
    };
    let is_3_13_plus = python_version(&python).is_some_and(|v| v >= (3, 13));
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let session = PySession::attach(proc.pid()).expect("attach to the live interpreter");
    let data = collect::collect_data(&session, &CollectRequest::all())
        .expect("collect a snapshot from a live interpreter");

    // Exercise the toggle combinations the loop can drive: full tree+hex, both collapsed,
    // and the runtime-hex view. None may panic and all must produce a non-empty frame.
    for (tree, hex, rt_hex) in [(true, true, false), (false, false, false), (true, true, true)] {
        let lines = render_frame_for_test(&data, 0, tree, hex, rt_hex);
        assert!(!lines.is_empty(), "frame must have content for ({tree},{hex},{rt_hex})");
        let out = lines.join("\n");
        assert!(out.contains("GC Generation Stats"), "GC section must render:\n{out}");
        if is_3_13_plus {
            assert!(
                out.contains("_Py_DebugOffsets"),
                "3.13+ frame must render the _Py_DebugOffsets section for ({tree},{hex},{rt_hex}):\n{out}"
            );
        }
    }
}
