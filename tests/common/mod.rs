//! Shared helpers for the live integration tests (see docs/adr/0005-testing-strategy.md).
//!
//! [`SpawnedPython`] is the RAII spawn guard: it launches the checked-in `spin.py`
//! fixture, blocks until its `READY <pid>` marker, and **kills the child on drop** so a
//! panicking or aborted test can never orphan an interpreter (blocker §3.3). It spawns
//! directly rather than through gcscope's `ProcessRunner`, keeping the harness independent
//! of the monitor loop.

// Shared across several test binaries (spawn, lifecycle); each compiles its own copy and
// uses a different subset, so some helpers look unused per binary.
#![allow(dead_code)]

use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long to wait for the fixture's `READY` marker before giving up.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// The fixture self-terminates after this many seconds — a backstop *under* the RAII kill,
/// so even a child that somehow escapes `Drop` dies on its own. Long enough that no test
/// races it.
const SPIN_LIFETIME_SECS: &str = "120";

/// Path to the checked-in fixture, resolved against the crate root.
fn spin_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("spin.py")
}

/// A Python interpreter to test against, or `None` if none is available — callers then
/// **skip with a log** rather than fail, since the unit `build` job installs no Python.
/// `GCSCOPE_TEST_PYTHON` overrides; otherwise the first of `python3`/`python` that runs.
pub fn test_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GCSCOPE_TEST_PYTHON") {
        let p = PathBuf::from(p);
        if runs(&p) {
            return Some(p);
        }
    }
    ["python3", "python"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| runs(p))
}

/// Best-effort `(major, minor)` of `python`, parsed from `python --version`
/// (e.g. `"Python 3.13.1"` → `(3, 13)`). `None` if it can't be determined.
///
/// Used to gate tests that touch machinery only present in newer interpreters —
/// e.g. the `_PyRuntime`/`"xdebugpy"` section that `find_runtime` needs exists
/// only from 3.13 on. 3.4+ prints the version to stdout; check stderr too for the
/// rare toolchain that still uses it.
pub fn python_version(python: &Path) -> Option<(u8, u8)> {
    let out = Command::new(python).arg("--version").output().ok()?;
    let text = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    let s = String::from_utf8_lossy(&text);
    let ver = s.split_whitespace().nth(1)?; // "Python" "3.13.1"
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Whether `python --version` runs and exits 0 (i.e. the interpreter is usable).
fn runs(python: &Path) -> bool {
    Command::new(python)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A running `spin.py` interpreter, killed on drop.
pub struct SpawnedPython {
    child: Child,
    pid: u32,
}

impl SpawnedPython {
    /// Spawn `spin.py` under `python`, blocking until it prints `READY <pid>`.
    ///
    /// The returned PID is the interpreter's *own* (from the marker, not `child.id()`), so
    /// it stays correct even if a launcher/shim sits in between. Errors if the fixture dies
    /// on startup or never reports `READY` within [`READY_TIMEOUT`].
    pub fn spawn(python: &Path) -> io::Result<Self> {
        let mut child = Command::new(python)
            .arg(spin_fixture())
            .arg(SPIN_LIFETIME_SECS)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        // Drain the fixture's stdout on a worker thread so the wait is bounded: an
        // interpreter that fails to start closes stdout (EOF → sender dropped) and a hung
        // one trips the recv timeout — neither blocks the test forever. spin.py writes
        // nothing to stdout after READY, so we can stop reading once we have it.
        let stdout = child.stdout.take().expect("stdout was piped");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(rest) = line.strip_prefix("READY ")
                    && let Ok(pid) = rest.trim().parse::<u32>()
                {
                    let _ = tx.send(pid);
                    return;
                }
            }
        });

        match rx.recv_timeout(READY_TIMEOUT) {
            Ok(pid) => Ok(SpawnedPython { child, pid }),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "spin.py exited or never reported READY",
                ))
            }
        }
    }

    /// The interpreter's own PID (from the `READY` marker) — the one a test attaches to.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the child is still running (has not exited on its own).
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the interpreter now and reap it, for a test that needs it dead mid-run (e.g.
    /// exercising the monitor's process-exit path). Idempotent with the kill-on-drop.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SpawnedPython {
    fn drop(&mut self) {
        // Kill-on-drop is the whole point: no orphaned interpreter, even on panic. Errors
        // are ignored — the child may already have exited (self-terminated or killed).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether a PID currently exists, via sysinfo — no ptrace/attach, so it works in the unit
/// `build` job where the live-smoke ptrace permission is not configured.
pub fn pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let spid = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), true);
    sys.process(spid).is_some()
}
