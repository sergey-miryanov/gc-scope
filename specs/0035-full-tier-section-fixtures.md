# 0035 — Unit-test the Full-tier section builders

- **Status:** Not started — reads best after [0034](0034-split-tui-sections.md)
- **Kind:** feature — cleanup
- **Effort:** M
- **Origin:** review of `src/tui/sections.rs`, 2026-08-10
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md)

## 1. Problem statement

`section_debug_offsets` and `section_interpreter` are roughly 425 lines of pure rendering —
`(data, offsets, flags) -> Vec<Line<'static>>` — and they draw the view the `tui` and
`tui --output` subcommands exist to produce.

They are not untested. `tests/tui.rs::tui_frame_renders_the_full_tier_sections_on_a_live_interpreter`
sweeps the tree/hex/runtime-hex flag combinations through `render_snapshot` and asserts the
frame is coherent. That is the only thing reaching them, and it is `--ignored`, needs
ptrace/taskport, is gated on a 3.13+ interpreter, and asserts a whole frame. A regression
inside one builder surfaces, if at all, as a whole-frame assertion failing on a leg that
happens to have the right Python. No test names either builder's output.

The codebase says why, in two places. `tui/test_support.rs`:

> The pre-3.13 (`Legacy`) tier is the only one constructible without a live process (flat
> `OffsetTable`, no bindgen struct), so `legacy_data` is the synthetic snapshot every section
> builder's tests render against.

and `layout.rs`, on `render_snapshot`:

> Always compiled so both `run_tui_snapshot` and the integration tests can reach it through
> the public API — in particular the **Full-tier** section builders … that only run against a
> real 3.13+ `_Py_DebugOffsets` struct and so can't be reached from the synthetic-Legacy unit
> tests.

**That reason no longer holds.** `remote_debugging::offsets`' own test module builds every
`VersionedOffsets` variant with no Python: `build_all()` allocates a 64 KB zeroed buffer and
reads each `LAYOUTS` constructor against gcscope's own process (`std::process::id()`,
`buf.as_ptr() as u64`), the same self-read the `-1` PID convention uses. A Full-tier
`VersionedOffsets` is constructible in a unit test today. The builder is private to that
module's `#[cfg(test)] mod tests`, so `tui` cannot see it.

That is the same lock-in [0023](0023-unit-test-symbol-resolution.md),
[0026](0026-unit-test-section-finders.md) and [0032](0032-fat-slice-arch-selection.md) hit
with `thin_macho`/`fat_macho` in `memory::binary` — and `tui::test_support` is already the
crate's answer to it, one module over.

## 2. Solution

Share the variant builder, add a `full_data` fixture beside `legacy_data`, and give each
Full-tier builder unit tests that name it.

## 3. User stories

1. As a maintainer changing the offsets tree rendering, I want a failing test that names the
   builder, so that I learn which section broke rather than that a frame stopped cohering.
2. As a maintainer without a 3.13+ interpreter to hand, I want the Full-tier rendering
   reachable in `cargo test`, so that the tier gcscope is mostly *for* is not the tier only CI
   can exercise.
3. As a maintainer adding a section or a flag, I want a fixture that already produces a
   Full-tier snapshot, so that the cost of covering the new thing is one test rather than one
   test plus an afternoon.
4. As a CI job, I want the live frame test to stay exactly as it is, so that synthetic
   fixtures never become the thing gcscope is validated against.

## 4. Implementation decisions

**Share the variant builder, don't duplicate it.** Expose the zeroed-self-read constructor
from `remote_debugging::offsets` as a `#[cfg(test)] pub(crate)` helper, and have
`offsets`' own tests keep using it. Duplicating a 64 KB self-read into `tui` would be a second
copy of a trick that is subtle enough to deserve one home.

