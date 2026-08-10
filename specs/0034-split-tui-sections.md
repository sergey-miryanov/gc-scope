# 0034 — Split `tui::sections` along the boundary it already documents

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/tui/sections.rs`, 2026-08-10
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

Nothing changes for an operator. `src/tui/sections.rs` holds two unrelated renderings in 526
lines, and the file's own header already describes them as two:

> section 1 renders the struct as a version-correct field tree beside its hexdump; section 2
> renders the focused `PyInterpreterState` + GC-state box. Pre-3.13 has no such struct, so it
> gets the stripped `section_interpreter_legacy` header instead.

Four functions live there, and the two large ones share nothing but the file:

| Function | Size | Section |
|---|---|---|
| `section_debug_offsets` | ~225 lines | 1 — the `_Py_DebugOffsets` tree + hexdump |
| `section_interpreter_legacy` | ~15 lines | 2, Legacy tier |
| `section_interpreter` | ~200 lines | 2, Full tier |
| `debug_tree_row_color` | ~10 lines | 1's tree helper |

The practical cost is [0035](0035-full-tier-section-fixtures.md): that spec adds unit tests
for the two Full-tier builders, and adding them to a file this size means a test module whose
subject has to be inferred from each test's name. Splitting first puts each builder's tests
next to it.

## 2. Solution

`sections.rs` becomes `sections/mod.rs` + one file per section, following the `offsets/` and
`gc_view/` precedent already in the tree. No logic is edited.

## 3. User stories

1. As a maintainer changing how the offsets tree renders, I want to open a file that contains
   the offsets tree and nothing else, so that the 200 lines of interpreter rendering are not
   in my way.
2. As a maintainer reviewing [0035](0035-full-tier-section-fixtures.md), I want its tests to
   arrive in files named after what they test, so that the diff reads as coverage rather than
   as a wall.
3. As a maintainer reviewing *this* change, I want it to be a move with no edited logic, so
   that a behaviour change would stand out.

## 4. Implementation decisions

```
src/tui/sections/mod.rs             the module doc comment + re-exports
src/tui/sections/debug_offsets.rs   section_debug_offsets, debug_tree_row_color
src/tui/sections/interpreter.rs     section_interpreter, section_interpreter_legacy
```

**Split by *section*, not one file per function.** `layout.rs` chooses between the two
interpreter builders in a single `match` on `Option<&VersionedOffsets>` — `Some(off)` takes
`section_interpreter`, `None` takes `section_interpreter_legacy`. They are two tiers of one
section and one decision; separating them scatters that decision across two files to buy a
16-line file. The section boundary is also the one the module header already draws, so the
split follows a line someone has already thought about.

**`debug_tree_row_color` goes with section 1.** It colours the tree rows
`section_debug_offsets` emits and has no other caller. Its existing test moves with it.

**`mod.rs` re-exports, so no call site changes:** `pub(super) use debug_offsets::…;` and
`pub(super) use interpreter::…;`. That keeps `layout.rs`'s
`use super::sections::{section_debug_offsets, section_interpreter, section_interpreter_legacy}`
resolving untouched, the same device [0021 §4](0021-version-grammar-module.md#4-implementation-decisions)
uses to keep `version::PythonVersion` at its path.

**The three builders become `pub(in crate::tui)`, not `pub(super)`.** This is the one edit in
the whole spec that is not a move, and it is forced. `pub(super)` inside
`sections/interpreter.rs` means "visible to `sections`", so a `pub(super) use` in `mod.rs`
re-exporting it out to `tui` would be wider than the item, which rustc rejects (`E0364`, with
`E0603` following at the `layout.rs` call site). `gc_view` already solved this and is the
pattern to copy verbatim: `compact.rs` declares `pub(in crate::tui) fn section_gc_stats`, and
`gc_view/mod.rs` re-exports it with `pub(super) use`. `debug_tree_row_color` stays private,
since it does not cross the boundary.

**No logic is edited.** Not a line of rendering, not a doc comment, not a test body. The only
permitted edits are the `mod`/`use` lines, the two visibility changes above, and whatever
`use super::*` adjustment the two moved test modules need.

**Rejected — moving the tests into a single `sections/tests.rs`.** The two existing tests
belong to specific functions and the crate keeps unit tests in the file they cover. A shared
test file would recreate the problem this spec is solving.

## 5. Seams and testing decisions

- **Seam:** the existing in-file `#[cfg(test)]` modules, moved with the code they cover.
- **New seam needed:** none. This spec exists so [0035](0035-full-tier-section-fixtures.md)
  does not need one either.
- **What makes a good test here:** for a pure move, that the existing tests pass unedited and
  none was deleted or weakened — two of them, plus the live-tier coverage below.
- **Prior art:** the `gc_view/` split in the same module (`mod.rs` + `compact.rs` + `full.rs`,
  each with its own test module), which is this shape already.
- **Cases:**
  1. `section_interpreter_legacy_names_the_interpreter_and_flags_the_missing_struct` and
     `debug_tree_row_color_shades_named_fields_and_every_first_entry_item` pass unedited in
     their new homes.
  2. `layout.rs` compiles with its `use super::sections::{…}` line unchanged — the property
     the re-export buys, and the compiler checks it.
  3. Regression guard: `tests/tui.rs::tui_frame_renders_the_full_tier_sections_on_a_live_interpreter`
     is unchanged and still green. It sweeps the tree/hex/runtime-hex flag combinations
     through `render_snapshot`, so it is the only thing today that watches both moved
     builders render.

## 6. Out of scope

- **Unit tests for the Full-tier builders.** [0035](0035-full-tier-section-fixtures.md) — the
  payoff, kept out so this stays a reviewable move.
- **Any rendering change.** Column widths, labels and ordering are pinned by the live frame
  test; this touches none of them.
- **`gc_view`, `layout` or `tree`.** Untouched. The GC generation-stats table already lives in
  `gc_view`, which is why `sections` is two things rather than three.

## 7. Further notes

Do it as `git mv sections.rs sections/mod.rs` followed by a commit that cuts the two files out
of it, so `git blame` follows the file and each extraction reads as a deletion-plus-addition
pair rather than a rewrite — the same recipe [0021 §7](0021-version-grammar-module.md#7-further-notes)
gives.
