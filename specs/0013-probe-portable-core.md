# 0013 — Move the Probe into this repo and make it portable

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** L
- **Origin:** Grilling session 2026-08-06 on productizing the Probe. The prototype lives at
  `X:/Work/gc-monitor/gcprobe` — one commit, no remote — and proved the approach works
  (research note §11–§12, `docs/research/cpython-314-gc-hook-points.md`). This spec turns
  that prototype into something shippable and moves it here as `gcscope_probe/`.
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md) (assert decoded shape),
  [ADR 0006](../docs/adr/0006-layout-registration-integrity.md) (fails closed),
  [ADR 0012](../docs/adr/0012-version-detection-fails-closed.md) (refuse rather than clamp)
- **Blocked by:** [0012](0012-gen-offsets-serves-the-probe.md) — the C layout header this
  asserts against does not exist yet. The port can start before it lands; the compile-time
  assertion cannot.

## 1. Problem statement

An operator running CPython 3.13 or 3.14 on Linux or macOS — which is nearly all of them —
cannot get per-Collection timing out of their process at all, and there is no path to it that
does not start with "build this yourself on Windows with MSVC."

Below 3.15 CPython publishes `collections`, `collected` and `uncollectable` per generation and
no timestamps whatsoever. gcscope reads those counts and can say how *often* the collector
ran; it cannot say what any Collection cost. The Probe closes that gap by recording timing
from inside the process, and it demonstrably works — but only on Windows, only on x86-64, only
against one hardcoded patch release, and only if you have Visual Studio and run a `.bat` file.

Three things stand between that and something an operator can install:

**It is Windows-shaped in four independent places.** `<windows.h>` with
`InterlockedCompareExchange` and `MemoryBarrier` for slot claiming and record publication;
`__declspec(dllexport)` for the discovery anchor; MSVC-only `.bat` builds; and a reader that
parses a **PE** export table. Each needs a different portable answer, and one of them is not a
portability chore at all — see below.

**Its offsets are hardcoded to one build.** `GCPROBE_INTERP_GC_OFF 7400`,
`GCPROBE_GC_HEAP_SIZE_OFF 216`, `GCPROBE_GC_COLLECTING_OFF 192` were transcribed from 3.14.5
and verified by hand against 3.14.0 and 3.14.4. They are correct for exactly the build
configurations someone checked.

**Its data does not mean what a reader will assume.** Every cumulative counter starts at zero
when the Probe is installed, so the `collections` an operator sees is not the interpreter's
`collections` — while being byte-identical to a Native ring where it *is*. `heap_size` reads 0
on 3.13, where the field does not exist, and also 0 when the self-check failed, and also 0 when
the heap is genuinely empty. And the self-check result is reachable only through a Python-level
`geometry()` call, which the one consumer that matters — an out-of-process reader — cannot make.

## 2. Solution

An operator on 3.13 or 3.14, on Linux, macOS or Windows, on x86-64 or arm64, adds one
dependency to their application, imports it, and gcscope starts reporting per-Collection pause
times for a process it previously could only count Collections in.

The Probe becomes `gcscope_probe`, living in this repo and released on its own train. It is
built per interpreter minor and per platform, so the interpreter internals it reads are
*compiled* from that interpreter's own headers rather than transcribed from one build. It
refuses to load, loudly, on anything it was not built for.

What it publishes becomes self-describing rather than assumed. Its cumulative counters are
seeded from CPython's own at install, so they remain **Lifetime totals** and every figure
gcscope derives from them keeps its existing meaning. The figures that *cannot* be seeded —
`duration`, because CPython never recorded it, and `candidates`, which is unobtainable — are
declared **Install-relative** in the region header rather than presented as though they were
not. A capability word says which fields are meaningful in this build and this run, so a reader
can tell "absent on this version" from "the self-check failed" from "genuinely zero" — three
conditions that today share one value.

