# 0023 — Unit-test the three symbol resolvers and the read-only-section lookup

- **Status:** Not started — blocked on [0022](0022-image-lookup-into-memory-binary.md)
- **Kind:** feature — cleanup
- **Effort:** M
- **Origin:** review of `src/remote_debugging/version.rs`, 2026-08-09
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

`resolve_symbol_elf`, `resolve_symbol_pe`, `resolve_symbol_macho` and `ro_data_range` have
**no unit tests**. Each computes an address from a per-platform fact that ADR 0004 says must
be verified rather than inferred, and each fails *open* when the fact is wrong: the read lands
in a mapped region and returns garbage.

They are covered only by the live matrix, which means:

- a wrong base or slice offset is caught on one CI leg, minutes into a job, on the platform
  that happens to run it — and on macOS, on a leg that needs root for 3.8–3.12;
- the failure modes that cannot be produced on demand are not covered at all. A real
  interpreter does not stage an undefined `Py_Version` symbol, a symbol present only in
  `.symtab` and not `.dynsym`, or a `__cstring` section in the second slice of a fat file.
  Those are the arms nothing exercises;
- `resolve_symbol_macho`'s `is_undefined` guard is the sharpest example. An undefined symbol
  carries `n_value == 0`, so dropping the guard yields `base - text_vmaddr`: a plausible
  address, in a mapped region, decoding into a full table of garbage that nothing would catch.

## 2. Solution

Each resolver gets unit tests built on synthetic images, beside the builders that already
exist for this. `cargo test` proves the address arithmetic in under a second, with no Python
and no CI leg. The live matrix keeps the job it alone can do: showing that a real CPython
image on a real platform has the shape gcscope assumes.

## 3. User stories

1. As a gcscope maintainer touching image parsing, I want a wrong base address to fail in
   `cargo test` on my machine, so that I learn about it before pushing rather than from a
   macOS leg twenty minutes later.
