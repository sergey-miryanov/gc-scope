# 0012 — Emit the ring layout for the Probe, and sweep the fields it compiles in

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** S
- **Origin:** Grilling session 2026-08-06 on productizing the Probe (`X:/Work/gc-monitor/gcprobe`,
  moving into this repo as `gcscope_probe/` — see [0013](0013-probe-portable-core.md)). This
  is the piece of that work that touches only `scripts/gen-offsets.py`, and it lands first
  because [0013](0013-probe-portable-core.md) asserts against the header this emits.
- **Respects:** [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md) (decode keyed
  by layout, not version), [ADR 0006](../docs/adr/0006-layout-registration-integrity.md)
  (registration fails closed), [ADR 0011](../docs/adr/0011-layout-equivalence-sweep.md)
  (layout equivalence is swept from source, not assumed)

## 1. Problem statement

A gcscope maintainer cannot tell whether the Probe and gcscope agree about the shape of a
Ring, and cannot learn that a CPython patch release moved a field the Probe compiles in
until a user reports wrong numbers.

Both gaps have the same root: `gen-offsets.py` currently serves one consumer. It emits Rust
for the reader and sweeps the fields the reader dereferences. The Probe is a second consumer
with an overlapping but not identical set of needs, and nothing connects them.

**The Ring layout is transcribed, not shared.** The Probe reproduces CPython 3.15's
`struct gc_stats` byte for byte so that this repo's decoder reads it unmodified. Today the
Probe encodes that shape as literals copied out of gcscope by hand, guarded by its own
`Py_BUILD_ASSERT`s. Those assertions are a check against *the copy*, not against the source
the copy came from, so a change to the registered 3.15 layout leaves them passing.

**The failure that follows is invisible.** If a field moves *within* the 64-byte entry, the
item size, the region size and both entry counts are unchanged. Every geometry check
available to the reader still passes, and every number is misattributed to the wrong field.
This is the characteristic bug of §6 in
[`docs/version-support.md`](../docs/version-support.md) — a wrong offset that executes the
same lines as a right one — arriving through a new door.

**The interpreter fields the Probe compiles in are unswept.** The Probe reads
`offsetof(PyInterpreterState, gc)`, and within `_gc_runtime_state` the `heap_size` and
`collecting` offsets. The reader uses none of these, so `offsets-sweep.yml` does not watch
them. They are not hypothetical: `sizeof(_gc_runtime_state)` already moved 240 → 264 between
3.14.4 and 3.14.5. The offsets survived that reshuffle, but only because someone checked.

## 2. Solution

The Ring layout has one source, and the Probe's fields are watched by the same weekly job
that watches the reader's.

`gen-offsets.py` gains a second emitter. Alongside the Rust it already writes, it emits a C
header describing the same registered layout — the entry field offsets, the item size, the
inter-generation padding, and the layout digest that identifies which registered shape it
describes. The Probe includes that header and asserts against it at compile time, so a
divergence between the two becomes a failed build in this repo rather than misattributed
numbers in someone's trace.

The weekly sweep gains three fields. `offsetof(PyInterpreterState, gc)`, `heap_size` and
`collecting` are compared across every patch release of every shipped minor, exactly as the
reader's fields already are. A maintainer learns from a red Monday job that a Probe rebuild
is needed, instead of learning it from a user.

Nothing changes for an operator. This spec is entirely upstream of anything a Probe or a
reader does at runtime.

## 3. User stories

1. As a **gcscope maintainer**, I want the Probe's Ring layout to come from the same
   generator as the reader's, so that the two cannot silently disagree about where a field
   sits.
2. As a **gcscope maintainer**, I want a compile-time failure when the registered layout
   changes under the Probe, so that I find out at build time rather than from a trace.
3. As a **gcscope maintainer**, I want the weekly sweep to cover the interpreter fields the
   Probe compiles in, so that a patch-release reshuffle is caught by CI rather than by a
   user on 3.14.9.
4. As a **gcscope maintainer adding a Python version**, I want the Probe's needs visible in
   the same script I already run, so that I do not have to remember a second procedure.
5. As an **operator**, I want nothing about this to reach me — no new output, no new flag,
   no behaviour change on any interpreter.

## 4. Implementation decisions

**A second emitter, not a second script.** The C header is produced by the same run that
produces the Rust, from the same parsed layout, so the two cannot be regenerated
independently and drift. A separate script sharing a parser is the rejected alternative: it
doubles the number of things a maintainer must remember to run, which is the failure mode
this spec exists to close.

**The header is a checked-in artifact.** A Probe wheel for 3.13 is built in an environment
that has 3.13 headers and no 3.15 source tree, so it cannot run `gen-offsets.py` at build
time. The header is generated when the registered layout changes, committed, and guarded by
the sweep — the same posture `src/remote_debugging/offsets/` already takes toward its
generated Rust.

**What the header carries**, trimmed to the decision:

