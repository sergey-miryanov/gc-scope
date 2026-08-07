"""Cost of a gc.callbacks entry on CPython 3.14.5.

Every configuration measures the SAME workload; the only variable is what is
registered in gc.callbacks. invoke_gc_callback fires twice per collection
("start" at Python/gc.c:1358, "stop" at :1527), so per-collection overhead
covers two invocations.
"""
import gc, sys, statistics, time
from collections import deque

import gcscope_probe

print(sys.version)
print("ring @", hex(gcscope_probe.region_addr()))

# ---- Python-side callbacks (the thing the roundtrip worry is about) --------
_sink = deque(maxlen=4096)

def py_noop(phase, info):
    pass

def py_real(phase, info):
    _sink.append((phase, info["generation"], info["collected"], info["uncollectable"]))

CONFIGS = [
    ("no callback",        None),
    ("C no-op",            gcscope_probe.noop),
    ("C on_gc (full record)", gcscope_probe.on_gc),
    ("Python no-op",       py_noop),
    ("Python + unpack",    py_real),
]

def make_heap(n):
    """n container objects, all reachable, each in a small cycle so the
    collector must actually traverse and cannot free them."""
    keep = []
    for i in range(n):
        a = {"i": i}
        b = [a]
        a["back"] = b
        keep.append(b)
    return keep

def measure(cb, gen, batches, per_batch):
    if cb is None:
        gc.callbacks.clear()
    else:
        gc.callbacks.clear()
        gc.callbacks.append(cb)
    # warm
    for _ in range(3):
        gc.collect(gen)
    times = []
    for _ in range(batches):
        t0 = time.perf_counter_ns()
        for _ in range(per_batch):
            gc.collect(gen)
        t1 = time.perf_counter_ns()
        times.append((t1 - t0) / per_batch)
    gc.callbacks.clear()
    return statistics.median(times)

def run(label, heap_n, gen, batches=9, per_batch=20):
    heap = make_heap(heap_n)
    gc.collect()
    gc.disable()
    print(f"\n=== {label}: heap={heap_n:,} containers, gc.collect({gen}) ===")
    base = None
    for name, cb in CONFIGS:
        ns = measure(cb, gen, batches, per_batch)
        if base is None:
            base = ns
        delta = ns - base
        pct = (delta / base * 100) if base else 0.0
        print(f"  {name:<18} {ns:12,.0f} ns/collect   delta {delta:+10,.0f} ns  ({pct:+6.2f}%)")
    gc.enable()
    del heap
    gc.collect()

# 1. Tiny heap -- collection cost is near zero, so the delta IS the callback cost.
run("isolate callback cost", 200, 2, batches=11, per_batch=200)

# 2. Realistic heap -- same overhead, now as a fraction of real work.
run("realistic full collection", 200_000, 2, batches=7, per_batch=5)

# 3. Young-gen collection, the frequent one.
run("young-gen collection", 200_000, 0, batches=9, per_batch=50)

# ---- 4. End-to-end: automatic collections under allocation churn ----------
print("\n=== churn workload, automatic gc enabled ===")

def churn(rounds=300_000):
    junk = []
    for i in range(rounds):
        d = {"x": i}
        d["self"] = d
        junk.append(d)
        if len(junk) > 1000:
            junk.clear()

for name, cb in CONFIGS:
    gc.callbacks.clear()
    if cb is not None:
        gc.callbacks.append(cb)
    gc.collect()
    gc.enable()
    before = [s["collections"] for s in gc.get_stats()]
    t0 = time.perf_counter_ns()
    churn()
    t1 = time.perf_counter_ns()
    after = [s["collections"] for s in gc.get_stats()]
    ncoll = sum(a - b for a, b in zip(after, before))
    gc.callbacks.clear()
    print(f"  {name:<18} {(t1-t0)/1e6:9,.1f} ms total   {ncoll:6,} collections")

print("\nring records written:", gcscope_probe.records())
print("geometry:", gcscope_probe.geometry())
