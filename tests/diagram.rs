//! Live coverage for the diagram collect→render pipeline. The pure pieces
//! (`parse_gc_slots`, the rate/avg summaries, the tree model, the Legacy ASCII
//! render) are unit-tested in-crate; this exercises `collect_data` against a real
//! interpreter and renders the **3.13+ (Full) tier** the synthetic Legacy unit test
//! can't reach.
//!
//! Attaches to a live process (ptrace/taskport), so it is `#[ignore]`d like
//! tests/lifecycle.rs — run with `cargo test -- --ignored` where attach is permitted.

mod common;

use common::{python_version, test_python, SpawnedPython};

use gcscope::diagram::collect::{
    self, avg_collection_time_per_gen, collections_rate_from_slots,
};
use gcscope::diagram::ascii;
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
    let data = collect::collect_data(&session).expect("collect a snapshot from a live interpreter");

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
