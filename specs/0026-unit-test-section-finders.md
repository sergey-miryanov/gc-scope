# 0026 — Unit-test the three `PyRuntime` section finders

- **Status:** Not started — shares its fixtures with
  [0023](0023-unit-test-symbol-resolution.md)
- **Kind:** feature — cleanup
- **Effort:** M standalone; S if [0023](0023-unit-test-symbol-resolution.md) lands first
- **Origin:** review of `src/memory/process.rs`, 2026-08-09
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

`find_section_in_elf`, `find_section_in_pe` and `find_section_in_macho` are the whole 3.13+
finding path — `try_find_runtime` classifies the image, dispatches to one of them, and
validates whatever address comes back against the `"xdebugpy"` cookie. Each is pure:
`(bytes, region_start) -> Option<u64>`, no pid, no filesystem, no process.

`src/memory/process.rs` has **no `#[cfg(test)]` module at all**. Their only coverage is
`tests/memory.rs::find_runtime_locates_the_cookie`, which is `--ignored`, gated on 3.13+, and
— the part that matters — exercises exactly **one** of the three on any given machine, because
a Linux leg only ever parses ELF and a macOS leg only ever parses Mach-O.

The accurate complaint is sharper than "untested": each finder is tested on one third of the
matrix, by a test that cannot run without a live 3.13+ interpreter, and several arms inside
them have never executed anywhere.

- **`find_section_in_elf` accepts an undotted `PyRuntime`** alongside `.PyRuntime`, and its
  own comment says why: CPython's `GENERATE_DEBUG_SECTION` macro dots it on Linux, so the
  undotted arm exists "for any toolchain that drops it". No toolchain in the matrix does. The
  branch is speculation, and speculation with no fixture is speculation with no test.
- **`find_section_in_pe` accepts `PyRuntime` alongside the truncated `PyRuntim`.** PE section
  names are eight bytes, so the untruncated spelling is likewise never what a real image
  carries.
- **`find_section_in_macho` discards the fat-slice offset on purpose** — `let (macho, _)`,
  with a comment that section addresses are virtual so the offset is not needed. Its sibling
  `ro_data_range` *uses* that same offset because it returns file bytes — today in
  `remote_debugging::version`, one layer up; after
  [0022](0022-image-lookup-into-memory-binary.md), in `memory::binary`, next door. Two
  functions, one value, opposite correct answers, and nothing states the rule.
- **`find_section_in_macho` depends on segment order.** `text_vmaddr` is captured in the same
  loop that looks for `__DATA`, so a `__DATA` segment appearing before `__TEXT` yields `None`
  through `text_vmaddr?`. Real images put `__TEXT` first; nothing says so.
- **A single malformed name aborts the whole Mach-O walk.** `segment.name().ok()?` and
  `sect.0.name().ok()?` return from the function, not from the iteration, so one bad segment
  loses the ones after it.

All of it is the ADR 0004 hazard: a base or bias computed wrong lands in a mapped region and
returns an address nothing can tell from a good one. The cookie check catches it in practice,
which is why this is a cleanup rather than a bug. It catches it as "no PyRuntime section in
process N", minutes into a CI job, on whichever leg happens to parse that format.

## 2. Solution

The three finders get unit tests built on synthetic images, sharing the builders
[0023](0023-unit-test-symbol-resolution.md) needs. Every format then gets checked on every
machine, rather than the one format a given CI leg happens to parse. The live matrix keeps the
job it alone can do: showing that a real CPython image on a real platform has the shape
gcscope assumes.

## 3. User stories

1. As a gcscope maintainer on Windows touching image parsing, I want an ELF or Mach-O mistake
   to fail in `cargo test` on my machine, so that I am not relying on a leg I cannot run to
   tell me about the two formats I never see.
