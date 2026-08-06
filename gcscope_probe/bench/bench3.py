"""Clean denominator: time ONLY gc.collect(0), with the young gen filled to
threshold beforehand (allocation excluded from the timed region)."""
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

thr0 = gc.get_threshold()[0]
gc.disable()

def run(cb, n_samples=400):
    gc.callbacks.clear()
    if cb is not None:
        gc.callbacks.append(cb)
    samples = []
    for _ in range(n_samples):
        junk = []
        for i in range(thr0):
            d = {"i": i}
            d["self"] = d
            junk.append(d)
        del junk                      # now ~2000 cyclic dicts are unreachable in gen 0
        t0 = time.perf_counter_ns()
        gc.collect(0)                 # <-- only this is timed
        t1 = time.perf_counter_ns()
        samples.append(t1 - t0)
    gc.callbacks.clear()
    samples.sort()
    return samples[len(samples)//2], samples[len(samples)//10]

print(sys.version)
print(f"\n=== gc.collect(0) only, ~{thr0} unreachable cyclic dicts in young gen ===\n")
base = None
for name, cb in CONFIGS:
    med, p10 = run(cb)
    if base is None:
        base = med
    d = med - base
    print(f"  {name:<18} median {med:9,.0f} ns   p10 {p10:9,.0f} ns"
          f"   overhead {d:+7,.0f} ns ({d/base*100:+5.2f}%)")

print(f"\nring records: {gcscope_probe.records():,}")
print("geometry:", gcscope_probe.geometry())
