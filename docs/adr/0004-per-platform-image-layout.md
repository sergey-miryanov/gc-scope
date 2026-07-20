# 0004 — Per-platform image layout is discovered, not assumed

**Status:** Accepted — implemented 2026-07-20.

## Context

Runtime finding ([ADR 0002](0002-version-split-runtime-finding.md)) reaches the
target's binary through three format-specific paths — `find_section_in_{elf,pe,macho}`
and `resolve_symbol_{elf,pe,macho}`. All development happened on Windows/PE; the
other two formats compiled, were reachable, and had never executed against a real
process. The first live smoke matrix across all three platforms failed every
non-Windows leg — Linux 3.13+ (3/8) and macOS entirely (8/8) — on five defects,
none version-related, each a place where PE-shaped facts were assumed universal:

1. **ELF section names are dotted.** CPython's `GENERATE_DEBUG_SECTION`
   (`Include/internal/pycore_debug_offsets.h`) emits `section("." #name)` on Linux
   only, so the section is `.PyRuntime` on ELF, `PyRuntime` on Mach-O, and
   `PyRuntim` on PE (8-char truncation). The ELF branch matched the undotted
   spelling, so the 3.13+ cookie path had **never** worked on Linux.
2. **macOS images are universal binaries.** The Python framework ships
   `universal2`, so offset 0 holds a fat header and `MachO::parse(bytes, 0)` failed
   outright — at all three Mach-O call sites.
3. **Mach-O decorates C symbols.** `_PyRuntime` is stored as `__PyRuntime`,
   `Py_Version` as `_Py_Version`.
4. **A fat slice must be parsed as a standalone buffer.** Unwrapping in place
   (`MachO::parse(bytes, slice_start)`) is not enough: a slice's internal *file*
   offsets (`symtab.symoff`/`stroff`, `section.offset`) are slice-relative, and
   goblin indexes them into whatever buffer it is given. It therefore read the
   symbol table out of the wrong slice and returned **no symbols at all**. The
   slice must be cut out and parsed at offset 0.
5. **The macOS image base is not the lowest mapping.** The kernel attributes
   low-address reservations to the image path, so the first mapping is a no-access
   `---` range ~14 MB below the image. The Mach-O header sits at the start of
   `__TEXT`, the executable mapping, and section `vmaddr`s are relative to that.
   ELF/PE are the opposite: their first mapping *is* the load base.

Defects 1–3 fail *closed* — the parse fails and the error is honest. The other two
do not, which is why they survived:

- **4 failed silently.** An empty symbol table is indistinguishable from a stripped
  binary, and it broke only symbol-driven paths while leaving address-driven ones
  (`vmaddr`, `n_value`) green. That split coincides exactly with the 3.13 version
  boundary, so a single format bug presented as a version-specific one.
- **5 failed open.** The wrong base still pointed into a mapped `rw-` region, so
  reads succeeded and returned garbage. Only the `"xdebugpy"` cookie caught it.

The lesson worth keeping: **a partially-correct fix for a format quirk is worse
than none**, because it relocates the failure to something that looks like a
different problem.

## Decision

**Per-platform image facts belong in `memory/binary.rs` and the format-specific
finders, and each must be confirmed by a live CI leg rather than inferred from the
format spec.**

- Section lookup accepts every spelling a platform emits (`.PyRuntime` /
  `PyRuntime` / `PyRuntim`).
- All Mach-O parsing goes through **`binary::parse_macho`**, which selects the
  host-architecture slice, cuts it out and parses it at offset 0, and returns the
  image plus the slice's start offset. Callers using virtual addresses ignore the
  offset; callers using *file* offsets (`sect.offset`) add it. Both halves matter:
  parse standalone or the symbol table is silently empty; rebase file offsets or
  reads land in the wrong slice.
- Symbol matching accepts the undecorated name or an underscore-prefixed one.
- `find_python_modules` picks the image base per platform — first executable
  mapping on macOS, first mapping elsewhere — `cfg`-gated so ELF/PE are untouched.

## Consequences

- All 24 legs green (3 OSes × Python 3.8–3.15); `live-smoke` is a blocking gate.
- ADR 0002's open question on ELF/Mach-O symbol visibility is **answered for both**.
  The documented data-segment scan fallback remains unnecessary everywhere.
- **macOS attach privilege is a property of the target.** Every framework build is
  hardened (`flags=0x10000(runtime)`), but only 3.13+ carries
  `com.apple.security.get-task-allow`, which lets a same-user caller take the task
  port; 3.8–3.12 returns `EPERM` and needs root. Granting
  `system.privilege.taskport` does not change this. The entitlement ships with PEP
  768 remote-debugging support, so unprivileged attach to 3.13+ is a supported
  configuration. CI encodes the split rather than running macOS under blanket root,
  so a build that stops shipping the entitlement fails instead of being masked.
- `find_runtime_pre_3_13` reports per module whether the symbol resolved and to what
  address, rather than collapsing "symbol missing" and "offsets disagree" into one
  message. That distinction is what located defect 4.
- Two fixes are **correct-but-inferred** and should be revisited:
  - `parse_macho` selects the slice by `cfg!(target_arch)` — *gcscope's*
    architecture, not the target's. A universal2 Python under Rosetta read by an
    arm64 gcscope would pick the wrong slice and compute a wrong address
    **silently**. The correct key is the target process's architecture.
  - "First executable mapping is the base" holds only because `__TEXT` is `r-x` and
    carries the header. The rigorous source is dyld (`task_dyld_info` / image list).
- Every one of the five was found by *running* the matrix; each attempt to reason
  ahead of it picked the wrong culprit. The cheap diagnostics that settled them —
  `file`/`otool`/`nm`/`codesign` on the image, a mapped-region dump on failure, and
  the per-module symbol-vs-cross-reference breakdown — are retained in the workflow
  and in `tests/live_smoke.py`.

## Verification

The live smoke matrix — 3 OSes × Python 3.8–3.15, one leg per pair; see
`docs/tests-harness-plan.md`. A platform assumption that regresses fails the
specific `(os, version)` leg that depends on it.