```c
/* Generated by scripts/gen-offsets.py — do not edit. */
#define GCSCOPE_RING_LAYOUT_DIGEST "<12-hex>"   /* the `stats` digest, §below */
#define GCSCOPE_RING_ITEM_SIZE     64
#define GCSCOPE_RING_OFF_TS_START   0
#define GCSCOPE_RING_OFF_TS_STOP    8
/* … one per field … */
#define GCSCOPE_RING_CURSOR_BYTES   8           /* the trailing per-buffer index */
```

Field offsets rather than a struct definition: the Probe declares its own struct for its own
reasons (a widened cursor, configurable depths), and what must be asserted is the offsets it
produces, not the declaration it uses to produce them.

**The digest already exists.** `gen-offsets.py` computes a `stats` key — a 12-hex SHA-256 of
the `gc_stats` struct block, produced by `_gc_stats_struct` — and the sweep already uses it
for layout equivalence. This spec exposes that value to the C side and to
[0014](0014-read-probe-regions.md), where a Probe declares it at runtime and the reader
matches it against the registry. No new identity scheme is introduced; an existing one gains
two consumers.

**Which layout the header describes.** The registry holds more than one 3.15 shape today —
`v_3_15_0b1::GC_LAYOUT` and `v_3_15_0b1_gcinc::GC_LAYOUT`. The header describes exactly one,
named explicitly in the generator invocation rather than inferred as "the newest", because
"newest" changes under a maintainer's feet when a new pre-release registers. When 3.15.0
final registers, pointing the header at it is a deliberate one-line change plus a Probe
rebuild.

**Sweep extension.** `--sweep` already exports `Include/` per release tag and compares
computed offsets. The three Probe fields join the compared set. They are reported in the same
table and fail the job the same way; there is no separate Probe sweep and no separate
schedule. Rejected: a second scheduled workflow. The cost of the sweep is the clone and the
per-tag export, both of which are already paid.

## 5. Seams and testing decisions

- **Seam:** `scripts/gen-offsets.py`'s own output, compared against a checked-in expectation.
  This is the highest seam available — the script is the unit, its output is the contract,
  and both consumers read that output rather than the script's internals.
- **New seam needed:** none. `offsets-sweep.yml` already runs the generator on PRs that touch
  `scripts/gen-offsets.py`, which is the trigger this needs.
- **What makes a good test here:** assert the emitted *values*, not that emission succeeded.
  A header that generates cleanly and describes the wrong layout is exactly the failure this
  spec exists to prevent, and it is indistinguishable from success at the "did it run" level.
- **Prior art:** the existing sweep comparison in `offsets-sweep.yml`, and the
  `matrix-unpinned` job — a CI leg whose whole job is to assert that a guard has not been
  silently disabled.
- **Cases:**
  1. Regenerating from an unchanged tree reproduces the committed header byte for byte — so a
     stale checked-in artifact is detectable, not merely possible.
  2. A tree whose `gc_stats` block differs produces a different digest and different offsets,
     and the sweep reports the tag as uncovered.
  3. The three added interpreter fields are present in the sweep output for every shipped
     minor, including 3.13 — where `heap_size` **does not exist** and must be reported as
     absent rather than as `0`.
  4. Regression guard: the emitted Rust is byte-identical to what the same tree produced
     before this change. Nothing about the reader's path moves.

## 6. Out of scope

- **Any change to the reader's decode path.** [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md)
  and [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) stand untouched; this spec
  adds an emitter and three swept fields, and moves no decode logic.
- **Emitting offsets for pre-3.13 interpreters.**
  [ADR 0010](../docs/adr/0010-pre-3-13-offsets-stay-hand-maintained.md) decided that era stays
  hand-maintained, and the Probe does not support it — see
  [0013 §6](0013-probe-portable-core.md).
- **Generating the Probe's own struct declaration.** The Probe declares its struct; this
  emits the offsets that declaration must satisfy. Generating the declaration would move a
  build-time decision (ring depth) into a generator that has no business knowing it.
- **Runtime layout negotiation.** A Probe declaring a digest the reader does not recognise is
  [0014](0014-read-probe-regions.md)'s problem, and it refuses rather than negotiates.

## 7. Further notes

**Open question to settle when this is picked up:** whether the emitted header lives under
`gcscope_probe/` (consumed as a local include) or under a shared `include/` at the repo root.
The second reads better if anything else ever needs the layout in C; the first keeps the
generated artifact next to its only consumer. Decide when [0013](0013-probe-portable-core.md)
establishes the directory.

**On the asymmetry with [ADR 0010](../docs/adr/0010-pre-3-13-offsets-stay-hand-maintained.md).**
That ADR rejected generating pre-3.13 offsets partly because a C program printing `offsetof()`
"needs a built CPython per version per platform ABI". The Probe *is* built per version per
platform ABI — that is what a wheel is — so the objection does not transfer. The two
decisions are consistent because the positions differ, not because one overturned the other;
this is recorded in [ADR 0013](../docs/adr/0013-probe-offsets-are-compiled-in.md).