**That exposure is a move, and it is its own commit.** `build_all` lives inside
`offsets/mod.rs`'s private `#[cfg(test)] mod tests`, which is the same lock-in this spec
diagnoses — `pub(crate)` on it is not enough while it sits there, so it has to be lifted into
a `#[cfg(test)] pub(crate)` module of its own. Land that lift with nothing else in it, exactly
as [0023 §4](0023-unit-test-symbol-resolution.md#4-implementation-decisions) and
[0026 §4](0026-unit-test-section-finders.md#4-implementation-decisions) require for
`memory::testimage`, so `offsets`' own tests are visibly unchanged by it.

**`tui::test_support` grows `full_data`, beside `legacy_data`.** Its module doc already
states the fixture contract for the whole TUI test suite ("kept in one place so the
`frame`/`layout`/`sections`/`gc_view` test modules can't drift"); this extends it rather than
adding a second convention. `frame`, `layout` and `gc_view` get the Full tier available for
free, which is the second reason to put it there.

**What the fixture can and cannot prove — say it in the fixture's doc comment.** A zeroed
buffer yields all-zero offsets, so these tests assert rendering **shape**: which rows appear,
their labels and ordering, which blocks each flag adds or omits, how an absent field renders.
They cannot assert that any offset *value* is right — that is
[ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md)'s territory and belongs to the
`offsets` tests and the live matrix. A test asserting a zero renders as `0` would be pinning
the fixture, not the renderer.

**Pick a variant deliberately and say which.** The tests should name one `LAYOUTS` row rather
than "whichever comes first", so that adding a version does not silently change what they
render. Where a case is about a *tier* difference rather than a version difference, assert
against two variants explicitly.

**Rejected — a `test-hooks` gate to build a Full-tier `CollectedData`.** ADR 0005 prefers an
honest signal to a gate, and none is needed: the constructor already exists under `cfg(test)`
and nothing about it has to ship.

**Rejected — deleting or narrowing the live frame test.** It is the only evidence a *real*
`_Py_DebugOffsets` renders correctly, and a zeroed struct cannot replace it. These tests sit
beside it.

## 5. Seams and testing decisions

- **Seam:** the section builders themselves, unit-tested in their own files (after
  [0034](0034-split-tui-sections.md), one test module each). They are pure functions of
  `(CollectedData, VersionedOffsets, flags)`; nothing above them adds information.
- **New seam needed:** none in production code — one `#[cfg(test)] pub(crate)` helper and one
  fixture function, both compiled out of any release build.
- **What makes a good test here:** assert a *named* row or a row count that differs between
  flag settings. "It rendered some lines" passes when a whole block is missing, which is the
  regression worth catching in a 200-line builder.
- **Prior art:** `tui::test_support::legacy_data` and the `gc_view`/`layout` tests built on
  it — the same arrangement, one tier up; and `offsets::build_all`, the constructor being
  shared.
- **Cases:**
  1. `section_debug_offsets` with `show_tree` on and off: the field-tree block is present in
     one and absent in the other, and the rest of the section is unchanged.
  2. The same for `show_hex` and `show_runtime_hex`, including both hex blocks on at once —
     the combination `tests/tui.rs` sweeps but cannot attribute to a builder.
  3. `section_interpreter` emits the GC-state box, and its header names a byte count
     consistent with the fixture's `gc.size` rather than a fixed cap.
  4. Tier dispatch: the same `CollectedData` with `Some(offsets)` and `None` produces the Full
     and Legacy sections respectively, and the Legacy one carries the
     "pre-3.13: no `_Py_DebugOffsets` struct" note. Pins the `match` in `layout.rs`.
  5. An entry the layout does not carry renders as absent rather than as a zero row — the
     rendering half of the "absent fields decode to `None`, not zero" rule
     `offset_table.rs::absent_fields_decode_to_none_not_zero` already pins one layer down.
  6. Regression guard: `tests/tui.rs::tui_frame_renders_the_full_tier_sections_on_a_live_interpreter`
     unchanged and green, and `offsets`' own tests unchanged by the helper being shared.

## 6. Out of scope

- **Splitting `sections.rs`.** [0034](0034-split-tui-sections.md), first, so these tests land
  in files named after what they cover.
- **Asserting offset values.** Reasoning in §4. The fixture is zeroed by construction.
- **[0005](0005-tui-ring-geometry-from-layout.md).** The open correctness bug in the tree's
  entry geometry on free-threaded targets is about a wrong constant, not a wrong shape; a
  zeroed fixture would not have caught it and this spec does not claim otherwise. Fix it on
  its own evidence.
- **`frame.rs`'s interactive loop.** Terminal IO, covered by the live tests; only the
  builders are in scope.
- **Making `memory`'s builders shareable.** Same pattern, different module —
  [0023](0023-unit-test-symbol-resolution.md) and
  [0026](0026-unit-test-section-finders.md) own that move.

## 7. Further notes

Worth noting for whoever does the `memory::testimage` move in
[0023](0023-unit-test-symbol-resolution.md)/[0026](0026-unit-test-section-finders.md)/[0032](0032-fat-slice-arch-selection.md):
`tui::test_support` is the same idea, already in the tree, already carrying a doc comment that
explains why one shared fixture module beats per-file copies. Four specs now want that shape;
it is worth stating once, somewhere both can point at, rather than four times.

The stale comments are part of the change. `tui/test_support.rs`'s header and `layout.rs`'s
note on `render_snapshot` both assert that the Full tier is unreachable without a live
process. Once it is reachable, those sentences are wrong in a way that would send the next
reader back down this same path — update them in the same commit.
