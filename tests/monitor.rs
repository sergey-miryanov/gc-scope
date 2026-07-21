//! Live coverage for the monitor poll loop's read-failure orchestration
//! (`MonitorContext::poll` in src/monitor.rs) — the one path with no automated
//! coverage until now: a `gc_stats` read fails, `revalidate` runs, and poll either
//! retries (on `Fresh`) or gives up.
//!
//! Reproducing a *transient* read failure on a genuinely live process needs a
//! fault seam: `PySession::inject_gc_stats_faults` arms the next N `gc_stats`
//! calls to fail, and `MonitorContext::insert_session_for_test` installs the
//! armed session so `poll` uses it instead of attaching a fresh one. The process
//! stays alive throughout, so `revalidate` genuinely returns `Fresh` — we're
//! testing the loop's control flow, not mocking the session.
//!
//! The seam is gated behind the `test-hooks` cargo feature, so this whole target
//! only builds with it enabled. It also attaches to and reads a live process, so —
//! like tests/lifecycle.rs — it needs ptrace/taskport and is `#[ignore]`d. Run with:
//!   cargo test --features test-hooks --test monitor -- --ignored

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{pid_alive, test_python, SpawnedPython};

use gcscope::monitor::exporters::{EventsExporter, ProcessLifecycle};
use gcscope::monitor::{run_loop, MonitorContext, PollStatus, StartupTimeoutPolicy};
use gcscope::remote_debugging::gc_stats::GcStat;
use gcscope::remote_debugging::session::PySession;

/// Counts the exporter callbacks so a test can assert what the poll emitted:
/// GC events, and the process Started/Died lifecycle marks.
#[derive(Default)]
struct RecordingExporter {
    events: usize,
    started: usize,
    died: usize,
}

impl EventsExporter for RecordingExporter {
    fn open(&mut self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn add_event(&mut self, _pid: u32, _event: &GcStat) {
        self.events += 1;
    }
    fn mark_process_lifecycle(&mut self, _pid: u32, kind: ProcessLifecycle, _ts_ns: i64) {
        match kind {
            ProcessLifecycle::Started => self.started += 1,
            ProcessLifecycle::Died => self.died += 1,
        }
    }
    fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Attach a live `spin.py` and arm its next `n` `gc_stats` reads to fail, then run
/// one `poll` through a fresh `MonitorContext`. Returns the poll status alongside
/// the exporter so the caller can assert both. `None` if no Python is available.
fn poll_once_with_faults(n: u32) -> Option<(PollStatus, RecordingExporter)> {
    let python = test_python()?;
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let session = PySession::attach(pid).expect("attach to the live interpreter");
    session.inject_gc_stats_faults(n);

    let mut exporter = RecordingExporter::default();
    let status = {
        let mut ctx = MonitorContext::new(&mut exporter);
        ctx.insert_session_for_test(pid, session);
        ctx.poll(pid)
    };
    Some((status, exporter))
}

/// Baseline: a healthy live process polls `Ok` on the first read and is reported
/// Started exactly once. Establishes that the harness itself works before the
/// fault cases lean on it.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn poll_reports_a_healthy_process_started() {
    let Some((status, exporter)) = poll_once_with_faults(0) else {
        eprintln!("SKIP poll_reports_a_healthy_process_started: no Python found");
        return;
    };
    assert_eq!(status, PollStatus::Ok, "a healthy live process must poll Ok");
    assert_eq!(exporter.started, 1, "the process should be reported Started once");
    assert_eq!(exporter.died, 0, "a live process must not be reported Died");
}

/// The recovery path: the first `gc_stats` fails, but the process is alive, so
/// `revalidate` returns `Fresh` and `poll` retries the read successfully — ending
/// `Ok` with the process Started and never Died. This is the orchestration that
/// had no coverage, and the one the `revalidate` empty-cmdline fix protects: a
/// transient read glitch must not drop a live session.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn poll_recovers_via_fresh_retry_after_a_transient_read_failure() {
    let Some((status, exporter)) = poll_once_with_faults(1) else {
        eprintln!("SKIP poll_recovers_via_fresh_retry_after_a_transient_read_failure: no Python found");
        return;
    };
    assert_eq!(
        status,
        PollStatus::Ok,
        "a live process whose first read glitched must recover via the Fresh retry"
    );
    assert_eq!(exporter.started, 1, "recovery still reports Started once");
    assert_eq!(exporter.died, 0, "a recovered process must not be reported Died");
}

