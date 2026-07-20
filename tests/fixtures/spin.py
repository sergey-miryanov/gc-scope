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
# Gen-0 seed rounds. Gen 1 gets 5 and gen 2 gets 1, giving the strict pyramid the
# harness asserts on; see main().
GEN_SEED_ROUNDS = 20


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
    #
    # The counts are deliberately unequal and applied BEFORE the marker, so a
    # reader sees a strict collections[0] > [1] > [2] pyramid no matter how
    # quickly it attaches. That asymmetry is load-bearing: it is what lets a
    # checker tell the three generations apart, and so catch a decode whose base
    # offsets alias one generation onto another's slot (right shape, wrong data).
    # An equal rotation would make those two cases indistinguishable.
    for generation, rounds in ((0, GEN_SEED_ROUNDS), (1, 5), (2, 1)):
        for _ in range(rounds):
            make_garbage(CYCLES_PER_BURST)
            gc.collect(generation)

    sys.stdout.write("READY %d\n" % os.getpid())
    sys.stdout.flush()

    # monotonic: a wall-clock jump must not shorten or extend the backstop.
    deadline = time.monotonic() + max_seconds
    tick = 0
    while time.monotonic() < deadline:
        make_garbage(CYCLES_PER_BURST)
        # Same weighting as the seed, so the pyramid keeps holding while a reader
        # polls. A bare gc.collect() is full and would only bump gen 2.
        gc.collect(0)
        if tick % 5 == 4:
            gc.collect(1)
        if tick % 25 == 24:
            gc.collect(2)
        tick += 1
        time.sleep(0.05)
    return 0


if __name__ == "__main__":
    sys.exit(main())
