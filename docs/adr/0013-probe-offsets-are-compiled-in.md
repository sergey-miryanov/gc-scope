# 0013 — Probe offsets are compiled in, not registered

**Status:** Accepted — decided 2026-08-06. (Records why the Probe does *not* reuse the
machinery of [ADR 0006](0006-layout-registration-integrity.md),
[ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md) and
[ADR 0011](0011-layout-equivalence-sweep.md), which is the first question anyone asks on
seeing the two side by side.)

## Context

gcscope carries a registry of interpreter offsets, generated from CPython source, swept
weekly against every patch release, and resolved exact-or-refuse. That machinery is not
optional: the reader is **outside** the process, so it cannot compile against the interpreter
it will meet and must be able to describe every build it might encounter.

The Probe is in the opposite position, and the difference is not a matter of taste:

| | Reader | Probe |
|---|---|---|
| Position | Outside the process | Inside it |
| Knows its target at build time | No — any 3.8–3.16 build | Yes — one minor, one platform ABI |
| Therefore offsets must be | Looked up at runtime | Compiled in |

A wheel is built per interpreter minor per platform tag. The internal headers of *that*
interpreter are present when it is compiled. So the offsets the Probe needs —
`offsetof(PyInterpreterState, gc)`, and `heap_size` and `collecting` within
`_gc_runtime_state` — are available as compile-time constants of the exact target, which is
strictly better information than a registry lookup can provide.

Three postures were weighed:

| | How offsets are fixed | Guards patch drift | Cost |
|---|---|---|---|
| **A** | Build time, per wheel | Needs a runtime guard | The wheel matrix |
| **B** | Build time, sdist only — every user compiles | By construction | A C toolchain on every user's machine |
| **C** | Runtime, from a gcscope-style registry | Per patch version | The full sweep burden, plus fields the reader does not use |

**B** is the most correct and was rejected on distribution grounds: the Probe is meant to go
into production applications and slim container images, where "install a compiler" ends the
conversation.

**C** was rejected because it buys a guarantee the position does not need. It would also
enlarge the registry with `collecting`, a field no reader dereferences, to serve a consumer
that already knows its own target exactly.

**A** carries one real risk, and the prototype documents it: `sizeof(_gc_runtime_state)` moved
240 → 264 between 3.14.4 and 3.14.5. Internal struct layout is explicitly outside CPython's
ABI stability promise, so a wheel built against 3.14.5 and installed on 3.14.9 is unguarded by
anything the wheel tag enforces. That is a runtime-guard problem, not an argument for a
registry — a registry would have needed regenerating for that release too.

There is a second fork inside A, and it matters more than it looks. Deriving offsets at build
time can mean **running** a generated program — which is what the prototype's `.bat` files do
— or **compiling** them into a second translation unit. Running one breaks under
cross-compilation, and cross-compilation is not hypothetical: macOS arm64 wheels and any
emulated Linux leg would have to execute a target-architecture binary on the host. The
prototype's own `offsets.c` records the property that makes the second form work: a TU
"compiled WITH `Py_BUILD_CORE` but referencing no Python data symbols … links without the
dllimport/dllexport conflict".

## Decision

1. **Probe offsets are compiled in, from a second translation unit.** `internals.c` is
   compiled with `Py_BUILD_CORE`, includes the internal headers, references no Python data
   symbols, and exposes the offsets to a main TU compiled without it. Nothing is executed at
   build time, so cross-compilation is unaffected.

   > **Amended 2026-08-07, building it.** The `7400` this replaced was a Windows number; the
   > same 3.14 puts `gc` at **7408** on Linux. Every Linux Probe built before this read
   > `collecting` from the wrong address, failed its own self-check and published
   > `heap_size = 0`, which the integration test accepted because 0 passes a magnitude check.
   > So the first thing compiled-in offsets bought was a working Linux Probe, not
   > patch-release safety.
   >
   > `internals.c` also asserts that `heap_size` is still a `Py_ssize_t` and `collecting` still
   > an `int`. An offset survives a retype; the read does not, and transcribed constants had no
   > way to notice.
   >
   > Correct offsets forced decision 5's free-threaded refusal to land here rather than later.
   > `heap_size` exists in a `Py_GIL_DISABLED` build and `gc_free_threading.c` never writes it.
   > While the offsets were wrong a free-threaded build failed its self-check and looked broken,
   > accidentally but visibly. Getting them right removes that accident: measured on 3.14.7t,
   > the Probe reported `offsets_ok 1` with `heap_size 0` on every Record. `PyInit` now refuses
   > at import, as this decision always said it should.
   >
   > Decision 3 is only partly delivered. The self-check still validates `gc` and `collecting`
   > jointly, and its result is still reachable only from inside the process. `heap_size` gained
   > an out-of-process floor in `tests/probe.rs` rather than its own in-process validation. The
   > patch gate and `capabilities` remain ahead.
2. **The registry is not extended to serve the Probe.** [ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md)
   and [ADR 0011](0011-layout-equivalence-sweep.md) continue to describe the reader's needs
   only. The asymmetry is intrinsic to the two positions and is recorded here rather than
   removed.
3. **The runtime guards carry the residual risk**, since the wheel tag pins only the minor:
   - The self-check must validate `heap_size`, not only `collecting`. Reading `collecting`
     inside a callback validates the `gc` and `collecting` offsets *jointly* and leaves a
     `heap_size`-only move undetected — the prototype has this hole.
   - A runtime patch version the wheel was not built against is refused, not published
     against, in the shape of [ADR 0012](0012-version-detection-fails-closed.md).
   - The self-check result is published in the region header, where an out-of-process reader
     can see it. In the prototype it is reachable only from inside the process, which is the
     one place it is not needed.
4. **The sweep is extended as early warning, not as a source of truth.**
   `scripts/gen-offsets.py --sweep` gains `offsetof(PyInterpreterState, gc)`, `heap_size` and
   `collecting`, so a maintainer learns from a red Monday job that a Probe rebuild is due,
   rather than from a user. The swept values never feed a build.
5. **Non-goals, stated.** Debug builds, 32-bit builds and free-threaded builds are out of
   scope; the last is refused at import rather than degraded.
6. **Triggers to revisit:**
   - A patch release moves one of the three offsets within a supported minor. That turns a
     rebuild-on-notice policy into a treadmill, and the registry becomes worth its cost.
   - Support is wanted below 3.13, where the per-minor wheel count grows and the argument
     from "one wheel already knows its target" weakens with every added row.
   - CPython publishes these offsets the way PEP 768 publishes the reader's. That would make
     the values readable from the target and reopen the question on far better terms — the
     same trigger [ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md) names.

## Consequences

- The prototype's hardcoded `7400 / 216 / 192`, its `offsets.c`, and all three `.bat` files
  are deleted rather than ported. The offsets stop being transcribed and start being
  compiled.
- A reader who notices that the Probe ignores the registry gets an answer here instead of
  re-deriving one, and — importantly — does not "fix" it.
- [ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md) rejected generation partly because
  a C program printing `offsetof()` "needs a built CPython per version per platform ABI". The
  Probe *is* built per version per platform ABI, so that objection does not transfer. The two
  decisions are consistent because the positions differ, not because either overturned the
  other.
- The weekly sweep acquires a second audience. A red sweep now means either "the reader's
  registry is incomplete" or "a Probe wheel needs rebuilding", and its output must say which.