And it holds enough history to be useful. The prototype's 11-entry young Ring laps in about
62 ms under churn; at a one-second poll an operator sees roughly 6% of their pauses. 512 entries
stretch that to about 2.9 seconds, for 49 KB per interpreter.

## 3. User stories

1. As an **operator on Linux running 3.14**, I want to `pip install` the Probe and have gcscope
   report pause times, so that I can see what my collector costs without building anything.
2. As an **operator on Apple Silicon**, I want the records I read to be intact rather than
   torn, so that a pause figure is a pause figure and not two halves of different Collections.
3. As an **operator on 3.13**, I want `heap_size` reported as *unavailable* rather than as `0`,
   so that I do not read a missing field as an empty heap.
4. As an **operator whose interpreter was already running**, I want `collections` to mean what
   it means everywhere else in gcscope, so that Coverage and Loss are computed against the
   interpreter's life and not against the moment I happened to import something.
5. As an **operator who upgraded CPython**, I want the Probe to refuse to load rather than
   publish plausible numbers against offsets it was not built for.
6. As a **developer profiling their own script**, I want installing the Probe to cost me
   nothing measurable when I am not reading it — the per-invocation overhead is already below
   the noise floor of the Collection it observes (§11), and it must stay there.
7. As a **gcscope maintainer**, I want the Ring layout asserted against the generated header
   rather than against copied literals, so that a registry change breaks a build here instead
   of a trace elsewhere.
8. As an **operator on a free-threaded build**, I want a clear refusal, so that I do not
   silently get a 1/1 Ring and a permanently zero `heap_size`.

## 4. Implementation decisions

### The move

`gcprobe/` becomes `gcscope_probe/` in this repo; `gcprobe/verify/` becomes a Cargo workspace
member. That second half is not cosmetic: `verify/Cargo.toml` depends on `gcscope` **by path**
(`../../gcscope`), and a path dependency cannot be published. Making it a workspace member
dissolves the problem rather than solving it. The alternative — publishing gcscope to crates.io
as a library purely so a sibling repo can depend on it — was rejected as a public contract
maintained for one internal consumer.

The prototype's research tooling does not come across: `peexp.py` established that no collector
symbol is exported and has no further job, and `offsets.c` plus all three `.bat` files are
deleted by the offsets decision below.

### Interpreter offsets: a second translation unit

`internals.c` is compiled **with** `Py_BUILD_CORE`, includes the internal headers, references
no Python data symbols, and exposes the three offsets. `gcscope_probe.c` is compiled without
`Py_BUILD_CORE` and calls into it. Both link into one module.

This is the prototype's own observation turned into structure — its `offsets.c` comment already
records that a TU "compiled WITH `Py_BUILD_CORE` but referencing no Python data symbols … links
without the dllimport/dllexport conflict". What was a separate executable becomes a second
object file.

Rejected: **generate a header by running a program at build time**, which is what the `.bat`
files do. It breaks under cross-compilation, and cross-compilation is not hypothetical here —
macOS arm64 wheels and any emulated Linux leg would have to execute a target-architecture
binary on the host. The second-TU form runs nothing at build time.

The include set differs across the two supported minors and needs a `PY_VERSION_HEX` split:
3.13 has `_gc_runtime_state` in `pycore_gc.h` and `PyInterpreterState` in `pycore_interp.h`;
3.14 consolidated both into `pycore_interp_structs.h`.

### Publication ordering

`gcprobe_add_stats` currently does `MemoryBarrier()` followed by a **plain** store to
`cur->ts_stop`. The entire torn-record protection rests on a reader never observing `ts_stop`
land before the payload fields it describes. On x86-64's TSO that holds. On aarch64 it does
not — a release *fence* followed by a plain store does not establish it.

`ts_stop` becomes an `atomic_store_explicit(…, memory_order_release)`, and slot claiming moves
from `InterlockedCompareExchange` to `atomic_compare_exchange_strong`. Both come from C11
`<stdatomic.h>`, which MSVC supports under `/std:c11`.

