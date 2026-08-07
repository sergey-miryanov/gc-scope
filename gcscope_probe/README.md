# gcscope-probe

A CPython extension that publishes per-Collection GC timing from inside your process, so
[gcscope](../README.md) can read it from outside.

Below 3.15, CPython records `collections`, `collected` and `uncollectable` per generation and
no timestamps. gcscope can tell you how often the collector ran, not what any Collection cost.
This module closes that gap: a `gc.callbacks` entry writes a region shaped like 3.15's native
`struct gc_stats`, which gcscope's ring decoder already reads.

[ADR 0016](../docs/adr/0016-probe-ships-from-this-repo.md) answers why a C extension lives in a
Rust repository.

## Status

3.14 only, x86-64 only. CI builds a Linux Probe, attaches to it and decodes it on every pull
request that reaches the layout contract. Windows passes the same test when someone runs it by
hand; no CI leg compiles this on Windows, so a Windows-only break merges green until
`specs/0015` adds one. macOS is written for and unproven. The behaviour matches the prototype
that proved the approach works (`docs/research/cpython-314-gc-hook-points.md` §11–§12).

| Today | Becomes | Where |
|---|---|---|
| A release fence before a plain `ts_stop` store | An explicit release store, which aarch64 needs | spec 0013 §4 |
| Mach-O untested, no arm64 anywhere | macOS and arm64 proven by a leg, not by compiling | spec 0013 §4, spec 0015 |
| Counters starting at zero on install | Seeded from CPython's own, so they stay Lifetime totals | spec 0013 §4 |
| `heap_size` 0 meaning absent, failed and empty alike | A capability word that tells the three apart | spec 0013 §4 |
| Read by an integration test | Discovered and validated by gcscope | spec 0014 |
| Built from source | Wheels | spec 0015 |

On anything but 3.14 it refuses to import, naming the reason, rather than publishing numbers
read at offsets it was not built for. Free-threaded builds are refused too: `heap_size` is
there in the struct and nothing in `gc_free_threading.c` ever writes it, so a Probe would
report 0 for every Collection and its self-check would call that healthy.

## Install

```
pip install .
```

That is the whole build. No Visual Studio path, no SDK path and no interpreter path appears in
this directory; setuptools finds the toolchain.

You need a C compiler and the CPython headers for your interpreter, **including the internal
ones** in `include/pythonX.Y/internal/`. `heap_size` lives in a struct CPython does not expose,
so `src/internals.c` takes its offset from those headers at compile time rather than carrying a
number somebody transcribed ([ADR 0013](../docs/adr/0013-probe-offsets-are-compiled-in.md)) —
the same 3.14 puts that struct at a different offset on Windows and on Linux. The python.org
installers ship the internal headers; on Debian and Ubuntu they come with `pythonX.Y-dev`. If
they are absent the build stops and says so.

MSVC needs `/std:c11 /experimental:c11atomics` for `<stdatomic.h>`; `setup.py` adds both when
it sees MSVC. That puts a floor under the Windows toolchain at **Visual Studio 2022 17.5**,
where the second flag first exists. Older toolchains fail at `"C atomic support is not
enabled"`, and no CI leg catches it. gcc and clang need no flags.

Ring depths default to 11 young and 3 old, making the region byte-identical to a Native 3.15
one. Override at build time:

```
CFLAGS="-DGCSCOPE_PROBE_YOUNG_STATS_SIZE=512 -DGCSCOPE_PROBE_OLD_STATS_SIZE=128" pip install .
```

Depth buys per-Collection fidelity. The other fields are cumulative, so totals survive
wrap-around by differencing; what wraps away is the `ts_start`, `ts_stop` and `heap_size` of
skipped Collections. At the measured young-generation rate of ~176/s under churn, an 11-slot
Ring laps in ~62 ms.

## Use

```python
import gcscope_probe
gcscope_probe.install()   # appends on_gc to gc.callbacks
```

Then attach gcscope. The exported `gcscope_probe_header` publishes the region address, item
size, Ring depths, slot layout, host version and which collector this interpreter runs, so
nothing passes out of band.

## Verify

```
pip install .
cargo test --test probe -- --ignored
```

The test attaches to a live interpreter and decodes the region through gcscope's own reader
and decoder. It asserts the decoded shape rather than a successful read: a Probe writing at a
wrong offset publishes a full table of plausible garbage that any non-empty check waves
through. With no Probe installed the test skips, so a CI leg that built one sets
`GCSCOPE_REQUIRE_PROBE=1` and gets a failure instead.

## Benchmarks

```
python bench/bench2.py    # per-invocation overhead against an empty young generation
python bench/bench3.py    # times only gc.collect(0) with a threshold-full young generation
```

`bench2.py` isolates the signal. `bench3.py`'s denominator drifts ±3 µs run to run, which is
the finding: the overhead sits below the noise floor of the Collection it observes. Ticket 10
turns this into a regression guard.

## Before you change this

**The module filename is a wire contract.** gcscope finds a Probe by matching mapped image
basenames against the `gcscope_probe` prefix, then looking up `gcscope_probe_header` in the
export table ([ADR 0014](../docs/adr/0014-probe-regions-discovered-by-module-export.md)).
Rename either and discovery breaks in every released gcscope with no error to read: the
renamed module still loads, still installs its callback, still publishes a valid region that
nothing can find.

**Ring index 1 means different things across the 3.14 line.** On 3.14.0–3.14.4, running the
incremental collector, index 1 counts increments of the old space; on 3.14.5 it counts gen-1
Collections. The header publishes `collector` and `py_version` so you can tell. Pool data
across the two without checking and you compare unlike quantities, with nothing to warn you.

**Counters are Install-relative today.** They start at zero when you install the Probe, while
the region is byte-identical to a Native one whose same fields are Lifetime totals. Seeding
(spec 0013 §4) makes them agree.

**Keep the magic a `char[8]`** if you change it. As a `uint64_t` literal it stores
byte-reversed, and a scan for the ASCII string finds nothing.

Two paths carried over from the prototype have never run: more than eight concurrent
interpreters, where the ninth is dropped rather than corrupting, and concurrent
sub-interpreters with separate GILs.
