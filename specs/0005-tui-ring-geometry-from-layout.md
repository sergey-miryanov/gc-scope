# 0005 — Drive the TUI tree's entry geometry from the resolved layout

- **Status:** Not started
- **Kind:** bug — correctness (presentation)
- **Effort:** S
- **Origin:** 2026-07-18 review (findings C10 / A2) — four of five copies of the formula
  eliminated; this is the last one.
- **Respects:** [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md),
  [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md)

## 1. Problem

An operator inspecting a free-threaded interpreter (`3.15t`) in the TUI sees an
`_Py_DebugOffsets` tree whose GC entry subtree is wrong — nonsense entry sizes, and
young/old entry counts that do not match the build. Running `gc-stats` against the same
process at the same moment gives correct numbers. Two views of one interpreter disagree,
and nothing on screen says which to believe.

## 2. Evidence

`tui::tree::gen_stats_layout` derives the geometry arithmetically from the region size
alone:

```rust
let item_size = if gen_stats_size >= 24 { (gen_stats_size - 24) / 17 } else { 0 };
let young_bytes = 11 * item_size;
let old_bytes = 3 * item_size;
```

The `17` is `11 young + 3 + 3 old` and the multipliers are the GIL entry counts. A
free-threaded build publishes `[1, 1, 1]` (see
[spec 0004 §2](0004-free-threaded-validation-reporting.md) for the CPython side), so the
division yields a meaningless item size and every offset derived below it is wrong.

The offsets layer already computes this correctly *and* cross-checks it against the
process: `offsets::set_ring` and `expected_ring_size` select `[1,1,1]` versus `[11,3,3]`
from the `free_threaded` flag, and `PySession::verify_ring_stats_size` hard-errors if the
selection contradicts the published `gc.generation_stats_size`. The TUI recomputes from the
size alone and consults none of it — so the one copy without a cross-check is the one that
is wrong.

This is the surviving instance of a formula that once appeared at five sites. The
`gc-stats`, snapshot and TUI **data** paths were all converted to consume `OffsetTable`
geometry (ADR 0003, ADR 0007); only the tree's **label** geometry was left behind.

## 3. Scope

**Affected:** the `_Py_DebugOffsets` tree panel, in interactive `tui` and in
`tui --output <file>`, on free-threaded ring builds (3.15t and later).

**Not affected:** any decoded GC value, on any build. `GcEntry` parsing goes through
`snapshot::collect::parse_gc_entries`, which is layout-driven. GIL builds render correctly
today — the formula happens to agree with the layout there, which is exactly why this
survived four rounds of de-duplication.

**Why CI misses it:** the `3.15t` leg asserts shape through `gc-stats`, and `tests/tui.rs`
runs against whichever interpreter `GCSCOPE_TEST_PYTHON` selects — in practice a GIL build,
where the formula is right.

## 4. Proposed change

1. Change `gen_stats_layout` to take the entry counts and item size — or the resolved
   `GcItemLayout` / `OffsetTable` directly — rather than deriving them from
   `gen_stats_size`. Keep it a pure function so its existing unit test stays a unit test.
2. Pass what `set_ring` already resolved: the snapshot the renderer consumes carries the
   `OffsetTable`, so no new plumbing is needed to reach it.
3. Replace the positional seven-tuple return with a named struct in the same change. Every
   caller currently has to remember the order of seven `u64`s, which is its own latent
   defect.
4. Update `gen_stats_layout_derives_entry_geometry_from_the_region_size` — both its name
   and its expectations encode the contract being replaced.

## 5. Seams and testing decisions

- **Seam:** `gcscope::tui::render_snapshot` — the public frame-dump seam behind
  `tui --output`, already exercised by `tests/tui.rs` against a live interpreter. It is the
  highest seam that can observe this: the defect is visible in rendered text, and this is
  the one place rendered text is available as a value.
- **Second seam (already exists):** the pure `gen_stats_layout` unit test, for the
  arithmetic itself. Two seams rather than one is justified here — the unit test pins the
  geometry, the snapshot test proves the geometry actually reaches the screen. The bug
  survived precisely because only the first kind existed.
- **New seam needed:** none.
- **What makes a good test here:** assert the rendered **shape** — that the young/old entry
  labels and sizes match what the target's own build reports — not that rendering
  succeeded. A rendered tree full of wrong numbers is exactly as "successful" as a right
  one, which is the general trap ADR 0005 names.
- **Prior art:** `tests/tui.rs`, which already drives `collect_data` + `render_snapshot`
  against a live interpreter and asserts Full-tier content; and `common::is_free_threaded`,
  which is how the test learns which geometry to expect.
- **Cases:**
  1. Free-threaded target: entry labels read `(1)` and the entry size matches `gc-stats` on
     the same process. Fails today.
  2. GIL target: frame dump byte-identical to before the change — the regression guard, and
     the reason to capture a baseline before touching anything.
  3. Unit: `gen_stats_layout` for both `[11,3,3]` and `[1,1,1]`, plus the degenerate
     below-header case that currently returns item size 0.

## 6. Out of scope

Changing where `[11,3,3]` / `[1,1,1]` are **defined**. They stay hardcoded in the offsets
layer because CPython publishes no entry counts — see
[spec 0004 §6](0004-free-threaded-validation-reporting.md). This spec only stops the TUI
from keeping a second, unchecked copy of them.

## 7. Further notes

Once this lands, the phrase "one ring position is an entry, never a slot" holds uniformly
across the code — `gen_stats_layout` is the last place the old geometry vocabulary is
implied by the arithmetic even where the names have been updated.