This is the one item on the list that is a **correctness fix rather than a port**. It is
invisible today only because the sole supported platform cannot exhibit it.

### The region header

Version 4. Two fields join it, and one moves into it:

- **`layout_digest`** — the `stats` digest emitted by
  [0012](0012-gen-offsets-serves-the-probe.md), declaring which registered Ring shape this
  Probe implements. Sizes cannot carry this: a field moving *within* the 64-byte entry leaves
  `item_size`, `region_size` and both entry counts unchanged.
- **`capabilities`** — one word carrying per-field validity (`heap_size` absent on 3.13,
  `candidates` always unobtainable), the seeding state, and the self-check result.
- **`offsets_ok`** currently lives only behind the Python-level `geometry()` method. An
  out-of-process reader cannot call it, so today a failed self-check is indistinguishable from
  success. It moves into `capabilities`.

The header is at `version = 3` with zero external consumers. Every one of these is free now and
a compatibility negotiation after the first wheel ships.

### Seeding

At install, the Probe reads CPython's own inline `generation_stats` and initialises
`collections`, `collected` and `uncollectable` from them. Those counters then *are* Lifetime
totals, and Coverage, Loss and every figure
[ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md) derives keep their
existing meanings with no new concept in the reader.

`duration` and `candidates` cannot be seeded — CPython never recorded the first and
`deduce_unreachable` is `static inline`, putting the second out of reach. They stay
Install-relative, and `capabilities` says so. The trap this guards against is specific and easy
to walk into: a Lifetime-total `collections` and an Install-relative `duration` sit in the same
64-byte entry, and averaging one against the other is silently wrong.

### Guards

- **Load gate**: minor must be 13 or 14. Free-threaded builds are refused outright rather than
  loaded with a 1/1 Ring and a `heap_size` that is never maintained.
- **Patch gate**: the runtime patch version is compared against the one the wheel was built
  against, via `Py_Version`. A mismatch refuses rather than publishes.
- **Self-check**: today it reads `collecting` inside a callback, where it must be 1, and thereby
  validates the `gc` and `collecting` offsets *jointly*. A `heap_size`-only move passes it and
  publishes garbage. `heap_size` gains its own validation, and failure is recorded in
  `capabilities` where a reader can see it.

### Ring depth

512 young, 128 old. The 11/3 default exists so the prototype's region would be byte-identical
to a Native one, which let it be tested against this repo's decoder without touching it.
[0014](0014-read-probe-regions.md) supersedes that: the reader takes geometry and layout digest
from the header on every attach, so a non-default depth costs nothing it does not already
handle. 393 KB across all eight interpreter slots is negligible against any process with a GC
problem worth measuring.

Consequence worth stating: this repo's **native** decode path hardcodes 11/3, so a Probe Ring is
readable only through the header's `young_entries`/`old_entries`. That path becomes the only way
a Probe region decodes, so it must be exercised deliberately rather than incidentally — see
[0014 §5](0014-read-probe-regions.md).

### Export visibility

`__declspec(dllexport)` becomes a macro that expands to `__attribute__((visibility("default")))`
on ELF and Mach-O. `PyMODINIT_FUNC` gets default visibility on its own; `gcscope_probe_header`
and the slot array do **not**, and will vanish from the dynamic symbol table if the build ever
picks up `-fvisibility=hidden`. The failure mode is discovery finding nothing, with no
diagnostic, so CI asserts the symbol is genuinely exported — see
[0015 §5](0015-publish-probe-wheels.md).

### Naming

Distribution `gcscope-probe`, import module `gcscope_probe`. The module *filename* becomes
contract-bound by [0014](0014-read-probe-regions.md), which matches on its prefix, so a later
rename silently breaks discovery in every released gcscope. That warrants a comment at the
declaration, not just a line in a spec.

## 5. Seams and testing decisions

