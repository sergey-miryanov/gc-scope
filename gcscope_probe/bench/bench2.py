"""Tighter measurement: isolate per-invocation callback cost, then express it
against a realistically-sized young-gen collection."""
import gc, sys, statistics, time
from collections import deque

import gcscope_probe

_sink = deque(maxlen=4096)
def py_noop(phase, info): pass
def py_real(phase, info):
    _sink.append((phase, info["generation"], info["collected"], info["uncollectable"]))

CONFIGS = [
    ("no callback",     None),
    ("C no-op",         gcscope_probe.noop),
    ("C on_gc (record)", gcscope_probe.on_gc),
    ("Python no-op",    py_noop),
    ("Python + unpack", py_real),
]

def bench(cb, setup, body, batches, per_batch):
    gc.callbacks.clear()
    if cb is not None:
        gc.callbacks.append(cb)
    for _ in range(5):
        body()
    times = []
    for _ in range(batches):
        setup()
        t0 = time.perf_counter_ns()
        for _ in range(per_batch):
            body()
        t1 = time.perf_counter_ns()
        times.append((t1 - t0) / per_batch)
    gc.callbacks.clear()
    return statistics.median(times)

# ---------------------------------------------------------------- part 1
# Empty young gen: gc.collect(0) does essentially no work, so the whole delta
# is the two invoke_gc_callback calls.
print(sys.version)
gc.collect(); gc.disable()
print("\n=== per-collection callback overhead (empty young gen) ===")
print("    2 invocations per collection: 'start' gc.c:1358, 'stop' gc.c:1527\n")
base = None
results = {}
for name, cb in CONFIGS:
    ns = bench(cb, lambda: None, lambda: gc.collect(0), batches=15, per_batch=400)
    if base is None:
        base = ns
    d = ns - base
    results[name] = d
    print(f"  {name:<18} {ns:8,.0f} ns/collect   overhead {d:+8,.0f} ns"
          f"   = {d/2:+7,.0f} ns/invocation")

# ---------------------------------------------------------------- part 2
# What does a young-gen collection actually cost when the young gen is full?
gc.enable()
thr0 = gc.get_threshold()[0]
print(f"\n=== realistic young-gen collection (threshold0={thr0}) ===")
gc.disable()

live = []
def setup_young():
    del live[:]
    for i in range(thr0):
        d = {"i": i}
        d["self"] = d
        live.append(d)

def collect0():
    for i in range(thr0):
        d = {"i": i}
        d["self"] = d
    gc.collect(0)

alloc_only_t = None
def alloc_only():
    for i in range(thr0):
        d = {"i": i}
        d["self"] = d

t_alloc = bench(None, lambda: None, alloc_only, batches=11, per_batch=200)
print(f"  allocation of {thr0} cyclic dicts alone: {t_alloc:,.0f} ns")

base2 = None
for name, cb in CONFIGS:
    ns = bench(cb, lambda: None, collect0, batches=11, per_batch=200)
    coll = ns - t_alloc
    if base2 is None:
        base2 = coll
    print(f"  {name:<18} collect0 ~= {coll:9,.0f} ns   overhead {coll-base2:+8,.0f} ns"
          f"  ({(coll-base2)/base2*100 if base2 else 0:+6.2f}%)")

print("\nring records:", gcscope_probe.records())
