# 0032 — Make the fat-slice architecture selection assertable

- **Status:** Not started — touches the same fixture as
  [0023](0023-unit-test-symbol-resolution.md) and [0026](0026-unit-test-section-finders.md)
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/memory/binary.rs`, 2026-08-10
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

macOS ships Python as a `universal2` framework, so on that platform *every* image gcscope
parses is a fat binary and `binary::parse_macho` has to pick a slice. It picks the host's:

```rust
let want = if cfg!(target_arch = "aarch64") {
    goblin::mach::cputype::CPU_TYPE_ARM64
} else {
    goblin::mach::cputype::CPU_TYPE_X86_64
};
for arch in fat.iter_arches() {
    let Ok(arch) = arch else { break };
    if arch.cputype != want { continue; }
    …
}
```

Every macOS address gcscope computes comes out of the slice this loop chooses. Choosing the
other one yields a well-formed `MachO` whose `vmaddr`s and symbol values belong to an
architecture the target is not running — the
[ADR 0004](../docs/adr/0004-per-platform-image-layout.md) failure mode, arriving as a section
address that is merely wrong rather than obviously so.

**Nothing asserts the selection, because the fixture cannot.** `fat_macho` computes the
cputype it stamps with *the same `cfg!` expression* the production code uses to choose:

```rust
fn fat_macho(thin: &[u8], slice_at: usize) -> Vec<u8> {
    let want = if cfg!(target_arch = "aarch64") { CPU_TYPE_ARM64 } else { CPU_TYPE_X86_64 };
    …
    b.extend(1u32.to_be_bytes());   // nfat_arch — always exactly one
```

Code and fixture agree by construction, whatever either says. And because every fat fixture
carries **one** architecture, the `cputype != want` arm never runs against a table where
skipping matters. So **a `parse_macho` that ignored `cputype` and took the first arch would
pass the whole suite.** Eight tests exercise fat parsing; none exercises the choice.

The live matrix does not close the gap either. All three legs run `macos-latest`, which on
GitHub's current images is Apple Silicon, so CI executes the `aarch64` arm. No test and no CI
leg runs the `else` arm that selects `CPU_TYPE_X86_64`.

## 2. Solution

The wanted architecture becomes an argument. `parse_macho` keeps its signature and supplies
the host's; the selection is then a function a test can call with an explicit cputype, against
a fixture holding more than one.

## 3. User stories

1. As a gcscope maintainer on any machine, I want "the slice matching the requested cputype is
   the one returned" asserted, so that the rule survives a simplification that reads like a
   no-op.
2. As a gcscope maintainer, I want the x86_64 arm exercised somewhere, so that the branch no
   CI leg runs is not also the branch no test runs.
3. As an operator on an Intel Mac, I want slice selection to keep working exactly as it does —
   which today rests on the arm that has the least evidence behind it.
4. As a maintainer reading `parse_macho`, I want the host lookup to be one named thing rather
   than an expression inline in a loop, so that what is a *platform fact* and what is *logic*
   are visibly different.

## 4. Implementation decisions

```rust
// memory::binary
pub fn parse_macho(bytes: &[u8]) -> Option<(goblin::mach::MachO<'_>, usize)> {
    parse_macho_for(bytes, host_cputype())
}

fn parse_macho_for(bytes: &[u8], want: u32) -> Option<(goblin::mach::MachO<'_>, usize)>;
fn host_cputype() -> u32;   // the sole `cfg!(target_arch)` site
```

The public entry point, its doc comment and every production call site are unchanged. This is
the same move [0031](0031-module-base-selection-is-testable.md) makes for the base-mapping
rule, for the same reason: a `cfg!` branch inside a shared function compiles everywhere and is
exercised only on the host, so the fact has to become a parameter before a test can state it.

**The fixture must stop deriving the cputype the way the code does.** `fat_macho` takes the
cputype explicitly, and a new builder takes several slices:

```rust
fn fat_macho_multi(slices: &[(u32, &[u8])], first_slice_at: usize) -> Vec<u8>;
fn fat_macho(thin: &[u8], slice_at: usize) -> Vec<u8>;   // one-arch wrapper, explicit cputype
```

Existing fat tests keep working through the wrapper and pass a constant, so code and fixture
agree because the *test* says so — which is the whole difference.

**`host_cputype`'s own test is host-dependent, and that is acceptable.** It asserts a
two-branch mapping and nothing else; all the behaviour worth testing sits in
`parse_macho_for`, which is now reachable with either value on any machine. Label it as such
rather than pretending otherwise.

**Rejected — making `parse_macho` public with the cputype parameter.** No production caller
wants a non-host slice, and a public knob invites one. `parse_macho_for` stays private,
reachable from the in-file test module.

**Rejected — reading the target process's actual architecture instead of the host's.** gcscope
reads a process on the machine it runs on, and the existing doc comment says so; making the
slice follow the target rather than the host would be a behaviour change with its own
justification (translated processes under Rosetta), not part of a coverage fix. See §6.

## 5. Seams and testing decisions

- **Seam:** `parse_macho_for`, in `memory::binary`'s existing `#[cfg(test)]` module. The
  highest seam that can observe the choice — `parse_macho` above it is pinned to the host by
  design.
- **New seam needed:** yes, and it is one private function plus a fixture parameter.
- **What makes a good test here:** a fat table where the wanted arch is **not** first and the
  two slices are **distinguishable**. A two-arch fixture whose slices are byte-identical
  passes under a wrong selection; give the slices different `__TEXT` vmaddrs so the returned
  `MachO` says which one was chosen.
- **Prior art:** `parse_macho_unwraps_a_universal_binary_and_reports_the_slice_offset` — the
  test this spec is repairing, which pins the *offset* correctly and the *choice* not at all.
- **Cases:**
  1. Two arches, the wanted one **second** ⇒ its slice is parsed and its offset reported. The
     case that dies the moment someone takes the first arch, and the one no fixture reaches
     today.
  2. Two arches, the wanted one **first** ⇒ the first. Guards the mirror error of taking the
     last match.
  3. The wanted arch is **absent** ⇒ `None`. Returning some other architecture's slice would
     be the fails-open outcome; refusing is what the caller's error message depends on.
  4. Both values work: run cases 1–3 once with `CPU_TYPE_ARM64` wanted and once with
     `CPU_TYPE_X86_64`, on every machine. This is the property the spec buys.
  5. A matching arch whose `offset + size` runs past the buffer ⇒ `None`, pinning that the
     bounds check applies after the cputype match rather than falling through to another
     entry.
  6. `host_cputype()` maps aarch64 to `CPU_TYPE_ARM64` and anything else to
     `CPU_TYPE_X86_64` — the one host-dependent assertion, and deliberately trivial.
  7. Regression: the eight existing fat/thin `parse_macho` tests pass unedited except for the
     explicit cputype argument, and the adversarial cases
     (`parse_macho_refuses_a_fat_header_claiming_impossible_arch_count`,
     `the_guard_does_not_skip_its_walk_at_goblins_bound`) are untouched.
  8. Live matrix green, macOS especially — still the only evidence that the host mapping is
     right for a real `universal2` framework image, which no fixture establishes.

## 6. Out of scope

- **Changing which slice is chosen.** This pins host-arch selection. Following the *target's*
  architecture instead — which would matter for a process translated by Rosetta — is a
  behaviour change needing its own evidence, and would want a live leg that can produce one.
- **The bounds and adversarial guards around the loop.** `narches`, the `break` on an
  unreadable entry and goblin's entry-point math are already covered by their own tests and
  the fuzz target; this spec adds cases beside them, not over them.
- **[0031](0031-module-base-selection-is-testable.md)'s base-mapping rule.** Same pattern,
  different function, different fixture — deliberately a separate ticket.
- **Any other `cfg!` in the crate.** These two are the only ones inside a decision;
  `remote_debugging` has none at all.

## 7. Further notes

Land it adjacent to [0023](0023-unit-test-symbol-resolution.md) and
[0026](0026-unit-test-section-finders.md). All three edit the Mach-O builders — 0023 adds an
`LC_SYMTAB` and a `__cstring` section, 0026 adds a `__DATA,PyRuntime` section, this one adds a
second architecture — and whichever lands first moves them to the shared
`#[cfg(test)] pub(crate) mod testimage` all three agree on. Doing them apart means
rebasing the same helper three times.

Keep the general lesson where the next reviewer will find it: **a fixture that derives an
expected value the same way the code under test derives it asserts nothing.** Here that took
the shape of two `cfg!` expressions agreeing across the seam, which reads as diligence rather
than as a gap.