2. As a gcscope maintainer, I want the fat-slice rebase and the underscore-prefix acceptance
   pinned by a test, so that a later cleanup that merges the two per-format section walks
   ([0022 §6](0022-image-lookup-into-memory-binary.md#6-out-of-scope)) has something to be
   checked against.
3. As a CI job, I want to keep the live matrix as the arbiter of *real* image shape, so that
   synthetic fixtures never become what gcscope is validated against.
4. As a gcscope maintainer, I want the undefined-symbol and section-absent paths covered, so
   that a resolver returns `None` rather than an address nothing can distinguish from a good
   one.

## 4. Implementation decisions

All of this lands in `memory::binary`'s existing `#[cfg(test)]` module — which is why
[0022](0022-image-lookup-into-memory-binary.md) comes first. `thin_macho` and `fat_macho` are
private to it, and reaching them from `remote_debugging::version` would mean making test
helpers public across a layer boundary.

**The builders themselves move to a shared `#[cfg(test)] pub(crate) mod testimage` under
`memory`**, as a first commit with nothing else in it.
[0026](0026-unit-test-section-finders.md) extends the same builders for the `PyRuntime`
section finders in `memory::process`, and [0032](0032-fat-slice-arch-selection.md) extends
them again with a second architecture; a helper private to one module's test mod cannot be
reached from another's. Whichever of the three specs lands first does the move.

**Assert the resolved address, never that resolution succeeded.** Each fixture is built with
known values, so the expected address is computed by hand in the test:
`base + (st_value - load_bias)` for ELF, `base + rva` for PE,
`base + (n_value - __TEXT.vmaddr)` for Mach-O. `assert!(…is_some())` would pass with every
base-math bug this spec exists to catch (convention 6).

**Land it as three commits, one per format, in this order.** Each is independently useful and
the first is the cheapest and the riskiest:

1. **Mach-O.** `thin_macho` exists; it needs an `LC_SYMTAB` (with a small symbol/string table)
   and an optional `__cstring` section under `__TEXT` — today it emits `nsects: 0`. Cases: the
   `_`-prefixed and undecorated spellings both resolve; an undefined symbol is skipped rather
   than resolved to `base - vmaddr`; a symbol absent entirely yields `None`; via `fat_macho`,
   `read_only_data` returns bytes at `slice_at + sect.offset` rather than at `sect.offset`
   (the fails-open rebase).
2. **ELF.** New builder: a minimal ELF64 with one `PT_LOAD`, a `.dynsym`/`.dynstr` pair, a
   `.symtab`/`.strtab` pair and a `.rodata` section header. Cases: a dynamic symbol resolves
   through the load bias; a symbol present only in `.symtab` still resolves (the second loop,
   which no real CPython build reaches); a non-page-aligned `PT_LOAD` vaddr is biased down
   correctly; `read_only_data` returns the `.rodata` range.
3. **PE.** New builder: DOS stub, PE header, one section, an export directory. Cases: an
   exported name resolves to `base + rva`; an absent name yields `None`; `read_only_data`
   returns the `.rdata` range.

**Shared negatives, written once against all three:** bytes `classify` does not recognize
yield `None`; a truncated image yields `None` rather than panicking; a section-range whose
declared size runs past the buffer is clamped (`read_only_data`'s `min(bytes.len())`), which
is the guard the fuzz target's near-misses land on.

**Rejected — a real image as fixture.** Using `std::env::current_exe()` gives a genuine
host-format image with a symbol table, and it is tempting because it costs nothing to write.
It tests one format per platform, cannot stage any of the failure modes above, and makes a
unit test depend on the filesystem and on how the test binary was linked. The live matrix
already covers "a real image works", better.

**Rejected — an image-writer dev-dependency.** `object::write` emits relocatable object
files, which have no `PT_LOAD` and so cannot exercise `elf_load_bias` at all. The precedent
for a read-only dev-dependency (`serde_json`, for parsing traces gcscope writes by hand)
does not extend to a crate that cannot produce the shape under test.

## 5. Seams and testing decisions

- **Seam:** `memory::binary`'s in-file `#[cfg(test)]` module, using its own image builders.
  This is the highest seam that can stage a malformed or adversarial image — above it, no
  caller can hand these functions bytes that a real interpreter would not have produced.
- **New seam needed:** none in the production code. The builders are test-module helpers, not
  a `test-hooks` gate, which is the distinction ADR 0005 draws.
- **What makes a good test here:** an exact expected address derived from the fixture's own
  numbers, plus a fixture where a plausible wrong implementation gives a *different* address —
  a non-zero `slice_at`, a non-zero load bias, a `__TEXT.vmaddr` that is not the base. A
  fixture with everything at zero passes under every arithmetic error.
- **Prior art:** `macho_entry_point_math_is_safe` and its `thin_macho`/`fat_macho` fixtures —
  same module, same genre of hazard, and the note in
  `image_scan_survives_adversarial_bytes` explaining why random bytes cannot reach past
  `MachO::parse` is the reason builders are the right tool here.
- **Cases:** the per-format lists in §4, plus the shared negatives.

## 6. Out of scope

- **Merging the two per-format section walks** (`ro_data_range` versus
  `memory::process::find_section_in_*`). This spec is what makes that merge checkable; doing
  both at once means the tests and the change they guard land together, which proves nothing.
- **Changing any resolver's behavior.** If a case here documents something surprising, pin
  the current behavior and raise it separately. A behavior change found while writing tests is
  a bugfix spec.
- **Fuzzing the resolvers.** The existing `scan_image_for_version` target already reaches them
  through `scan_image_for_version`; whether they deserve a target of their own is a separate
  judgement, and the ADR 0005 note that goblin rejects random bytes applies just as much here.
- **Reducing the live matrix.** These tests are additional evidence, not a replacement. The
  live legs are the only thing that proves a real CPython image has the assumed shape.

## 7. Further notes

If the Mach-O commit alone turns out to be the bulk of the effort, split this into one spec
per format rather than letting the ELF and PE builders wait behind a large first commit.

Write the `is_undefined` case first, before the builders are complete. The current code is
right there for a reason no reader would guess, and a test is the only place to record it
where a "simplification" will trip over it.
