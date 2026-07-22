//! §4.2 — the `SpawnedPython` spawn/kill guard.
//!
//! These are *live* tests: they need a Python interpreter but **not** attach permission
//! (no ptrace), so they run under plain `cargo test` in the unit `build` CI job. Attach +
//! decode is covered separately by the live-smoke matrix (tests/live_smoke.rs). With no
//! Python present they skip with a log rather than fail.

mod common;

use common::{pid_alive, test_python, SpawnedPython};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn spawn_reports_ready_pid() {
    let Some(python) = test_python() else {
        eprintln!("SKIP spawn_reports_ready_pid: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let mut proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    assert!(proc.pid() > 0, "READY must report a real PID");
    assert!(
        proc.is_running(),
        "spin.py should still be running right after READY"
    );
    assert!(
        pid_alive(proc.pid()),
        "the reported PID {} should exist on the system",
        proc.pid()
    );
}

#[test]
fn kills_on_drop() {
    let Some(python) = test_python() else {
        eprintln!("SKIP kills_on_drop: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let pid = {
        let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
        proc.pid()
    }; // dropped here → kill-on-drop must reap the interpreter

    // The OS may take a moment to tear the process down after wait(); poll briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pid_alive(pid),
        "SpawnedPython must kill spin.py on drop (pid {pid} still alive)"
    );
}
