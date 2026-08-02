# 0010 — Pre-3.13 offsets stay hand-maintained

**Status:** Accepted — decided 2026-08-02. (Records a deliberate non-action and its trigger,
in the shape of [ADR 0009](0009-performance-guarded-by-shape.md). Complements
[ADR 0002](0002-version-split-runtime-finding.md),
[ADR 0003](0003-layout-driven-gc-stats-decode.md) and
[ADR 0006](0006-layout-registration-integrity.md).)

## Context

gcscope resolves offsets two ways, and the split is visible in the source: `v_*.rs` for 3.13+,
`pre_3_13.rs` for 3.8–3.12. The recurring question is whether that asymmetry should be removed
by generating the pre-3.13 tables from CPython source too — extending
`scripts/gen-offsets.py` down to 3.8 and deleting the hand-transcribed numbers.

The premise behind the question is that the two eras differ in **how the numbers were
authored**. They do not. They differ in **where the numbers come from at runtime**:

| Build | Compiled in | Read from the live process |
|---|---|---|
| 3.13+ | the *shape* of `_Py_DebugOffsets` — which field sits where in the block | the offset **values** |
| 3.8 – 3.12 | the offset **values** | nothing |

This is the whole point of PEP 768. On 3.13+ the target reports its own `offsetof()` results,
so gcscope is correct for a debug build, a 32-bit build, a vendor build with a patched struct —
without knowing any of that in advance. Before 3.13 the interpreter publishes nothing, so the
values must be compiled in and are correct only for the build configuration they were taken
from.

**Generation cannot close that gap.** It would relocate the transcription into a script and
leave a static table behind, with the same blindness to build configuration. Making pre-3.13
self-describing would require code running inside the target, which is the constraint the
project is built on.

What generation would cost, against that:

- **The header-reconstruction trick does not generalize.** `compute_inline_stats_off`
  reconstructs `_gc_runtime_state` by scraping four small structs out of the headers and
  forward-declaring `PyObject` / `_PyInterpreterFrame` as opaque. That works because
  `_gc_runtime_state` refers to its dependencies **by pointer**. `PyInterpreterState` and
  `_PyRuntimeState` embed dozens of types **by value** — config, ceval, unicode, GIL and
  thread state — and an embedded struct cannot be forward-declared. Reconstructing them means
  pulling in most of `Include/internal` for each of five versions.
- **The tractable alternative is worse, not better.** Compiling a C probe that prints
  `offsetof()` against a real build tree works, but needs a built CPython **per version per
  platform ABI** at generation time, and still emits numbers that get frozen into a static
  table. Same output, much larger machine.
- **The set is closed.** 3.8–3.12 will never gain a new minor version. There is no future
  invocation to amortize a generator over: it would automate a job that is already finished.

And the thing being replaced is small. `pre_3_13.rs` is 254 lines, ~100 of them tests. It holds
**four distinct rows** (3.10 delegates to 3.9 verbatim) of **nine offsets each**, plus two
constants shared with 3.13/3.14. Every one of those numbers is exercised end-to-end on every
push by the live matrix — 3 OSes × 3.8–3.12 — which is the only instrument that can catch a
wrong offset at all, since a wrong one executes exactly the same lines as a right one.

## Decision

1. **Pre-3.13 offsets stay hand-transcribed.** The asymmetry with `v_*.rs` is intrinsic to the
   two eras, not an artifact of how the tables were written, and removing it is not achievable
   by generation. `version-support.md` §4 describes the asymmetry; this ADR records that it was
   weighed and kept.
2. **The live matrix is the guard, and it must stay unpinned.** CI resolves
   `python-version: '3.12'` through `setup-python`, which floats to the newest patch release.
   That float is what would catch a patch-release struct reshuffle in 3.11 or 3.12 (supported
   until Oct 2027 / Oct 2028) — the residual risk a static table carries. **Pinning an exact
   patch version in the smoke matrix silently disables this guard** and must not be done for
   the pre-3.13 legs without replacing it.
3. **Non-goals stated explicitly.** Neither the current design nor a generator handles a
   `--with-pydebug` or 32-bit pre-3.13 interpreter; both fail open. gcscope supports
   release-configuration 64-bit builds before 3.13, and that is a property of the era, not a
   bug to be fixed by changing how the table is authored.
4. **Triggers to revisit** — any one of them reopens this:
   - A live leg goes red on a pre-3.13 patch release, i.e. the structs turn out to move within
     a minor after all. That changes the set from closed to drifting.
   - The pre-3.13 live coverage narrows (a leg dropped, or the matrix pinned per point 2). The
     hand table's only validation is that coverage; without it, the positional nine-argument
     `table()` constructor should become a named-field struct so transposition is
     compiler-visible — the tradeoff already recorded in the comment at `pre_3_13.rs:24-33`.
   - Support is asked for a pre-3.13 build configuration that the single static table cannot
     describe. The answer there is a per-configuration table or a refusal, still not generation.

## Consequences

- The two-era split in `offsets/` is documented as intentional. A reader who notices
  `pre_3_13.rs` and wonders why it was not generated gets an answer here instead of
  re-deriving one.
- The live matrix acquires a stated second job. It was already the correctness gate
  ([ADR 0005](0005-testing-strategy.md)); it is now also the *only* thing standing between a
  pre-3.13 patch release and a silent misread, which is why point 2 is a constraint on the
  workflow rather than a note.
- Effort stays on 3.13+, where versions keep arriving and where
  [ADR 0006](0006-layout-registration-integrity.md)'s registration guards are the live
  maintenance burden.
- If CPython ever backports a `_Py_DebugOffsets`-equivalent to a maintenance branch, that
  reopens the question on much better terms than generation — the values would become readable
  from the target, which is the property that actually matters.
