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

use std::io::{self, BufRead, BufReader, Read};
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

/// Path to a checked-in fixture by filename, resolved against the crate root.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
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
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let s = String::from_utf8_lossy(&text);
    let ver = s.split_whitespace().nth(1)?; // "Python" "3.13.1"
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The interpreter's **full** self-reported version as gcscope encodes it:
/// `(major, minor, micro, release_level, serial)`, with the level as the hex nibble
/// `PY_VERSION_HEX` uses (`0xA` alpha, `0xB` beta, `0xC` candidate, `0xF` final).
///
/// `python_version` stops at `(major, minor)` because its callers gate on the minor. The
/// string scan decides micro, level and serial too, and a scan that got the micro wrong
/// would sail through a `(major, minor)` check.
pub fn full_python_version(python: &Path) -> Option<(u8, u8, u8, u8, u8)> {
    let out = Command::new(python)
        .args([
            "-c",
            "import sys;v=sys.version_info;print(v[0],v[1],v[2],v[3],v[4])",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut f = s.split_whitespace();
    let major = f.next()?.parse().ok()?;
    let minor = f.next()?.parse().ok()?;
    let micro = f.next()?.parse().ok()?;
    let level = match f.next()? {
        "alpha" => 0xA,
        "beta" => 0xB,
        "candidate" => 0xC,
        "final" => 0xF,
        _ => return None,
    };
    let serial = f.next()?.parse().ok()?;
    Some((major, minor, micro, level, serial))
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

/// A Python with the Probe **extension** installed, or `None`, in which case callers **skip
/// with a log**: a checkout where nobody ran `pip install ./gcscope_probe` has no Probe to
/// read. Selected like [`test_python`], then filtered.
///
/// A plain `import gcscope_probe` answers the wrong question here: it passed on a runner with
/// no Probe at all, so the test ran instead of skipping. Two guards close that.
///
/// - `-P` keeps the cwd off `sys.path` (3.11+, and the Probe needs 3.13+), so the source
///   directory `gcscope_probe/` in the crate root cannot import as a namespace package and
///   pass on a machine that never built anything. `PYTHONPATH` still applies, which `-I`
///   would have dropped.
/// - Calling `geometry()` proves this is the extension. A namespace package imports fine and
///   carries no attributes, and the `AttributeError` surfaces later in the spawned fixture,
///   where it reads as a timeout.
pub fn probe_python() -> Option<PathBuf> {
    let python = test_python()?;
    let ok = Command::new(&python)
        .args(["-P", "-c", "import gcscope_probe; gcscope_probe.geometry()"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(python)
}

/// The file `python` would load for `import gcscope_probe`, or `None` if it has no Probe.
///
/// This asks the interpreter rather than searching the filesystem, so it resolves the same
/// `sys.path` the fixture will and inspects the file that gets mapped. `-P` for the reason
/// [`probe_python`] gives: without it the source directory in the crate root answers, and that
/// is not a built module.
pub fn probe_module_path(python: &Path) -> Option<PathBuf> {
    let out = Command::new(python)
        .args([
            "-P",
            "-c",
            "import gcscope_probe; print(gcscope_probe.__file__)",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    path.is_file().then_some(path)
}

/// Whether a missing Probe must **fail** rather than skip. Skipping is right on a laptop and
/// wrong in CI, where it would turn a Probe that failed to compile into a green leg, so the
/// leg that builds one sets `GCSCOPE_REQUIRE_PROBE=1`.
pub fn probe_required() -> bool {
    std::env::var("GCSCOPE_REQUIRE_PROBE").ok().as_deref() == Some("1")
}

/// True for a free-threaded (no-GIL) build, whose GC ring holds one entry per generation
/// instead of the GIL build's 11/3/3 — the live-smoke shape check needs this to pick the
/// expected entry counts. `false` if it can't be determined (the common GIL case).
pub fn is_free_threaded(python: &Path) -> bool {
    Command::new(python)
        .args([
            "-c",
            "import sysconfig; print(sysconfig.get_config_var('Py_GIL_DISABLED') or 0)",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
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
        Self::spawn_fixture(python, "spin.py")
    }

    /// As [`Self::spawn`], for a fixture other than `spin.py`: today `probe_spin.py`, which
    /// installs the Probe before collecting. Both take the same lifetime argument and print
    /// the same `READY <pid>` marker, so they share the wait.
    pub fn spawn_fixture(python: &Path, fixture_name: &str) -> io::Result<Self> {
        Self::spawn_fixture_env(python, fixture_name, &[])
    }

    /// As [`Self::spawn_fixture`], with environment variables set on the child.
    ///
    /// The fault hooks a fixture reads are environment-driven, since a fixture takes one
    /// positional argument and both fixtures have to keep taking the same one.
    pub fn spawn_fixture_env(
        python: &Path,
        fixture_name: &str,
        env: &[(&str, &str)],
    ) -> io::Result<Self> {
        let mut cmd = Command::new(python);
        cmd.arg(fixture(fixture_name)).arg(SPIN_LIFETIME_SECS);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

        // Capture stderr rather than discarding it. A fixture that dies on startup, usually on
        // a failed import, otherwise surfaces only as "never reported READY", which reports
        // the symptom and hides the traceback naming the cause.
        let stderr = child.stderr.take().expect("stderr was piped");
        let err_h = thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut s);
            s
        });

        // Drain the fixture's stdout on a worker thread so the wait is bounded: an
        // interpreter that fails to start closes stdout (EOF → sender dropped) and a hung
        // one trips the recv timeout — neither blocks the test forever. The fixtures write
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
                // Kill first: that closes the stderr pipe, so the drain thread reaches EOF and
                // the join cannot hang on a child that is merely stuck rather than dead.
                let _ = child.kill();
                let _ = child.wait();
                let stderr = err_h.join().unwrap_or_default();
                let tail: Vec<&str> = stderr.lines().rev().take(12).collect();
                let detail = if tail.is_empty() {
                    " (it wrote nothing to stderr)".to_string()
                } else {
                    let mut lines = tail;
                    lines.reverse();
                    format!("\n----- {fixture_name} stderr -----\n{}", lines.join("\n"))
                };
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{fixture_name} exited or never reported READY{detail}"),
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
