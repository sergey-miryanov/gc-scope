//! §4.4 — the two `PySession` lifetime paths no one-shot `gc-stats` reaches: the
//! layout-cache hit on re-attach, and the soft-reattach revalidation after a process dies.
//!
//! Both **attach** (read the target's memory), so unlike `tests/spawn.rs` they need
//! ptrace/taskport permission the plain `build` job doesn't grant. They are therefore
//! `#[ignore]`d — run them with `cargo test -- --ignored` on a box (or CI leg) where
//! attach is permitted. With no Python present they skip with a log instead of failing.

mod common;

use common::{pid_alive, test_python, SpawnedPython};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use gcscope::remote_debugging::session::{LayoutSource, PySession, Revalidated};

/// Both tests attach to the *same* interpreter binary, so they share the
/// process-wide `LAYOUT_CACHE`. Run concurrently — the default under `cargo test`
/// — one test's attach can re-parse and `insert`-overwrite the cached layout `Arc`
/// between the other's two attaches, so `layout_cache_hit_on_reattach`'s identity
/// check sees two different Arcs and fails (issue #7). The shared-Arc property only
/// holds absent a concurrent attack to the same binary, which is a precondition the
/// harness's parallelism violates — so serialize the attach-heavy tests here.
static ATTACH_SERIAL: Mutex<()> = Mutex::new(());

/// Take the serialization lock, tolerating poisoning: a panicking test still
/// releases the lock, and the guard's only job is mutual exclusion, not guarding data.
fn attach_serial() -> MutexGuard<'static, ()> {
    ATTACH_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Second attach to the same live binary reuses the process-wide layout cache instead of
/// re-parsing (ADR 0001, E1/E2). A one-shot `gc-stats` attaches once and never sees this.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn layout_cache_hit_on_reattach() {
    let _serial = attach_serial();
    let Some(python) = test_python() else {
        eprintln!("SKIP layout_cache_hit_on_reattach: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let s1 = PySession::attach(pid).expect("first attach");
    let s2 = PySession::attach(pid).expect("second attach");

    // The second attach to a still-live binary must be a cache hit — no re-parse. (The
    // first may itself be a hit if an earlier test warmed the cache for this binary; only
    // the second's status is guaranteed, which is exactly the property under test.)
    assert_eq!(
        s2.layout_source(),
        LayoutSource::Cached,
        "second attach should reuse the process-wide layout cache"
    );
    // ...and it must be the *same* layout: the cache hands back a clone of one
    // `Arc<Resolved>`, so both sessions share it; a re-parse would allocate a distinct Arc.
    assert!(
        Arc::ptr_eq(&s1.resolved_arc(), &s2.resolved_arc()),
        "both attaches must share the one cached layout (identical offsets)"
    );
}

/// After the attached process exits, `revalidate` must report a clean outcome — never
/// `Fresh`, which would mean it soft-reattached into a dead or reused address space and
/// would then read garbage.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn revalidate_is_clean_after_process_exits() {
    let _serial = attach_serial();
    let Some(python) = test_python() else {
        eprintln!("SKIP revalidate_is_clean_after_process_exits: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();
    let mut session = PySession::attach(pid).expect("attach while alive");

    drop(proc); // kill + reap the interpreter

    // Make the check deterministic: wait until the OS has actually torn the process down.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "spin.py should be gone after drop");

    // Dead PID → `Dead` (or `Changed` if it was reused by a different program). Never
    // `Fresh`: that is the garbage-read bug this test guards against.
    let out = session.revalidate();
    assert!(
        matches!(out, Revalidated::Dead | Revalidated::Changed),
        "a reaped process must revalidate cleanly (expected Dead), never a Fresh \
         soft-reattach into a dead/reused address space; got {out:?}"
    );
}
