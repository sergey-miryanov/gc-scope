# gcscope-probe

A CPython extension that publishes per-Collection GC timing from **inside** your process, so
that [gcscope](../README.md) can read it from outside.

Below 3.15, CPython records `collections`, `collected` and `uncollectable` per generation and
no timestamps whatsoever. gcscope can tell you how *often* the collector ran; it cannot tell
you what any Collection cost. This module closes that gap: a `gc.callbacks` entry writes a
region shaped exactly like 3.15's native `struct gc_stats`, which gcscope's ring decoder
already knows how to read.

Why a C extension lives in a Rust repository is answered by
[ADR 0016](../docs/adr/0016-probe-ships-from-this-repo.md).

## Status

**3.14 only, Windows only, x86-64 only.** This is the prototype, moved here and packaged; the
behaviour is unchanged from the version that proved the approach works
(`docs/research/cpython-314-gc-hook-points.md` §11–§12). What it is *not* yet:

| Not yet | Becomes | Where |
|---|---|---|
| Interpreter offsets transcribed from one build | Compiled from the interpreter's own headers | spec 0013 §4 |
| A release *fence* before a plain `ts_stop` store | An explicit release store — a correctness fix on aarch64 | spec 0013 §4 |
| `__declspec(dllexport)`, MSVC, PE | Portable visibility, Linux and macOS | spec 0013 §4 |
| Counters starting at zero on install | Seeded from CPython's own, so they stay Lifetime totals | spec 0013 §4 |
| `heap_size` 0 meaning absent, failed *and* empty | A capability word that tells the three apart | spec 0013 §4 |
| Read by an integration test | Discovered and validated by gcscope itself | spec 0014 |
| Built from source | Wheels | spec 0015 |

It refuses to import on anything but 3.14, naming the reason, rather than publishing plausible
numbers read at offsets it was not built for.

## Install

```
pip install .
```

That is the whole build. No Visual Studio path, no SDK path, no interpreter path appears
anywhere in this directory — setuptools finds the toolchain. You need a C compiler and the
CPython headers for the interpreter you are installing into (on Windows, the standard
python.org installer provides them).

Ring depths default to 11 young / 3 old, which makes the region byte-identical to a Native
3.15 one. Override at build time:

```
CFLAGS="-DGCSCOPE_PROBE_YOUNG_STATS_SIZE=512 -DGCSCOPE_PROBE_OLD_STATS_SIZE=128" pip install .
```

Depth buys per-Collection fidelity and nothing else — every other field is cumulative, so
totals survive wrap-around by differencing. What wraps away is the individual
`ts_start`/`ts_stop`/`heap_size` of skipped Collections. At the measured young-generation rate
(~176/s under churn) an 11-slot Ring laps in ~62 ms.

## Use

```python
import gcscope_probe
gcscope_probe.install()   # appends on_gc to gc.callbacks
```

Then attach gcscope to the process. Everything a reader needs — the region address, item size,
Ring depths, slot layout, the host's version, and which collector this interpreter runs — is
published in the exported `gcscope_probe_header`; nothing is passed out of band.

## Verify

The correctness gate is a gcscope integration test that attaches to a live interpreter and
decodes the region through gcscope's own reader and decoder:

```
pip install .
cargo test --test probe -- --ignored
```

It asserts the decoded **shape**, never that a read succeeded — a Probe writing at a wrong
offset publishes a full table of plausible garbage that any non-empty check waves through.
With no Probe installed the test skips with a log; a CI leg that built one sets
`GCSCOPE_REQUIRE_PROBE=1` so a skip fails instead.

## Benchmarks

```
python bench/bench2.py    # per-invocation overhead against an empty young generation
python bench/bench3.py    # times only gc.collect(0) with a threshold-full young generation
```

`bench2.py` isolates the signal. `bench3.py`'s denominator drifts ±3 µs run to run, which is
itself the finding: the overhead is smaller than the noise floor of the Collection it observes.
`specs/0013` ticket 10 turns this into a regression guard rather than a measurement.

## Things to know before changing this

**The module filename is a wire contract.** gcscope discovers a Probe by matching mapped image
basenames against the `gcscope_probe` prefix and looking up `gcscope_probe_header` in the
export table ([ADR 0014](../docs/adr/0014-probe-regions-discovered-by-module-export.md)). A
rename breaks discovery in every released gcscope *silently* — a renamed module still loads,
still installs its callback, still publishes a valid region that nothing can find.

**Ring index 1 means different things across the 3.14 line.** On 3.14.0–3.14.4 (incremental
collector) index 1 counts *increments* of the old space, not old-generation passes; on 3.14.5
it counts true gen-1 Collections. The header publishes `collector` and `py_version` so a
consumer can tell. Pooling data across the two without checking compares unlike quantities, and
nothing errors.

**Counters are Install-relative today.** Every cumulative counter starts at zero when the Probe
is installed, while the region is byte-identical to a Native one where the same fields are
Lifetime totals. Seeding (spec 0013 §4) is what makes them agree.

**If you change the magic, keep it a `char[8]`.** As a `uint64_t` literal it is stored
byte-reversed and a scan for the ASCII string silently finds nothing.

**Untested paths carried over from the prototype:** more than eight concurrent interpreters
(the ninth is dropped rather than corrupting, but that path has never run), and genuinely
concurrent sub-interpreters with separate GILs.
