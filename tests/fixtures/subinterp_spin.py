"""Attach target: a process whose main interpreter and a sub-interpreter both collect.

Usage:  python subinterp_spin.py [max_seconds]

Prints one flushed "READY <pid>" line once *both* interpreters have collected, so a
harness attaching on the marker knows there is more than interpreter zero to find.
Self-terminates after max_seconds (default 120) so a crashed harness cannot orphan it.

Exits non-zero with the reason on stderr if this build cannot create a sub-interpreter,
rather than quietly running as one: a harness that read a skip out of a silent success
would pass against the very thing it exists to catch. Callers gate on 3.12, the first
version that can create one from Python.
"""

import gc
import os
import sys
import tempfile
import threading
import time

CYCLES_PER_BURST = 2000
# How long the main interpreter waits for the sub-interpreter to stop. Both run to the
# same deadline, so this covers the last burst rather than a stretch of the run: an
# interpreter destroyed early hands the head of the chain back to interpreter zero, and a
# reader that only ever looks at the head then appears to have read both.
SUB_SHUTDOWN_GRACE = 10.0

# The sub-interpreter's own program. A source string rather than a function, since the
# two interpreters share no objects: everything it needs is spelled into it, and the
# marker file is how it reports back.
SUB_SOURCE = """
import gc
import time


def make_garbage(count):
    for _ in range(count):
        a = {}
        b = {"peer": a, "payload": [0] * 32}
        a["peer"] = b
        del a, b


gc.enable()
for generation, rounds in ((0, 20), (1, 5), (2, 1)):
    for _ in range(rounds):
        make_garbage(%(burst)d)
        gc.collect(generation)

# Seeded: this interpreter is now something to find.
open(%(marker)r, "w").close()

deadline = time.monotonic() + %(seconds)r
tick = 0
while time.monotonic() < deadline:
    make_garbage(%(burst)d)
    gc.collect(0)
    if tick %% 5 == 4:
        gc.collect(1)
    tick += 1
    time.sleep(0.05)
"""


def make_garbage(count):
    """Allocate `count` unreachable reference cycles and drop them."""
    for _ in range(count):
        a = {}
        b = {"peer": a, "payload": [0] * 32}
        a["peer"] = b
        del a, b


def sub_interpreter_runner(source):
    """A zero-argument callable that runs `source` in a fresh sub-interpreter and
    destroys it afterwards.

    Three spellings, newest first: PEP 734's `concurrent.interpreters` (3.14+), the
    private `_interpreters` (3.13) and `_xxsubinterpreters` (3.12). Raises if this
    build has none of them.

    Destroying it is not tidiness. A sub-interpreter still running while the main one
    finalizes takes the process down with an access violation, and the harness reads
    that crash as gcscope's.
    """
    try:
        from concurrent import interpreters
    except ImportError:
        pass
    else:
        interp = interpreters.create()

        def call_public():
            try:
                interp.exec(source)
            finally:
                interp.close()

        return call_public

    for name in ("_interpreters", "_xxsubinterpreters"):
        try:
            module = __import__(name)
        except ImportError:
            continue
        interp = module.create()
        run = getattr(module, "run_string", None) or getattr(module, "exec")

        def call_private():
            try:
                # `run_string` raises; `exec` hands back the exception instead, which
                # would otherwise leave the sub-interpreter silently dead and the marker
                # unwritten.
                failure = run(interp, source)
                if failure is not None:
                    raise RuntimeError("the sub-interpreter failed: %r" % (failure,))
            finally:
                module.destroy(interp)

        return call_private

    raise RuntimeError(
        "no sub-interpreter API on this build (tried concurrent.interpreters, "
        "_interpreters, _xxsubinterpreters)"
    )


def main():
    max_seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 120.0

    gc.enable()

    marker = os.path.join(
        tempfile.gettempdir(), "gcscope_subinterp_%d" % os.getpid()
    )
    # Both run to the same deadline, so both are collecting for every second gcscope
    # watches either; the sub-interpreter is destroyed after that, not during.
    source = SUB_SOURCE % {
        "burst": CYCLES_PER_BURST,
        "marker": marker,
        "seconds": max_seconds,
    }
    run_sub = sub_interpreter_runner(source)

    # Daemon as a backstop only: the join below is what actually shuts it down.
    worker = threading.Thread(target=run_sub, daemon=True)
    worker.start()

    # The same unequal seeding spin.py uses, for the same reason: a strict
    # collections[0] > [1] > [2] pyramid tells the generations apart.
    for generation, rounds in ((0, 20), (1, 5), (2, 1)):
        for _ in range(rounds):
            make_garbage(CYCLES_PER_BURST)
            gc.collect(generation)

    # Both interpreters seeded is the condition READY reports, so a reader that attaches
    # on the marker finds two of them however slowly the sub-interpreter started.
    deadline = time.monotonic() + max_seconds
    while not os.path.exists(marker):
        if not worker.is_alive():
            sys.stderr.write("the sub-interpreter died before it collected\n")
            return 1
        if time.monotonic() >= deadline:
            sys.stderr.write("the sub-interpreter never reported collecting\n")
            return 1
        time.sleep(0.05)

    sys.stdout.write("READY %d\n" % os.getpid())
    sys.stdout.flush()

    tick = 0
    while time.monotonic() < deadline:
        make_garbage(CYCLES_PER_BURST)
        gc.collect(0)
        if tick % 5 == 4:
            gc.collect(1)
        if tick % 25 == 24:
            gc.collect(2)
        tick += 1
        time.sleep(0.05)

    # Joined before finalization, for the reason `sub_interpreter_runner` gives.
    worker.join(SUB_SHUTDOWN_GRACE)
    if worker.is_alive():
        sys.stderr.write("the sub-interpreter did not stop on time\n")

    try:
        os.remove(marker)
    except OSError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
