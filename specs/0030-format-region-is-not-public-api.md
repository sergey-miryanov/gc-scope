# 0030 — Narrow `format_region` to private

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** XS
- **Origin:** review of `src/memory/regions.rs`, 2026-08-09
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

Nothing changes for an operator. The public surface carries one more name than the crate means
to expose.

`src/lib.rs` says what `pub` is for here: the crate is a library "so integration tests in
`tests/*.rs` can reach `PySession` and the offset registry directly", pointing at
[ADR 0005](../docs/adr/0005-testing-strategy.md). `pub` in this tree means *an integration
test or a bench needs to reach this*, not *this is API for the world* — there is no external
consumer and no semver or `cargo public-api` gate in CI that would ever say otherwise.

`memory::regions::format_region` does not meet that bar. Its only callers are `print_region`,
in the same module, and the four tests in the same file, which reach it through
`use super::*` and would reach it just as well if it were private. Of the nineteen `pub fn` in
`memory/`, it is the only one with no reference from `tests/`, `benches/` or `src/main.rs`
that is not already slated for deletion.

## 2. Solution

`pub fn format_region` becomes `fn format_region`. One word.

## 3. User stories

1. As a gcscope maintainer, I want the public surface of `memory` to be the set of things
   something outside the lib calls, so that reading it tells me what the crate
   promises rather than what it happens to expose.
2. As a maintainer moving or rewriting `format_region` later, I want the compiler to tell me
   the blast radius is one module, so that a formatting change is not weighed against
   imaginary consumers.

## 4. Implementation decisions

**Private, not `pub(crate)`.** The only non-test caller is `print_region`, in the same file.
`pub(crate)` would still be wider than the truth.

**This applies the neighbouring pattern rather than breaking it.** `memory::dump` has the same
shape — a printing wrapper over a pure formatter — and its `format_hex_dump` is correctly
`pub`, because `benches/memory.rs` imports it by name. The pair differ in whether anything
outside the lib calls them, which is exactly the distinction `pub` should be carrying.

**Rejected — narrowing `print_region` too.** `src/main.rs` is a separate crate and calls it
for the `list` subcommand. It stays `pub`.

**Rejected — auditing the rest of the public surface in this change.** See §6.

## 5. Seams and testing decisions

- **Seam:** the compiler. There is nothing to assert at runtime — the change either compiles
  or names its own counter-example.
- **New seam needed:** none, and the existing tests move not at all: they are in the same file
  and already use `use super::*`.
- **What makes a good test here:** the four existing `format_region` tests passing unedited.
  If one of them had to change, the premise was wrong.
- **Cases:**
  1. `cargo build`, `cargo test` and `cargo bench --no-run` all succeed — the third is the one
     that matters, since `benches/` is the other external consumer of `memory` and the only
     place a formatter is known to be imported from.
  2. `cargo clippy` clean, with no `#[allow(dead_code)]` needed — `print_region` is a live
     caller, so the function is not dead.

## 6. Out of scope

- **`reader::read_u64_h`.** It is the only other `pub fn` in `memory/` with no external
  reference, but [0028](0028-process-reader-owns-the-handle.md) deletes it and the whole `_h`
  pair outright. Narrowing it now would be work discarded twice over.
- **The other seventeen `pub fn` in `memory/`, and the rest of the crate.** Several are used
  only within the lib and could in principle be `pub(crate)`, but each needs checking against
  `tests/`, `benches/` and `main.rs` individually, and a sweep of that size is a different
  change with a different review. This spec is the one case that is unambiguous.
- **Anything about what `format_region` prints.** The column layout is pinned by
  `formats_a_region_line_exactly`; this change does not touch it.

## 7. Further notes

Fold into whatever next opens `src/memory/regions.rs`.
[0025](0025-interpreter-scan-takes-its-reads.md) is the likely carrier — it adds
`mapped_ranges` and `contains` to that module — and its §6 already names this as adjacent and
deliberately separate. Kept as its own ticket so it does not vanish if 0025 moves or slips.
