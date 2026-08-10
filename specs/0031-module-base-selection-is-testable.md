# 0031 — Make the module base-mapping rule testable on every platform

- **Status:** Not started — lands with [0002](0002-pyruntime-module-discovery.md)
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/memory/binary.rs`, 2026-08-10
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

`memory::binary::find_python_modules` decides two things inside one loop over a process's
mappings: *is this a Python module* (a rule about the path) and *which of its mappings is the
image base* (a rule about the platform). [0002](0002-pyruntime-module-discovery.md) extracts
the first and unit-tests it. The second stays where it is:

```rust
if cfg!(target_os = "macos") && !m.is_exec() {
    continue;
}
if !modules.iter().any(|(p, _)| *p == path) {
    modules.push((path.to_string(), m.start()));
}
```

That is the whole rule — *the base is the first mapping, except on macOS where it is the first
executable one* — and every address gcscope computes is relative to whatever it returns.
[ADR 0004](../docs/adr/0004-per-platform-image-layout.md) exists for exactly this class of
fact, and the fourteen-line comment above the branch says what getting it wrong costs:

> Anchoring on the first mapping instead lands ~14 MB low — still inside *some* mapped
> region, so the read succeeds and silently returns garbage rather than failing cleanly.

Nothing tests it. And the way it is written means nothing *can*, portably: `cfg!` is a runtime
boolean that compiles on every platform and const-folds per target, so even a unit test of an
extracted function would only ever exercise the branch belonging to the machine running it. A
maintainer on Windows or Linux — and every CI leg but one — executes the `false` arm and never
the rule the comment is about.

A third rule rides along unstated: the surviving mappings are deduplicated by path with the
**first** winning, and the output preserves first-appearance order. That order is load-bearing
downstream — `find_runtime_pre_3_13`, `try_find_runtime` and the phase loops
[0024](0024-detect-policy-takes-its-reads.md) describes all take the first module that works,
so "which module wins" is decided here and asserted nowhere.

## 2. Solution

The selection becomes a pure function whose platform rule is an **argument**, not a `cfg!`.
Both arms then run in `cargo test` on every machine, so a maintainer who cannot run macOS can
still break the macOS rule and find out.

## 3. User stories

1. As a gcscope maintainer on Windows, I want the macOS base rule exercised by `cargo test` on
   my machine, so that the branch I never execute is not the branch nothing checks.
2. As a gcscope maintainer, I want "first mapping wins, in map order" pinned, so that the
   module ordering three other specs depend on is a stated property rather than a side effect
   of an iteration.
3. As an operator attaching to a framework Python on macOS, I want base selection to keep
   working exactly as it does, so that a tidy-up cannot reintroduce the ~14 MB-low read the
   comment describes.

## 4. Implementation decisions

```rust
// memory::binary — private, unit-tested in-file
enum BaseMapping { First, FirstExecutable }

fn select_module_bases(
    mappings: impl IntoIterator<Item = (String, usize, bool)>,  // path, start, is_exec
    rule: BaseMapping,
) -> Vec<(String, usize)>
```

`find_python_modules` becomes: read the maps, project each to `(path, start, is_exec)`,
filter with [0002](0002-pyruntime-module-discovery.md)'s `is_python_module`, then call this
with `if cfg!(target_os = "macos") { FirstExecutable } else { First }`.

**Passing the rule in is what earns the extraction.** Keeping `cfg!` inside the extracted
function would make it pure and still leave each machine testing only its own arm: the status
quo with extra steps. The argument turns a platform fact into something a test can state,
which is what [ADR 0004](../docs/adr/0004-per-platform-image-layout.md) asks for when it says
such facts are verified rather than inferred.

**An enum, not a `bool`.** `select_module_bases(mappings, cfg!(target_os = "macos"))` puts a
bare boolean at the call site with nothing saying what it selects. Two named variants cost one
type and read correctly at both ends.

**The projection stays in the adapter.** `m.filename().and_then(|p| p.to_str())` is a
`MapRange` accessor chain with nothing to decide — a mapping with no filename or a non-UTF-8
one is skipped, and that is not a rule worth a seam. It also keeps `proc_maps` types out of
the pure function, for the reason [0025 §4](0025-interpreter-scan-takes-its-reads.md#4-implementation-decisions)
gives: `MapRange` cannot be constructed in a test on any platform.

**No rule changes.** Same selection, same order, same dedup. If a case below documents
something surprising, pin it and raise it separately.

## 5. Seams and testing decisions

- **Seam:** `select_module_bases`, unit-tested in `memory::binary`'s existing `#[cfg(test)]`
  module. The highest seam that can observe the rule — above it sits `find_python_modules`,
  which needs a pid.
