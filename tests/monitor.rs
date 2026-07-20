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

use common::{test_python, SpawnedPython};

use gcscope::exporters::{EventsExporter, ProcessLifecycle};
use gcscope::monitor::MonitorContext;
use gcscope::monitor_loop::PollStatus;
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
