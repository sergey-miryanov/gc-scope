# 0022 — Move image symbol and read-only-section lookup into `memory::binary`

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/remote_debugging/version.rs`, 2026-08-09
- **Respects:** [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md),
  [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md)

## 1. Problem statement

Nothing changes for an operator. ADR 0008's layer boundary does not hold here, and it breaks
in the most expensive place.

`memory::process::find_runtime_pre_3_13` resolves the `_PyRuntime` symbol by calling
`crate::remote_debugging::version::resolve_symbol_in_bytes`. Neither `_PyRuntime` nor the
function has anything to do with versions: it takes a name and a base address and dispatches
on the image's magic bytes. So L1 imports L2 to do L1's own job, and the arrow ADR 0008 drew
(`memory → remote_debugging → …`) points backwards for the whole pre-3.13 finding path.

The second half is the same shape. `version::ro_data_range` locates a named section per
binary format (`.rdata` / `.rodata` / `__TEXT,__cstring`), which is the job
`memory::process::find_section_in_elf` / `_pe` / `_macho` already does for `PyRuntime`, one
layer down. Two per-format section walks, two layers apart, neither aware of the other. Both
carry the ADR 0004 hazards: the Mach-O fat-slice rebase that `ro_data_range` remembers
(`slice_at + sect.offset`) is the class of mistake that fails *open*.

The practical cost is [0023](0023-unit-test-symbol-resolution.md): the three symbol
resolvers have no unit tests at all, and the image builders that could test them
(`thin_macho`, `fat_macho`) are private to `memory::binary`'s test module and out of
`version.rs`'s reach.

## 2. Solution

Image parsing lives in `memory::binary` with the rest of image parsing. `version` keeps the
version-domain composition — "look in the read-only data section first, then the whole
image" — and calls down for both primitives. `memory::process` stops importing `version`.

## 3. User stories

1. As a gcscope maintainer, I want the per-format image walks in one module, so that a
   platform fact learned the hard way (Mach-O's underscore prefix, the fat-slice rebase, the
   `PyRuntim` truncation on PE) is recorded once where the next walk will find it.
2. As a gcscope maintainer, I want `memory::process` to stop importing `version`, so that the
   one plainly inverted import ADR 0008 forbids is gone from the pre-3.13 path. Two L2 imports
   remain and §4 says why; this story is not "`memory` compiles without `remote_debugging`".
3. As a maintainer writing tests for symbol resolution, I want the resolvers in the module
   that owns the image builders, so that testing them does not require making a test helper
   public across a layer boundary.
4. As an operator attaching to a 3.8–3.12 interpreter, I want `find-runtime` to resolve
   `_PyRuntime` exactly as it does today, so that the only path to a pre-3.13 runtime is not
   disturbed.
5. As the `scan_image_for_version` fuzz target, I want my entry point unmoved, so that the
   Linux fuzz leg needs no edit.

## 4. Implementation decisions

Two functions move into `src/memory/binary.rs`, renamed to drop the qualifier that only made
sense next to a grammar:

```rust
// memory::binary
pub fn resolve_symbol(bytes: &[u8], base_addr: usize, name: &str) -> Option<u64>;
pub fn read_only_data(bytes: &[u8]) -> Option<&[u8]>;
```

`resolve_symbol_elf` / `_pe` / `_macho` and `ro_data_range` move with them as private
helpers. Their doc comments move verbatim — the Mach-O underscore-prefix note and the
fat-slice rebase note are the ADR 0004 record for those two walks.

**Call sites, all of them:**

- `memory::process::find_runtime_pre_3_13` → `binary::resolve_symbol`, and
  `src/memory/process.rs` drops `version` from its `use crate::remote_debugging::{…}` list.
- `remote_debugging::version::detect` → `binary::resolve_symbol` for `Py_Version`.
- `remote_debugging::version::scan_image_for_version` → `binary::read_only_data`.

**`scan_image_for_version` stays in `version`.** It is the version-domain policy — which
region to search and in what order, with the whole-image pass as the fallback so no build
regresses — expressed over two `binary` primitives. Keeping it put also keeps the fuzz
target's path (`gcscope::remote_debugging::version::scan_image_for_version`) valid, which is
worth something on its own.

**`version::resolve_symbol_in_bytes` is deleted, not re-exported.** Nothing outside the crate
calls it: `tests/remote_debugging.rs` uses `detect`, `scan_image_for_version` and
`PythonVersion::from_hex`, and nothing else. A compatibility shim would preserve the inverted
arrow under a new name.

**This spec does not straighten the arrow completely.** `src/memory/process.rs` also imports
`check_interpreter` and `offsets::offset_table::OffsetTable`, because `find_runtime_pre_3_13`
validates a candidate with the structural cross-reference ADR 0002 chose. That function is
arguably a consumer sitting in L1, and relocating it carries live-matrix risk. This spec
removes the one L2 import that is plainly misplaced, and claims nothing beyond that.

**Rejected — converging the two section walks now.** After the move,
`binary::ro_data_range` and `process::find_section_in_*` are neighbours differing in the
section they name and what they return (a file range versus a load address). Unifying them is
worth doing and is also a chance to get a base address wrong, which ADR 0004 says fails open.
Land the move first, with the resolvers under test ([0023](0023-unit-test-symbol-resolution.md)),
and judge the merge against tests that exist.

## 5. Seams and testing decisions

- **Seam:** `memory::binary`'s existing `#[cfg(test)]` module for the moved code, and
  `tests/remote_debugging.rs` (live, `--ignored`) for the composition that stays behind. Both
  already exist; this spec adds no seam and is chosen partly for that.
- **New seam needed:** none.
- **What makes a good test here:** this change must be observably empty, so the assertion is
  that the live matrix still decodes the *same shape* — `find-runtime` on a 3.8–3.12 target
  still yields a `_PyRuntime` that passes the interpreters_head cross-reference, and `detect`
  still agrees with `python --version` on every leg. A test asserting only that resolution
  "succeeded" would pass with a base address off by a slice offset.
- **Prior art:** ADR 0008's own reorg, where `exporters/` moved with zero internal edits
  because its cross-references were `super::`-relative; and
  `tests/remote_debugging.rs::detect_matches_the_interpreter_version`.
- **Cases:**
  1. `cargo build` succeeds with `src/memory/process.rs` no longer naming `version` — the
     property the spec buys, and the compiler checks it.
  2. Regression guard: the live smoke matrix (3 OS × 3.8–3.15t) is unchanged. macOS and the
     pre-3.13 legs are the ones that matter; the fat-slice rebase in `ro_data_range` and the
     underscore-prefix acceptance in `resolve_symbol_macho` are only exercised there today.
  3. Regression guard: the fuzz target compiles unedited.

## 6. Out of scope

- **Unit tests for the moved resolvers.** [0023](0023-unit-test-symbol-resolution.md) — it is
  the payoff, but it is a distinct, larger change (the image builders need a symbol table),
  and keeping it out is what makes this one a reviewable move.
- **Merging the two per-format section walks.** Reasoning above.
- **Relocating `find_runtime_pre_3_13` out of `memory`.** Reasoning above.
- **Any change to what is searched or in what order.** `read_only_data` first, whole image
  second, `Py_Version` before the scan: all preserved.

## 7. Further notes

Order against [0021](0021-version-grammar-module.md) does not matter; the two touch disjoint
symbols. Doing 0021 first makes this one's diff smaller to read, since what remains in
`version/mod.rs` is then only what this spec is emptying.
