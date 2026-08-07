# 0018 — Presentation belongs to the consumer layer, in code and in comments

**Status:** Accepted — implemented 2026-08-07. Completes
[ADR 0008](0008-reader-consumer-package-layering.md), which lifted `collect` and `poller` out
of the reading layer but left the `gc-stats` table behind. Delivers spec 0016, now deleted.

## Context

ADR 0008 made the layering structural — `memory → remote_debugging → {snapshot, monitor, tui}
→ cli` — and stated that `remote_debugging` "holds no consumer-shaped types". One thing stayed
on the wrong side. `print_stats` and its column formatters decided how the `gc-stats` table
looked from inside the layer that reads the target, and it was the only `println!` in
`remote_debugging`; the three other writers there are `eprintln!` diagnostics.

Nothing was broken, and no operator was affected. The cost falls on whoever writes the second
rendering of those numbers: a `--format json`, a Probe-aware wider table, a summary line. That
author either imports the reader's printer and inherits its column decisions, or restates them
and lets the two drift. gcmon shipped the second outcome, recorded in its ADR 0007 as two
renderings of one run that disagreed. This repo already guards the same seam by keeping
`monitor::convert` (the format-independent model) apart from `exporters` (which encodes what
it is handed).

Moving the code surfaced two things the structural arrows had not covered.

**Prose.** The reading layer justified its own invariants by naming the CLI printer, as in
"`print_stats` keys its whole column set on `increment_size.is_some()`". Moving the symbol
turned those into cross-layer references, and both a cleanup pass and a review agent proposed
repointing them at the new path rather than deleting them. A path pointing up the arrow points
up whether or not it sits inside a comment.

**Testability.** `print_stats` wrote to stdout, and Rust offers no stable in-process capture of
it, so the empty-slice message, the separator's width and the order of the lines had no seam
and no test. The three column formatters already returned `String` for exactly this reason.
The printer had never been given the same treatment.

## Decision

1. **Rendering lives in the consumer layer.** `remote_debugging` decodes; `cli` decides how a
   decode reads as text. `remote_debugging` now holds no `println!`, and ADR 0008's layering
   has no remaining exception.
2. **The arrows bind prose.** No comment, doc comment or rustdoc link in `memory` or
   `remote_debugging` names a consumer. State an invariant in the reader's own terms, or move
   the comment to the consumer that cares. Delete an upward reference rather than repointing
   it.
3. **A printing function delegates to a pure one.** `print_stats` prints the lines `render`
   returns. Where output cannot be captured, put a function returning the value behind it and
   let tests assert the value.
4. **Column selection stays where [ADR 0007](0007-gcstat-layout-driven-view.md) put it:** field
   presence in the Entry layout, never a version. The move changed no column, width or
   ordering, and the output is byte-identical.

## Consequences

- A second output format consumes the decode primitive as a peer of `cli::gc_stats`. Nothing
  routes it through the table's column decisions.
- The table's non-row behavior is now pinned: the empty-slice message, the header, the
  separator width and the line order. Two tests, each checked against a deliberate mutant.
- Decision 3 is unapplied elsewhere. `memory::dump::hex_dump`, `memory::regions::print_region`
  and `list_pids`' row writer still print directly, and stay untestable that way.
  [Spec 0008](../../specs/0008-shared-formatters-and-pid-table.md) already wants two of them
  shared; a render seam is the cheaper half of that work and should land with it.
- Decision 2 costs some expressiveness. A reader-layer test can no longer explain itself by
  what the CLI does with the value, so it states the invariant instead.
- `git blame` does not follow the moved functions. This was a partial extraction rather than
  the whole-file rename ADR 0008 used.