2. As a gcscope maintainer, I want the "section addresses are virtual, so the fat-slice offset
   is *not* added" rule pinned against a fat fixture, so that the note explaining it has a
   test behind it and the merge with `ro_data_range` that
   [0023 §6](0023-unit-test-symbol-resolution.md#6-out-of-scope) defers has something to be
   checked against.
3. As a gcscope maintainer, I want the undotted-ELF and untruncated-PE arms exercised, so that
   branches kept for a toolchain nobody has are known to work rather than assumed to.
4. As an operator on a build whose image is unusual, I want a finder that cannot locate the
   section to return `None` rather than an address, so that the cookie check is a second line
   of defence and not the only one.

## 4. Implementation decisions

**The builders move to a shared `#[cfg(test)]` module first.** They live today in
`binary.rs`'s private `mod tests`, out of reach of any other module's tests. Declare
`#[cfg(test)] pub(crate) mod testimage;` under `memory`, move `Shape`, `thin_macho`,
`thin_macho_with_entry_point` and `fat_macho` into it, and have both `binary`'s and
`process`'s test modules use it. It compiles only under `cfg(test)`, so this is a test-module
helper and not a `test-hooks` gate — the distinction
[ADR 0005](../docs/adr/0005-testing-strategy.md) draws.

Whichever of this spec, [0023](0023-unit-test-symbol-resolution.md) and
[0032](0032-fat-slice-arch-selection.md) lands first does that move, as its own commit, with
no other change in it.

**Fixture work, on top of what `thin_macho` emits today** (one `__TEXT` segment, `nsects: 0`):

- **Mach-O:** sections under a segment, and a `__DATA` segment carrying a `PyRuntime` section.
  0023 wants `LC_SYMTAB` and a `__cstring` section under `__TEXT` from the same builder, which
  is the second reason the two specs share a module.
- **ELF:** the builder 0023 introduces (one `PT_LOAD`, symbol and string tables, `.rodata`)
  plus a `.PyRuntime` section header, and the section-header string table entries to name it.
- **PE:** 0023's builder (DOS stub, PE header, one section, export directory) plus a section
  named `PyRuntim`.

**Assert the resolved address, never that resolution succeeded.** Each expected value is
computed by hand from the fixture's own numbers: `region_start + (sh_addr - load_bias)` for
ELF, `region_start + virtual_address` for PE, `region_start + (sect.addr - __TEXT.vmaddr)` for
Mach-O. Every fixture uses a **non-zero** `region_start`, a **non-zero** load bias, a
`__TEXT.vmaddr` that is not the base, and for the fat case a non-zero `slice_at` — a fixture
with those at zero passes under every arithmetic error this exists to catch (convention 6).

**Rejected — testing through `try_find_runtime`.** It is the natural caller and it needs a
pid, a real module list and a readable cookie. The finders are pure so they do not
need any of that; going through the caller would put the whole point behind a live process.

**Rejected — one shared parameterized test over the three formats.** The three take different
fixtures, compute different arithmetic and have different never-run arms. A table-driven test
would assert the intersection, which is the part that already works.

## 5. Seams and testing decisions

- **Seam:** a new in-file `#[cfg(test)]` module in `src/memory/process.rs`, over the shared
  `memory::testimage` builders. Highest seam that can stage an image a real interpreter would
  not produce; the next one up is `try_find_runtime`, which needs a pid.
- **New seam needed:** none in production code. One new `cfg(test)`-only module, and it is a
  relocation of helpers that already exist.
- **What makes a good test here:** a fixture where a *plausible wrong implementation* gives a
  different address — the fat Mach-O with a non-zero `slice_at` is the sharp one, since adding
  the offset is exactly what the sibling function correctly does.
- **Prior art:** `macho_entry_point_math_is_safe` with `thin_macho`/`fat_macho` — same module
  family, same genre of hazard, same builders.
- **Cases:**
  1. **ELF:** `.PyRuntime` resolves to `region_start + (sh_addr - load_bias)` with a non-zero,
     non-page-aligned bias.
  2. **ELF:** the undotted `PyRuntime` spelling resolves identically — the arm that has never
     run.
  3. **ELF:** no matching section ⇒ `None`; a section whose `sh_addr` is below the load bias
     wraps rather than panicking (the `wrapping_sub` is deliberate).
  4. **PE:** `PyRuntim` resolves to `region_start + virtual_address`.
  5. **PE:** the untruncated `PyRuntime` resolves identically — the other never-run arm.
  6. **PE:** no matching section ⇒ `None`.
  7. **Mach-O:** `__DATA,PyRuntime` resolves to `region_start + (addr - __TEXT.vmaddr)`, and
     the same via `__DATA_CONST` and `__AUTH_CONST`, which an arm64 system Python carries.
  8. **Mach-O, fat:** the address is the **same** as for the thin fixture despite a non-zero
     `slice_at`. The rule the comment states, and the one a future merge with `ro_data_range`
     is most likely to break.
  9. **Mach-O:** `__DATA` before `__TEXT` ⇒ `None`. Pins the order dependence as current
     behavior; see §6.
  10. **Mach-O:** no `PyRuntime` section, and no `__TEXT` segment, each ⇒ `None`.
  11. **Shared negatives:** bytes of the wrong format for the finder called ⇒ `None`; a
      truncated image ⇒ `None`, not a panic.
  12. Regression guard: `tests/memory.rs::find_runtime_locates_the_cookie` and the 3.13+ live
      legs are unchanged, and remain the only evidence a *real* image has this shape.

## 6. Out of scope

- **Changing any finder's behavior.** Case 9 pins the segment-order dependence; case 3 pins
  the wrapping arithmetic. If either is judged wrong, that is a bugfix spec, not a quiet fix
  inside a test commit. Both currently fail closed — `None`, then the next module, then a
  clear error — which is why neither is urgent.
- **Merging `find_section_in_*` with `version::ro_data_range`.** Named in
  [0022 §1](0022-image-lookup-into-memory-binary.md#1-problem-statement) and deferred in
  [0023 §6](0023-unit-test-symbol-resolution.md#6-out-of-scope). This spec plus 0023 are what
  make the merge checkable; doing it here means the tests and the change they guard land
  together, which proves nothing.
- **Moving the finders into `memory::binary`.** They arguably belong there beside
  `resolve_symbol` and `classify`, but a move plus new tests in one change is the same
  problem. Test them where they are; move them later against green tests, if at all.
- **`search_pid_and_children` and `try_find_runtime`.** Different technique — closures, not
  fixtures. See §7.
- **Reducing the live matrix.** Additional evidence, not a replacement.

## 7. Further notes

**A second, separate split exists in this file and is not this spec.**
`search_pid_and_children` is policy over two IO calls — try self, stop at `MAX_DEPTH`, recurse
into children, aggregate each child's error — and it would split the same way
[0024](0024-detect-policy-takes-its-reads.md) and
[0025](0025-interpreter-scan-takes-its-reads.md) do, taking `try_find_runtime` and
`get_child_pids` as closures. Two things a test would then state: the depth cap, and that a
leaf process calls `try_find_runtime` **twice** — once at the top and once more when
`children.is_empty()`, so every module is read from disk a second time to produce the error
message. Neither is wrong; both are invisible. Worth its own spec if the depth-3 recursion is
ever touched.

**One asymmetry worth knowing while reading `try_find_runtime`.** It propagates
`validate_cookie`'s read error with `?`, so an unreadable candidate address ends the module
loop, where its pre-3.13 sibling accumulates a `tried` line and continues. No supported build
reaches it — the computed address lies inside the module's own mapping — so this is a note
about the shape of the code, not a defect, and it is IO either way and so outside what this
spec can test.
