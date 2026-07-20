"""Live smoke driver: spawn a real interpreter, attach with gcscope, assert stats.

Exercises the end-to-end pipeline (find `_PyRuntime` -> detect version -> read
offsets -> decode GC generation stats) against a live process. Used by the
`live-smoke` CI job and runnable by hand on any OS:

    python tests/live_smoke.py                       # uses this interpreter
    python tests/live_smoke.py --python C:/py38/python.exe --label py3.8

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

SPIN_LIFETIME_SECS = 120
READY_TIMEOUT_SECS = 20
CMD_TIMEOUT_SECS = 60


def default_gcscope():
    exe = "gcscope.exe" if os.name == "nt" else "gcscope"
    return os.path.join(REPO, "target", "debug", exe)


def run(argv, timeout=CMD_TIMEOUT_SECS):
    """Run a command, capturing merged output as text. Returns (rc, output)."""
    try:
        proc = subprocess.run(
            argv,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            # Explicit: the default locale encoding chokes on gcscope's
            # box-drawing output under a non-UTF-8 Windows code page.
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
        return proc.returncode, proc.stdout or ""
    except subprocess.TimeoutExpired:
        # A hang is a real failure mode here (bad pointer walk on an unknown
        # layout), so report it rather than letting CI stall.
        return 124, "TIMEOUT after %ds: %s" % (timeout, " ".join(argv))


def target_version(python):
    """(major, minor) of the interpreter under test, or None if unparseable."""
    rc, out = run([python, "-c",
                   "import sys; print('%d %d' % sys.version_info[:2])"])
    if rc != 0:
        return None
    try:
        major, minor = out.split()[:2]
        return int(major), int(minor)
    except (ValueError, IndexError):
        return None


def is_free_threaded(python):
    """True for a free-threaded (no-GIL) build, which halves the GC ring slots."""
    rc, out = run([python, "-c",
                   "import sysconfig;"
                   "print(sysconfig.get_config_var('Py_GIL_DISABLED') or 0)"])
    return rc == 0 and out.strip() == "1"


def expected_shape(version, free_threaded):
    """(kind, slots-per-generation) gcscope should decode for this interpreter.

    Mirrors `GcStatsKind` selection in `offsets/mod.rs`: one inline slot per
    generation through 3.14, ring buffers from 3.15. Ring slot counts differ
    between GIL (11/3/3) and free-threaded (1/1/1) builds.
    """
    if version is None:
        return None, None
    if version < (3, 15):
        return "InlineArray", [1, 1, 1]
    return "RingBuffer", ([1, 1, 1] if free_threaded else [11, 3, 3])


def parse_stats_table(out):
    """Rows of `gc-stats` output as dicts, ignoring the header and rule lines.

    Columns are fixed-width and shared by the plain and extended layouts; only
    the first nine are needed here.
    """
    rows = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 9:
            continue
        try:
            row = {
                "generation": int(parts[0]),
                "slot": int(parts[1]),
                "interpreter_id": int(parts[2]),
                "collections": int(parts[3]),
                "collected": int(parts[4]),
                "uncollectable": int(parts[5]),
                "candidates": int(parts[6]),
                "heap_size": int(parts[7]),
                "duration": float(parts[8]),
            }
        except ValueError:
            continue  # header or separator
        rows.append(row)
    return rows


# A wrong stats address still lands in mapped memory and returns whatever is
# there, so the counters are the only signal that the address was right. Real
# counters stay far below this; garbage rarely does.
SANE_COUNTER_MAX = 10 ** 12


def check_stats(rows, kind, slots, fail):
    """Assert the decoded table has the right shape and plausible values.

    Shape is the point: without it, a mis-keyed decode that emits the right
    number of garbage rows passes as readily as a correct one.
    """
    want = sum(slots)
    if len(rows) != want:
        return fail("expected %d %s rows (slots %s), decoded %d"
                    % (want, kind, slots, len(rows)))

    # Every (generation, slot) pair exactly once — catches a base offset that
    # aliases two generations onto the same slot range.
    got = sorted((r["generation"], r["slot"]) for r in rows)
    expect = sorted((g, s) for g in range(3) for s in range(slots[g]))
    if got != expect:
        return fail("wrong (generation, slot) set for %s: %s" % (kind, got))

    for r in rows:
        where = "gen %d slot %d" % (r["generation"], r["slot"])
        for name in ("collections", "collected", "uncollectable",
                     "candidates", "heap_size"):
            v = r[name]
            if v < 0 or v > SANE_COUNTER_MAX:
                return fail("%s: implausible %s=%d (reading the wrong address?)"
                            % (where, name, v))
        # Objects freed cannot exceed objects examined. `candidates` is 0 on
        # pre-3.13 builds, whose layout has no such field.
        if r["candidates"] and r["collected"] > r["candidates"]:
            return fail("%s: collected=%d exceeds candidates=%d"
                        % (where, r["collected"], r["candidates"]))

    # spin.py collects every generation before it prints READY, so each one must
    # show progress. Zeros across a whole generation mean we read a live-looking
    # but wrong region -- the 3.8 global-GC branch's failure mode in particular,
    # since it resolves the stats address from `_PyRuntime` instead of the
    # interpreter and a wrong base still reads as mapped memory.
    peak = []
    for gen in range(3):
        counts = [r["collections"] for r in rows if r["generation"] == gen]
        if max(counts) <= 0:
            return fail("generation %d shows no collections; spin.py collects "
                        "all three before READY" % gen)
        peak.append(max(counts))

    # The pyramid. spin.py seeds 20/5/1 collections into generations 0/1/2 before
    # READY and keeps that weighting afterwards, so this is deterministic rather
    # than timing-dependent -- and it is the check that catches a decode landing
    # on the right-shaped table with another generation's data (e.g. gen-2's base
    # offset aliasing gen 1), which the row-count and index checks above cannot
    # see. CPython's own invariant points the same way: an older generation is
    # never collected more often than a younger one.
    if not (peak[0] > peak[1] > peak[2]):
        return fail("generation collections %s are not a strict pyramid; "
                    "generations may be aliased onto the same slots" % peak)
    return 0


def wait_for_ready(proc, log_path):
    """Poll spin.py's log for READY; return the interpreter's PID, or None.

    The PID comes from the marker rather than proc.pid so it stays correct if a
    launcher or shim sits between us and the real interpreter.
    """
    deadline = time.monotonic() + READY_TIMEOUT_SECS
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return None  # died on startup; caller prints the log
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
    version = target_version(args.python)
    free_threaded = is_free_threaded(args.python)
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

        # Same attach path as gc-stats, so a failure here isolates *finding* from
        # decoding.
        print("----- find-runtime -----")
        rc, out = run([args.gcscope, "find-runtime", str(pid)])
        print(out)
        if rc != 0:
            # Separates "no python module was mapped" from "found it but the
            # cookie/cross-reference check failed".
            print("----- mapped python regions (diagnostic) -----")
            _, regions = run([args.gcscope, "list", str(pid)])
            shown = [ln for ln in regions.splitlines() if "ython" in ln]
            print("\n".join(shown[:25]) if shown
                  else "(no mapped region has a python path — "
                       "module enumeration is the problem)")
            return fail("could not locate _PyRuntime")

        # Informational: read-runtime uses its own finder, not attach.
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

        # Shape, not just presence: a mis-keyed decode emits a full table of
        # garbage and would otherwise pass every leg of the matrix.
        kind, slots = expected_shape(version, free_threaded)
        if kind is None:
            print("WARN: could not determine the target version; "
                  "skipping the shape check")
        else:
            print("expecting %s, slots %s%s"
                  % (kind, slots, " (free-threaded)" if free_threaded else ""))
            rows = parse_stats_table(out)
            if check_stats(rows, kind, slots, fail):
                return 1

        print("PASS(%s): attached + detected + decoded %s stats, shape %s"
              % (label, kind or "?", slots or "?"))
        return 0
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        log.close()


if __name__ == "__main__":
    sys.exit(main())
