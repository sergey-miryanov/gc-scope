# 0021 — Give the version grammar a module of its own

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/remote_debugging/version.rs`, 2026-08-09
- **Respects:** [ADR 0012](../docs/adr/0012-version-detection-fails-closed.md),
  [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

Nothing changes for an operator. The cost falls on a maintainer, who cannot tell what a
change to `version.rs` can be *proved* against.

`src/remote_debugging/version.rs` holds three jobs in one file with no boundary between
them:

- **the grammar** — `PythonVersion` with its hex codec and `Display`, `is_version_char`,
  `is_terminator`, `parse_exact`, `parse_digits`, `scan_for_version_string`. Pure: a
  function of its arguments, no goblin, no filesystem, no process.
- **image parsing** — `resolve_symbol_in_bytes` and its three per-format arms,
  `ro_data_range`, `read_only_data`. Pure over a byte slice, but goblin's job, not the
  grammar's. Spec [0022](0022-image-lookup-into-memory-binary.md) moves it out.
- **process IO** — `detect`, `read_version_from_process`. Needs a live PID and a readable
  module on disk.

Roughly 170 lines of grammar, 140 of image parsing and 50 of IO, under 435 lines of tests
that reach mostly the first. So the part with the deepest test coverage in the crate shares a
file with the part that has none, and nothing in the file says which is which. The grammar's
rules, which [ADR 0012](../docs/adr/0012-version-detection-fails-closed.md) settled and which
are the sole version source for 3.8/3.9/3.10 on every platform, sit in a module whose name
promises a live process.

## 2. Solution

The grammar becomes its own module, `remote_debugging::version::grammar`, and its tests go
with it. Every public path stays where it is: `version::PythonVersion`,
`version::detect`, `version::scan_image_for_version`, `version::resolve_symbol_in_bytes`.
An operator sees nothing; a maintainer opening `grammar.rs` sees a file with no `use` of
`reader`, `binary`, `goblin` or `std::fs`, and can rely on that.

## 3. User stories

1. As a gcscope maintainer tightening the version grammar, I want the grammar and its tests
   in one file with no IO in it, so that "does `cargo test` cover this?" is answered by the
   module I am editing rather than by reading its imports.
2. As a gcscope maintainer, I want the ADR 0012 reasoning attached to each rule to move with
   the rule, so that the next person to look at `+`-suffix acceptance or the `0xF` serial
   refusal finds the why next to the what.
3. As a maintainer reviewing this change, I want it to be a move with no edited logic, so
   that the diff is checkable by eye and a behavior change would stand out.
4. As the `scan_image_for_version` fuzz target, I want my entry point at the path I already
   call, so that `fuzz/` needs no edit and the Linux CI leg keeps running.
5. As an operator attaching to a 3.10 interpreter, I want nothing about detection to change,
   so that the only build with no source but the scanner is not disturbed by a tidy-up.

## 4. Implementation decisions

`src/remote_debugging/version.rs` becomes `src/remote_debugging/version/mod.rs` plus
`src/remote_debugging/version/grammar.rs`, following the `offsets/` precedent.

**Into `grammar.rs`:** `PythonVersion` (fields, `from_hex`, `to_hex`, `impl Display`),
`is_version_char`, `is_terminator`, `parse_exact`, `parse_digits`,
`scan_for_version_string`. Visibility: `PythonVersion` stays `pub`,
`scan_for_version_string` becomes `pub(super)` (its only caller is
`scan_image_for_version`), the rest stays private.

**Stays in `mod.rs`:** `detect`, `read_version_from_process`, `scan_image_for_version`,
`resolve_symbol_in_bytes` and the three per-format resolvers, `ro_data_range`,
`read_only_data` — until [0022](0022-image-lookup-into-memory-binary.md) takes the image
half.

**`mod.rs` re-exports the type:** `pub use grammar::PythonVersion;`. This is what keeps
`crate::remote_debugging::version::PythonVersion` resolving for `offsets/mod.rs`,
`offsets/pre_3_13.rs`, `session.rs` and `tests/remote_debugging.rs` without touching them.

**No logic is edited.** Not the grammar, not a doc comment, not a test body. The only
permitted edits are the `mod`/`use` lines, the visibility changes above, and whatever
`use super::*` adjustment the moved test module needs. A rewrite hiding inside a move is the
failure mode this rules out: the grammar's rules are load-bearing and several of them look
like accidents (the one-byte advance, `+` as a terminator rather than a rejection, the
charset run that swallows `c` so `parse_exact` can refuse it).

**Rejected — a third module for the value type.** `PythonVersion`'s hex codec is a different
encoding from the string grammar, so `value.rs` + `grammar.rs` is defensible. It is not worth
it at this size: `Display` and `parse_exact` describe one format,
`display_round_trips_parse_exact` asserts exactly that, and splitting them puts a round-trip
test across a module boundary for no gain. Revisit if the type grows behavior of its own.

**Rejected — flattening `detect` into a `version_io.rs` sibling.** The nesting is what makes
the public paths survivable; sibling modules would rename every call site in the crate to buy
the same separation.

## 5. Seams and testing decisions

- **Seam:** the existing in-file `#[cfg(test)]` module, moved with the code it covers. It is
  already the highest seam that can observe the grammar — the grammar has no other observable
  effect than its return value.
- **New seam needed:** none. This spec exists partly so the *next* two do not need one either.
- **What makes a good test here:** for a pure move, the test is that the existing tests still
  hold and none was deleted or weakened. `every_representable_version_round_trips_through_the_scanner`
  is the real proof — it sweeps 21 × 100 × 49 versions through `Display` and back through the
  scanner, so a rule changed during the move fails there rather than at a hand-picked example.
- **Prior art:** the `offsets/` split, where per-version logic moved into `v_*.rs` alongside
  the struct it describes and the tests moved with it.
- **Cases:**
  1. Every test currently in `version.rs`'s test module passes unedited in its new home, with
     `image_scan_survives_adversarial_bytes` staying in `mod.rs` (it exercises
     `scan_image_for_version`, which is image-level, not grammar).
  2. Regression guard: `fuzz/fuzz_targets/scan_image_for_version.rs` compiles and runs with no
     edit, and `tests/remote_debugging.rs` compiles with no edit — both name paths this spec
     promises not to move.
  3. `grammar.rs` has no `use` of `crate::memory`, `goblin`, or `std::fs`. Check it by
     reading the file at review time.

## 6. Out of scope

- **Moving the image-parsing half.** That is [0022](0022-image-lookup-into-memory-binary.md),
  kept separate because it changes a call site in `memory::process` and this one changes
  nothing.
- **Testing the symbol resolvers.** [0023](0023-unit-test-symbol-resolution.md).
- **Making `detect` testable.** [0024](0024-detect-policy-takes-its-reads.md).
- **Any grammar change.** Widening or tightening what the scanner accepts is an ADR 0012
  question, not a refactor.
- **`#[allow(dead_code)]` on `to_hex`.** It is only reachable from tests today.
  Leave the attribute; removing it is a separate judgement about whether the crate wants a
  public encoder.

## 7. Further notes

Do this as `git mv version.rs version/mod.rs` followed by a second commit that cuts
`grammar.rs` out of it, so `git blame` follows the file and the extraction shows as a pure
deletion-plus-addition pair rather than a rewrite.