- **New seam needed:** yes, and it is the same one [0002](0002-pyruntime-module-discovery.md)
  introduces one line earlier in the same function. The two together turn that loop into two
  named rules and a projection.
- **What makes a good test here:** fixtures where the two rules give **different** answers. A
  mapping list whose first entry is already executable passes under both and proves nothing;
  the fixture has to put a non-executable mapping first, which is the macOS shape the
  comment describes and the one no non-macOS machine can produce today.
- **Prior art:** `binary::parse_macho_unwraps_a_universal_binary_and_reports_the_slice_offset`
  — same module, same platform fact, same technique of building the awkward shape by hand
  rather than waiting for a leg to produce it.
- **Cases:**
  1. `First`: two mappings for one path ⇒ the earlier `start`.
  2. `FirstExecutable`: a non-exec mapping precedes an exec one for the same path ⇒ the
     **exec** one's `start`. The ~14 MB-low bug in miniature, and unreachable on Linux or
     Windows today.
  3. The same input under `First` and `FirstExecutable` yields **different** bases. This is
     the assertion that the argument is load-bearing; without it, cases 1 and 2 can both pass
     against a function that ignores its rule.
  4. `FirstExecutable` with no executable mapping for a path ⇒ that path is **absent**, not
     present with a wrong base. Fails closed, which is what the caller's "no python-related
     modules found" message depends on.
  5. Dedup: three mappings for one path ⇒ one entry.
  6. Order: two paths interleaved across several mappings ⇒ output in first-appearance order.
     What `find_runtime_pre_3_13`'s "tried" list and 0024's "first match wins, in module
     order" both rest on.
  7. Empty input ⇒ empty output.
  8. Regression guard: the live matrix, and the macOS legs in particular — they remain the
     only evidence that the rule chosen for macOS is the *right* rule for a real framework
     image, which no fixture can establish.

## 6. Out of scope

- **The `python` / `pyruntime` name predicate.** [0002](0002-pyruntime-module-discovery.md),
  which is a bug fix and should not wait on this.
- **Changing the selection, the ordering or the dedup.** This pins them. Whether module order
  is the *right* order is the question [0024 §6](0024-detect-policy-takes-its-reads.md#6-out-of-scope)
  parks; this spec makes it observable without answering it.
- **Routing `find_python_modules` through `memory::regions`.** It needs each mapping's path
  and exec bit, not its extent, so `regions::mapped_ranges` as
  [0025](0025-interpreter-scan-takes-its-reads.md) defines it is the wrong projection. Left
  standing there and here for the same reason.
- **`list_pids`' process-name filter.** Different input, no substring rule available — see
  [0002 §6](0002-pyruntime-module-discovery.md#6-out-of-scope).

## 7. Further notes

Do [0002](0002-pyruntime-module-discovery.md) first and this immediately after: they extract
two rules from the same loop, and landing them apart means rebasing the same six lines twice.
0002 is the bug and should not be held up; this is the cleanup that stops the *other* rule in
that loop from being the next one to lose a clause with nothing to notice.

**The `cfg!`-versus-argument point generalises, and its one sibling is
[0032](0032-fat-slice-arch-selection.md).** `parse_macho` picks the fat slice with
`if cfg!(target_arch = "aarch64") { CPU_TYPE_ARM64 } else { CPU_TYPE_X86_64 }`, and its
fixture tags the slice using the same expression, so code and test agree by construction.
Same pattern, same file, same fix — the platform fact becomes an argument. Kept separate
because the fixture work differs: this spec needs no image at all, 0032 needs a
two-architecture one. Between them they cover every `cfg!` inside a decision in the crate.

Note the contrast with `#[cfg]`-gated code, such as what
[0029](0029-macos-task-port-lifetime.md) adds: that is a whole implementation per platform,
compiled only where it applies. A `cfg!` branch inside a shared function is the case that
needs the parameter, because it compiles everywhere and is exercised nowhere but the host.
