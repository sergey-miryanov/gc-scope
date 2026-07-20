"""Attach target: a process that keeps CPython's cyclic GC continuously busy.

Usage:  python spin.py [max_seconds]

Prints one flushed "READY <pid>" line once every generation has been collected at
least once, so a harness can wait on that instead of sleeping a guessed interval.
Self-terminates after max_seconds (default 120) so a crashed harness cannot orphan
it.

Must stay 3.8-compatible with no third-party imports: it runs unchanged against
every interpreter in the test matrix.
"""

import gc
import os
import sys
import time

CYCLES_PER_BURST = 2000


def make_garbage(count):
    """Allocate `count` unreachable reference cycles and drop them.

    Cycles are the point: a plain list would be reclaimed by refcounting and
    never move the generation counters.
    """
    for _ in range(count):
        a = {}
        b = {"peer": a, "payload": [0] * 32}
        a["peer"] = b
        del a, b


def main():
    max_seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 120.0

    gc.enable()

    # Seed every generation before READY, so a reader that attaches the instant it
    # sees the marker isn't racing the first gen-2 collection.
    make_garbage(CYCLES_PER_BURST)
    for generation in range(3):
        gc.collect(generation)

    sys.stdout.write("READY %d\n" % os.getpid())
    sys.stdout.flush()

    # monotonic: a wall-clock jump must not shorten or extend the backstop.
    deadline = time.monotonic() + max_seconds
    tick = 0
    while time.monotonic() < deadline:
        make_garbage(CYCLES_PER_BURST)
        # Rotate generations; a bare gc.collect() is full and only bumps gen 2.
        gc.collect(tick % 3)
        tick += 1
        time.sleep(0.05)
    return 0


if __name__ == "__main__":
    sys.exit(main())
