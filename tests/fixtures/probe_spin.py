"""Attach target with the Probe installed: publishes a Ring, then keeps it moving.

Usage:  python probe_spin.py [max_seconds]

Same harness contract as `spin.py`: one flushed "READY <pid>" line, self-terminating after
max_seconds so a crashed harness cannot orphan it. This one imports `gcscope_probe` and
registers its `gc.callbacks` entry before collecting, so every generation already holds
Records when a reader attaches.

The only output is a pid. The reader finds the module among this process's mappings,
resolves `gcscope_probe_header` from its export table, and takes the region address and
geometry from there.

`spin.py` stays 3.8-compatible; this runs only where the Probe does, which is 3.14 today.
"""

import gc
import os
import sys
import time

import gcscope_probe

CYCLES_PER_BURST = 2000
# Gen-0 seed rounds. Gen 1 gets 5 and gen 2 gets 1, matching spin.py: unequal counts give a
# reader a strict collections[0] > [1] > [2] pyramid, which separates a correct decode from
# one whose base offsets alias two generations onto the same entries. An even rotation would
# leave those two indistinguishable.
GEN_SEED_ROUNDS = 20


def make_garbage(count):
    """Allocate `count` unreachable reference cycles and drop them.

    They have to be cycles: refcounting reclaims a plain list without reaching the
    collector, so no callback fires and no Record gets written.
    """
    for _ in range(count):
        a = {}
        b = {"peer": a, "payload": [0] * 32}
        a["peer"] = b
        del a, b


def main():
    max_seconds = float(sys.argv[1]) if len(sys.argv) > 1 else 120.0

    # Before any collection below, so every Record in the Ring comes from this run.
    gcscope_probe.install()
    gc.enable()

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
        # Same weighting as the seed, so the pyramid keeps holding while a reader polls
        # across several samples. A bare gc.collect() is full and would only bump gen 2.
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