/// The give-up path: both the first read and the post-`Fresh` retry fail, so poll
/// returns `InvalidProcess`. Because the process was never successfully read, it
/// was never marked Started, so no `Died` is emitted either — poll just hands the
/// retry-vs-give-up decision back to the caller's `WaitPolicy`.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn poll_gives_up_when_the_retry_also_fails() {
    let Some((status, exporter)) = poll_once_with_faults(2) else {
        eprintln!("SKIP poll_gives_up_when_the_retry_also_fails: no Python found");
        return;
    };
    assert_eq!(
        status,
        PollStatus::InvalidProcess,
        "if the read fails even after a Fresh re-attach, poll must give up"
    );
    assert_eq!(exporter.started, 0, "a never-read process must not be reported Started");
    assert_eq!(exporter.died, 0, "nothing to mark Died for a process never reported alive");
}

/// `mark_died` is the single eviction point (C7): after a process has been reported
/// Started, marking it died must emit `Died` exactly once and drop its per-PID state, and
/// a second `mark_died` on the same PID must be a silent no-op (nothing left to report).
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn mark_died_emits_died_once_and_is_idempotent() {
    let Some(python) = test_python() else {
        eprintln!("SKIP mark_died_emits_died_once_and_is_idempotent: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();
    let session = PySession::attach(pid).expect("attach to the live interpreter");

    let mut exporter = RecordingExporter::default();
    {
        let mut ctx = MonitorContext::new(&mut exporter);
        ctx.insert_session_for_test(pid, session);
        assert_eq!(ctx.poll(pid), PollStatus::Ok, "healthy first poll reports Started");
        ctx.mark_died(pid);
        // Already evicted from the alive set — a repeat must not double-report.
        ctx.mark_died(pid);
    }
    assert_eq!(exporter.started, 1);
    assert_eq!(exporter.died, 1, "mark_died must emit Died exactly once");
}

/// The process-exit path: a PID reported Started that then exits. The next `gc_stats` read
/// fails, `revalidate` sees a dead/absent process, and `poll` gives up with
/// `InvalidProcess` — emitting `Died` once because the process had been alive. (Which
/// internal failure branch runs — Dead, or a Fresh retry that also fails — is irrelevant;
/// the observable contract is InvalidProcess + a single Died.)
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn poll_reports_invalid_and_died_when_the_process_exits_after_being_seen() {
    let Some(python) = test_python() else {
        eprintln!("SKIP poll_reports_invalid_and_died_when_the_process_exits_after_being_seen: no Python found");
        return;
    };
    let mut proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();
    let session = PySession::attach(pid).expect("attach to the live interpreter");

    let mut exporter = RecordingExporter::default();
    {
        let mut ctx = MonitorContext::new(&mut exporter);
        ctx.insert_session_for_test(pid, session);
        assert_eq!(ctx.poll(pid), PollStatus::Ok, "healthy first poll");

        // Kill the interpreter and wait until the OS has really reaped it, so the second
        // read genuinely fails against a gone process rather than racing a live one.
        proc.kill();
        for _ in 0..100 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "the interpreter must be gone before the second poll");

        assert_eq!(
            ctx.poll(pid),
            PollStatus::InvalidProcess,
            "a process that has exited must poll InvalidProcess"
        );
    }
    assert_eq!(exporter.started, 1);
    assert_eq!(exporter.died, 1, "an exited-after-Started process must be reported Died once");
}

/// End-to-end for `run_loop`: it discovers and polls a live process, reports it Started,
/// and — once `running` is cleared — breaks out and marks the still-tracked PID Died in the
/// teardown pass. Unlike the TUI loop this needs no terminal, so it runs headless. A stopper
/// thread flips `running` after the loop has had time to attach and poll a few times.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn run_loop_tracks_a_live_process_and_marks_it_died_on_stop() {
    let Some(python) = test_python() else {
        eprintln!("SKIP run_loop_tracks_a_live_process_and_marks_it_died_on_stop: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let running = AtomicBool::new(true);
    let mut exporter = RecordingExporter::default();
    {
        let mut ctx = MonitorContext::new(&mut exporter);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                running.store(false, Ordering::SeqCst);
            });
            run_loop(&mut ctx, pid, 50, &running, || {
                StartupTimeoutPolicy::new(Duration::from_secs(2))
            })
            .expect("run_loop should return Ok");
        });
    }
    // `proc` is still alive here, so the Died comes from the loop's teardown pass, not a
    // process exit.
    assert!(exporter.started >= 1, "run_loop must report the live process Started");
    assert!(exporter.died >= 1, "loop teardown must mark the tracked process Died");
}
