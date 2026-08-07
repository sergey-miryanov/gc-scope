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

3.13 and 3.14, with the GIL. On every pull request that reaches the layout contract, CI builds a
Probe from source on Linux, Windows and macOS against each of those minors, attaches to it, and
decodes the Records out of the process. `macos-latest` is Apple Silicon, so those legs run on
native arm64. That is the only configuration where the release store on `ts_stop` does work
x86-64's TSO would have done anyway. Emulation would not settle it, since it may serialise the
writes and hide the defect, so the workflow fails if that runner ever stops being arm64.

Each leg compiles against its own interpreter's internal headers, under gcc, MSVC or Apple
clang. The header lookup reads PE exports, ELF `.dynsym` and the Mach-O export trie through one
path.

No leg covers musllinux, 32-bit, debug builds, or arm64 outside macOS. `specs/0015` owns wheels
and the rest of the matrix.

The behaviour otherwise matches the prototype that proved the approach works
(`docs/research/cpython-314-gc-hook-points.md` §11–§12).

| Today | Becomes | Where |
|---|---|---|
| Counters starting at zero on install | Seeded from CPython's own, so they stay Lifetime totals | spec 0013 §4 |
| `layout_digest` empty | Filled from the generated layout header | spec 0012 |
| Read by an integration test | Discovered and validated by gcscope | spec 0014 |
| Built from source | Wheels | spec 0015 |

Three gates decide whether it loads, each naming what it refused and why, rather than publishing
numbers read at offsets it was not built for:

- **The minor.** 3.12 and below have no public `PyTime_PerfCounterRaw` to time a Collection
  with, so a source build there stops at compile time. From 3.15 CPython publishes these
  statistics itself, and a Probe would be a second source for the same numbers.
- **The patch release.** A module built against 3.14.5 refuses to load into 3.14.6, naming both.
  A wheel tag pins the minor and the stable ABI, and neither covers the internal struct layout
  `src/internals.c` takes its offsets from: `sizeof(_gc_runtime_state)` moved 24 bytes between
  3.14.4 and 3.14.5.
- **The GIL.** `heap_size` sits in the struct of a free-threaded build and nothing in
  `gc_free_threading.c` writes it, so a Probe would report 0 for every Collection and its
  self-check would call that healthy.

`gcscope_probe.version_refusal(hex)` answers what any `PY_VERSION_HEX` word would get, so the
first two are testable from an interpreter that loads. The third is decided at compile time and
has no version word to take.

**`heap_size` is absent on 3.13**, not zero: the field arrived with 3.14's collector rework, and
3.13's `_gc_runtime_state` ends before it. The Probe reads nothing there and publishes 0.

## What the capability word says

A field the Probe cannot fill publishes 0, so does a genuinely empty one, and so does one whose
offset is wrong. The header's `capabilities` separates the three. A set bit is a claim, so a
reader finding the word zero concludes nothing is meaningful.

| Bit | Set when |
|---|---|
| `OFFSETS_OK` | `gcstate->collecting` read 1 inside a callback, where it must be. Clear until the first Collection. |
| `HEAP_SIZE_PRESENT` | This interpreter's `_gc_runtime_state` has the field at all. Clear on 3.13. |
| `HEAP_SIZE_VALID` | ...and the check below reached it. Present without this is a field the Probe suppressed. |
| `CANDIDATES_VALID` | Never. `deduce_unreachable()` is `static inline` and the count is reachable nowhere else. |
| `COUNTERS_SEEDED` | Nothing sets it yet. It will mean `collections`, `collected` and `uncollectable` are Lifetime totals rather than counts since install (spec 0013 §4). |

`duration` gets no bit. CPython never recorded it, so there is nothing to seed from and it stays
Install-relative on every build. That is what the word guards: a Lifetime-total `collections` and
an Install-relative `duration` share a 64-byte entry, and dividing one by the other is wrong with
nothing to say so.

**`heap_size` is checked causally, not for plausibility.** At import the Probe allocates 1024
tracked objects, watches the field rise by at least that many, drops them, and watches it fall by
at least that many. `generations[0].count` also rises; nothing else in the struct does both. This
answers separately from `OFFSETS_OK`, which validates `offsetof(PyInterpreterState, gc)` and
`collecting` against *each other* and lets through a `heap_size` that moved on its own, as it did
between 3.14.4 and 3.14.5 when `sizeof(_gc_runtime_state)` moved 24 bytes.

A failed check suppresses the field instead of publishing what it found.
`_fault_heap_size_offset` runs that path: it displaces the offset so
`probe_reports_a_suppressed_heap_size` can read the consequences out of the process.

## Install

```
pip install .
```

That is the whole build. No Visual Studio path, no SDK path and no interpreter path appears in
this directory; setuptools finds the toolchain.

You need a C compiler and the CPython headers for your interpreter, **including the internal
ones** in `include/pythonX.Y/internal/`. `heap_size` and `collecting` live in a struct CPython
does not expose, so `src/internals.c` takes their offsets from those headers at compile time
rather than carrying transcribed numbers
([ADR 0013](../docs/adr/0013-probe-offsets-are-compiled-in.md)): the same 3.14 puts that struct
at a different offset on Windows and on Linux. Which headers it reaches for depends on the
minor, since 3.14 consolidated `PyInterpreterState` and `_gc_runtime_state` into
`pycore_interp_structs.h` while 3.13 splits them. The python.org installers ship the internal
headers; on Debian and Ubuntu they come with `pythonX.Y-dev`. Without them the build stops and
says so.

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
size, Ring depths, slot layout, host version, which collector this interpreter runs, and which
fields mean anything, so nothing passes out of band.

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
Collections, as it does on 3.13, which never shipped that collector. The header publishes
`collector` and `py_version` so you can tell. Pool data across the two without checking and you
compare unlike quantities, with nothing to warn you.

**Counters are Install-relative today.** They start at zero when you install the Probe, while
the region is byte-identical to a Native one whose same fields are Lifetime totals. Seeding
(spec 0013 §4) makes them agree.

**Keep the magic a `char[8]`** if you change it. As a `uint64_t` literal it stores
byte-reversed, and a scan for the ASCII string finds nothing.

Two paths carried over from the prototype have never run: more than eight concurrent
interpreters, where the ninth is dropped rather than corrupting, and concurrent
sub-interpreters with separate GILs.
