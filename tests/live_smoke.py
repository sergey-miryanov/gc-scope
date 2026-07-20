"""Live smoke driver: spawn a real interpreter, attach with gcscope, assert stats.

Runs the end-to-end pipeline (find `_PyRuntime` -> detect version -> read offsets
-> decode GC generation stats) against a live process and fails loudly if any
stage breaks. Used by the `live-smoke` CI job and runnable by hand on any OS:

    python tests/live_smoke.py                       # uses this interpreter
    python tests/live_smoke.py --python C:/py38/python.exe --label py3.8

This is deliberately Python rather than shell: the assertions must run
identically on Linux, macOS and Windows/PowerShell, and `setup-python` has
already put an interpreter on PATH in CI. It drives the CLI only — the
lifecycle tests (layout-cache hit, soft-reattach) need in-process access to
`PySession` and belong in the Rust harness (docs/tests-harness-plan.md §4.4).

Exit code 0 = PASS, 1 = FAIL (with the reason and the captured output).
"""

import argparse
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
SPIN = os.path.join(HERE, "fixtures", "spin.py")

# spin.py's own lifetime cap. Comfortably longer than a smoke run, short enough
# that a hard-killed driver can't leave an interpreter running for long.
SPIN_LIFETIME_SECS = 120
READY_TIMEOUT_SECS = 20
CMD_TIMEOUT_SECS = 60


def default_gcscope():
    """The debug binary cargo just built, with the platform's exe suffix."""
    exe = "gcscope.exe" if os.name == "nt" else "gcscope"
    return os.path.join(REPO, "target", "debug", exe)


def run(argv, timeout=CMD_TIMEOUT_SECS):
    """Run a command, capturing merged output as text. Returns (rc, output)."""
    try:
        proc = subprocess.run(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            # Decode explicitly: the default locale encoding chokes on gcscope's
            # box-drawing output under a non-UTF-8 Windows code page.
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        return proc.returncode, proc.stdout or ""
    except subprocess.TimeoutExpired:
        # A hang is a real failure mode for an attach tool (bad pointer walk on
        # an unknown layout), so surface it as one rather than letting CI stall.
        return 124, "TIMEOUT after %ds: %s" % (timeout, " ".join(argv))


def wait_for_ready(proc, log_path):
    """Poll spin.py's log for its READY marker; return the interpreter's PID.

    Waiting on the marker instead of sleeping removes both the slow-runner race
    and the "attached before the first gen-2 collection" flake. The PID comes
    from the marker rather than proc.pid so it stays correct if a launcher or
    shim ever sits between us and the real interpreter.
    """
    deadline = time.monotonic() + READY_TIMEOUT_SECS
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return None  # fixture died on startup; caller prints the log
        try:
            with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    if line.startswith("READY "):
                        return int(line.split()[1])
        except IOError:
            pass  # not created yet
        time.sleep(0.25)
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gcscope", default=default_gcscope(),
                        help="path to the gcscope binary (default: target/debug)")
    parser.add_argument("--python", default=sys.executable,
                        help="interpreter to attach to (default: this one)")
    parser.add_argument("--label", default=None,
                        help="tag for PASS/FAIL lines (default: the target's version)")
    parser.add_argument("--tmpdir", default=os.environ.get("RUNNER_TEMP") or
                        os.path.join(REPO, ".temp"),
                        help="scratch dir for the fixture log")
    args = parser.parse_args()

    if not os.path.exists(args.gcscope):
        print("FAIL: gcscope binary not found at %s (run `cargo build` first)"
              % args.gcscope)
        return 1

    rc, ver = run([args.python, "--version"])
    if rc != 0:
        print("FAIL: target interpreter is not runnable: %s" % ver.strip())
        return 1
    ver = ver.strip()
    label = args.label or ver.replace("Python ", "py")
    print("target: %s (%s)" % (ver, args.python))

    def fail(reason, detail=""):
        print("FAIL(%s): %s" % (label, reason))
        if detail:
            print(detail)
        return 1

    if not os.path.isdir(args.tmpdir):
        os.makedirs(args.tmpdir)
    log_path = os.path.join(args.tmpdir, "spin-%s.log" % label)

    log = open(log_path, "w+", encoding="utf-8")
    proc = subprocess.Popen(
        [args.python, SPIN, str(SPIN_LIFETIME_SECS)],
        stdout=log, stderr=subprocess.STDOUT,
    )
    try:
        pid = wait_for_ready(proc, log_path)
        if pid is None:
            log.flush()
            with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
                return fail("fixture never reported READY", fh.read())
        print("spin.py ready as pid %d" % pid)

        # find-runtime goes through the same version-aware attach path as
        # gc-stats, so failing here localizes the problem to *finding* (e.g. the
        # pre-3.13 symbol lookup) rather than decoding.
        print("----- find-runtime -----")
        rc, out = run([args.gcscope, "find-runtime", str(pid)])
        print(out)
        if rc != 0:
            return fail("could not locate _PyRuntime")

        # Informational only: read-runtime uses its own finder, not attach, so a
        # failure here says nothing about the pipeline under test.
        print("----- read-runtime (version, best-effort) -----")
        _, out = run([args.gcscope, "read-runtime", str(pid)])
        print("\n".join(out.splitlines()[:4]))

        print("----- gc-stats -----")
        rc, out = run([args.gcscope, "gc-stats", str(pid)])
        print(out)
        if rc != 0:
            return fail("gc-stats exited %d" % rc)
        if "No GC stats found." in out:
            return fail("stats decoded empty")
        if "Collections" not in out:
            return fail("no stats table in output")

        print("PASS(%s): attached + detected + decoded non-empty GC stats" % label)
        return 0
    finally:
        # Mirrors the SpawnedPython kill-on-drop guard: one cleanup path for
        # every exit, including the exception one.
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        log.close()


if __name__ == "__main__":
    sys.exit(main())