- **Seam:** the region header and the Ring bytes, read from **outside** the process. The Probe's
  entire output is a memory region a reader decodes; asserting through the Python-level
  `geometry()` would test the one surface no real consumer uses.
- **New seam needed:** none for the out-of-process path. In-process tests use the existing
  `records()` and `geometry()` for liveness only, never as the correctness gate.
- **What makes a good test here:** assert the decoded **shape** and the declared capabilities,
  never that a read succeeded. A Probe with a wrong `heap_size` offset publishes a full table of
  plausible garbage and returns success at every level — [README §6](README.md#conventions).
- **Prior art:** `gcprobe/verify/src/main.rs`, which already checks that every written entry
  decodes, passes `is_complete()`, carries a positive duration, and that cumulative counters
  never regress between samples. It is promoted to a gcscope integration test by
  [0014](0014-read-probe-regions.md) rather than rewritten.
- **Cases:**
  1. Seeding: immediately after install, the Ring's `collections` equals `gc.get_stats()` for
     each generation — the property that makes them Lifetime totals.
  2. Capabilities: `heap_size` valid on 3.14, **absent** on 3.13; `candidates` always absent;
     the self-check result readable from outside the process.
  3. Ordering: on **native arm64**, under sustained churn, every entry a reader selects as
     newest is internally consistent. Emulation cannot substitute — it can mask reordering
     entirely, which would make a green leg prove nothing about the defect being fixed.
  4. Refusal: importing on 3.12, on 3.15, and on a free-threaded build raises `ImportError`
     naming the reason.
  5. Regression guard: per-invocation overhead against an empty young generation stays within
     the §11 measurement, so an operator who installs the Probe and never reads it pays what
     they paid before.

## 6. Out of scope

- **3.11 and 3.12.** `PyTime_PerfCounterRaw` and `PyTime_AsSecondsDouble` are public only from
  3.13; below that the clock must be hand-rolled per platform and proven to match CPython's
  perf counter semantics. Reversible later at the cost of that shim and nothing structural.
- **3.15 and later.** They publish a Native ring; a Probe there would be a second source for
  the same quantity with no benefit. The load gate enforces this rather than leaving it to
  wheel tags.
- **Free-threaded builds.** `heap_size` is never maintained there, the Ring geometry is 1/1, and
  the per-interpreter slot design is argued rather than exercised. Refused, not degraded.
- **Deeper-than-512 or runtime-configurable Ring depth.** Additive later; shipping a tunable
  before anyone has named a number is guessing at a distribution that can be measured instead.
- **The reader side entirely** — discovery, validation, precedence, operator messaging. That is
  [0014](0014-read-probe-regions.md).
- **Packaging and CI** — wheels, `cibuildwheel`, release. That is
  [0015](0015-publish-probe-wheels.md).
- **A threat model for a component that runs inside other people's production processes.** Named
  here because productizing raises it and this spec does not answer it; see §7.

## 7. Further notes

**Interaction with the monitoring tiers, which landed first.**
[ADR 0017](../docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md) gives a build with no
timing fields counter tracks rather than spans, Coverage `0`, and pause figures reported as
*absent*. A Probe makes that false on 3.13 and 3.14: there are Records, there is timing, and
Coverage is computable. The Probe track amends that ADR's sub-3.15 branch rather than leaving
two accounts of the same behaviour standing. This is the largest cross-track interaction in the
set.

**Untested paths carried over from the prototype**, none of which this spec closes: more than
eight concurrent interpreters (the ninth is dropped rather than corrupting, but that path has
never run), and genuinely concurrent sub-interpreters with separate GILs.

**Open question for when this is picked up.** The prototype exports a slot **array** and walks
it by interpreter id. With the depth increase, that array is 393 KB of BSS in every process that
imports the module, whether or not eight interpreters ever exist. Whether the slots should be
allocated on first claim instead is a real decision with a discovery consequence — `slots_addr`
would become a pointer to read rather than an address to use — and it was not settled in the
session that produced this spec.
