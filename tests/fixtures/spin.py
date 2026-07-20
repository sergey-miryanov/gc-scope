"""Attach target: a process that keeps CPython's cyclic GC continuously busy.

Shared by the CI `live-smoke` job and (later) the local integration harness
(see docs/tests-harness-plan.md). gcscope needs a target whose GC *generation
counters actually advance*, on every supported interpreter — so this file
sticks to plain 3.8-compatible syntax with no third-party imports and no
f-strings, and must keep working unchanged through 3.16.

Usage:  python spin.py [max_seconds]

Prints one flushed "READY <pid>" line once the interpreter is fully initialized
and every generation has been collected at least once, so a harness can wait on
that marker instead of sleeping a guessed interval. Self-terminates after
max_seconds (default 120) so a crashed or killed harness cannot orphan it.
"""

import gc
import os
import sys
import time

# Per burst. Big enough that a collection has real work to attribute to the
# counters, small enough that the loop stays responsive to a kill.
CYCLES_PER_BURST = 2000


def make_garbage(count):
    """Allocate `count` unreachable reference cycles and drop them.

    Cycles are the point: a plain list would be reclaimed by refcounting and
    would never move the generation counters. Two dicts pointing at each other
    can only be freed by the cyclic collector.
    """
    for _ in range(count):
        a = {}
        b = {"peer": a, "payload": [0] * 32}
        a["peer"] = b
        del a, b


def main():
    max_seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 120.0

    gc.enable()

    # Seed every generation before announcing READY, so a reader that attaches
    # the instant it sees the marker finds non-zero stats in all of them rather
    # than racing the first gen-2 collection.
    make_garbage(CYCLES_PER_BURST)
    for generation in range(3):
        gc.collect(generation)

    sys.stdout.write("READY %d\n" % os.getpid())
    sys.stdout.flush()

    # monotonic, not time(): the lifetime backstop must not be shortened or
    # extended by a wall-clock jump (NTP correction, DST, a CI runner syncing
    # its clock mid-job).
    deadline = time.monotonic() + max_seconds
    tick = 0
    while time.monotonic() < deadline:
        make_garbage(CYCLES_PER_BURST)
        # Rotate the target generation so all three keep advancing; a bare
        # gc.collect() is a full collection and would only bump gen 2.
        gc.collect(tick % 3)
        tick += 1
        time.sleep(0.05)
    return 0


if __name__ == "__main__":
    sys.exit(main())
