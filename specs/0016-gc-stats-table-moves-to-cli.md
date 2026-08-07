# 0016 — The `gc-stats` table moves to the CLI layer

- **Status:** Not started — `ready-for-agent`
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** 2026-08-07 conversation reviewing `remote_debugging::gc_stats` layering.
- **Respects:** [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md) (the layering
  this completes), [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) (the column-set
  rule this must not change), [ADR 0005](../docs/adr/0005-testing-strategy.md) (why no new
  test surface is introduced)

## 1. Problem statement

Nothing an operator can see today. `gc-stats` prints the right table, in the right columns,
on every supported build.

What an operator will see is the *second* one. The `gc-stats` table's column set — which
columns exist, in what order, and the rule that widens them for an extended (`+inc`) build —
is currently defined inside the layer whose entire job is *reading* the target, not
presenting it. The moment a second presentation of the same numbers exists (a `--format
json`, a wider table for Probe-backed timing, a summary line), whoever writes it has two
choices, and both are bad: import the reader's printer and inherit its column decisions, or
restate them and let the two drift. That is the failure gcmon shipped and its ADR 0007
records — two renderings of one run that disagreed — and it is the reason this repo already
keeps `monitor::convert` (the format-independent model) separate from `exporters` (which
only encodes what it is handed).

The layering that prevents it is already written down and already enforced everywhere else.
[ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md) states that
`remote_debugging` "holds no consumer-shaped types" and that `cli` holds "command
definitions and handlers only". It lifted `collect` and `poller` out of the reading layer
for exactly this reason. The `gc-stats` table was left behind: `print_stats` is the only
`println!` in the whole `remote_debugging` layer — the three other writers there are
`eprintln!` diagnostics, not presentation.

## 2. Solution

The `gc-stats` table becomes the CLI's, like every other rendering in the tree. The reading
layer goes back to answering "what does this interpreter's GC state say", and the question
"how does that look as a table" is asked one layer up, next to the other command handlers.

For an operator, nothing changes. Same columns, same widths, same ordering, same
`No GC stats found.` on an empty read — byte-identical the day it lands, on every supported
version and both column sets. The whole benefit is downstream: the next output format is
written against the shared decode primitive rather than against a printer that lives in the
wrong place.

## 3. User stories

1. As an operator running `gc-stats`, I want the table to look exactly as it does today, so
   that I do not have to re-learn a tool that was supposed to be refactored, not changed.
2. As an operator on an extended (`+inc`) build, I want the wide column set to keep being
   selected by what my build actually publishes, so that a structural change does not
   silently narrow my output.
3. As an operator on a stock 3.8–3.14 build, I want the core column set unchanged, so that
   a cleanup aimed at the newest builds does not disturb the oldest ones.
4. As an operator whose target had no collections yet, I want `No GC stats found.` rather
   than an empty table or a bare header, so that the absence reads as an answer.
5. As an operator scripting around `gc-stats`, I want column positions stable, so that an
   `awk` field index written last month still works.
6. As a maintainer adding a Python version, I want the reading layer to contain only
   reading, so that registering a layout never means reading table-formatting code to
   understand what a new field does.
7. As a maintainer adding a `+inc` field, I want exactly one place that decides whether it
   becomes a column, so that adding it cannot half-land.
8. As a maintainer adding a second output format, I want the existing table to be a peer
   consumer of the decode primitive rather than something to import from underneath me, so
   that I am not forced to choose between duplication and inheritance.
9. As a maintainer, I want the layering ADR to describe the tree that actually exists, so
   that it stays an authority rather than an aspiration with a known exception.
10. As a maintainer, I want `GcStat`'s decode tests to sit with `GcStat` and the table's
    formatting tests to sit with the table, so that reading either file shows me its own
    contract.
11. As a maintainer, I want no function to become more visible because of this change, so
    that the crate's public surface does not grow to accommodate a move.
12. As a reviewer, I want this to read as a rename, so that I can check it by looking for
    changed characters rather than by reasoning about behavior.
13. As a reviewer, I want the two doc references to the moved symbol updated in the same
    change, so that the ADR and the layout doc comment do not point at a path that no longer
    exists.
14. As a CI job, I want `cargo test` to stay Python-free and sub-second, so that the fast
    gate keeps its purpose.
15. As a CI job, I want the live matrix and the custom-build leg to need no edits, so that
    the change cannot be masked by a test that moved with it.
16. As an AFK agent picking this up, I want the seam already chosen and the scope already
    bounded, so that I can land it without a design decision.

## 4. Implementation decisions

### What moves

Four functions and their tests, as one unit:

- `print_stats` — the only public item of the new module, and the only one `main` calls.
- `format_header`, `format_row` — already pure, already returning `String`.
- `has_extended` — moves *with* them. It is not a decode helper that the table happens to
  use; it is the table's column-set decision, and splitting it from the formatters would
  leave the decision straddling the boundary this change exists to draw.

The four `#[cfg(test)]` tests that cover them move too: the `has_extended` test and the
three row/column tests (extended layout in order, extended zero-fallback, core nine
columns). The six `GcStat` decode tests stay. The only duplication is the two `LazyLock`
entry-layout statics the two test sets share.

### Where it lands

A new module in the `cli` layer owning the `gc-stats` table. `main` keeps its
`Command::GcStats` handler and changes only the path it calls.

The arrow analysis matters here, because ADR 0008 has a documented exception: `list_pids`
stayed a top-level peer rather than moving under `cli`, because the TUI's PID picker
consumes it and `tui → cli` would invert the layering. The stats table has no such consumer.
The TUI renders GC entries through its own `tui::gc_view` renderers, and `snapshot::collect`
builds its own display projection; neither reaches for `print_stats`. Single consumer, so
`cli` is the correct home and no inversion is created.

### What explicitly does not change

The column-set rule is [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md)'s and
survives verbatim: the wide set is selected by the presence of `increment_size` in *any*
entry's `GcItemLayout` — keyed on field presence, never on a version. So does the
zero-fallback on the extended path, where each `+inc` column is read with a `0` default so
that an entry missing one prints `0` rather than dropping a column and misaligning the whole
table. Both are behavior; this spec moves code.

### No visibility widening

This is the part that makes the move cheap, and it should be verified before starting rather
than discovered halfway:

- The formatters read `GcStat` only through its already-public accessors — the typed core
  (`collections`, `collected`, `uncollectable`, `candidates`, `heap_size`, `duration`) and
  `get` for the `+inc` set. Nothing private to `remote_debugging` is touched.
- The moved tests build synthetic entries with `seq_layout` and `GcStat::from_fields`, both
  already `pub` under `#[cfg(any(test, feature = "test-hooks"))]`. A `#[cfg(test)]` module in
  a different file of the same crate reaches them unchanged, because the `cfg` applies to the
  whole crate compilation.

So all three of `has_extended`, `format_header` and `format_row` stay **private** in their
new home. If the move appears to require making something `pub`, that is a signal the
boundary was drawn in the wrong place — stop rather than widen.

### Rejected alternatives

- **A `print` submodule under `remote_debugging::gc_stats`.** Splits the file, which is the
  cosmetic half of the problem, and leaves presentation inside the reading layer, which is
  the actual one.
- **Moving the `Command::GcStats` handler in the same change** (attach → `gc_stats` → print,
  the way `cli::monitor` already owns its handler). Deferred deliberately: it changes control
  flow, so it would cost this change its "reviewable as a rename" property. Worth doing;
  worth doing separately.
- **Leaving it and documenting the exception in ADR 0008.** An exception with one member and
  no cause is a defect with paperwork.

### Follow-on edits

Two prose references to the moved symbol need updating in the same change: the mention of
`print_stats` in [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md), and the one in
the `GcItemLayout` doc comment in `remote_debugging::offsets::offset_table`.

Note for the implementer: this is a partial extraction, not a whole-file rename, so `git
blame` will not follow it the way ADR 0008's moves did. That is expected and not worth
contorting the change to avoid.

## 5. Seams and testing decisions

- **Seam:** `GcStat` + `GcItemLayout` — a synthetic entry layout in, a formatted line out.
  It already exists (the current tests were written against it precisely so the column
  decisions were testable without capturing stdout), and it is the **highest** seam that can
  observe the extended column set at all: above it, the `+inc` columns are reachable only
  from a custom-built interpreter on the `custom-build-smoke` leg.
- **New seam needed:** none. This is the strongest position a refactor can be in, and the
  same one [spec 0008](0008-shared-formatters-and-pid-table.md) argues from. The four tests
  move with the code and keep driving private functions from inside the module.
- **What makes a good test here:** for a pure move, the specification *is* byte-identical
  output, so the correct test suite is the existing one, unmodified. Any assertion added
  about where a function now lives would test the refactor rather than the behavior, and
  would need rewriting the next time the structure moves. Resist the pull to "improve the
  tests while they are open in the editor" — a test edited in the same change that moves it
  can no longer prove the move was inert.
- **Prior art:** the four formatting tests themselves; the byte-for-byte regression gate on
  the Chrome exporter, which is the model for pinning rendered output — and whose lesson
  (regenerating the fixture to make a build pass defeats its only purpose) applies exactly
  here; [spec 0008](0008-shared-formatters-and-pid-table.md) for the byte-identical stance on
  a de-duplication.
- **Cases:**
  1. The four moved tests pass **unmodified** after the move — same names, same assertions,
     same expected strings.
  2. The six `GcStat` decode tests pass unmodified in their original home.
  3. `gc-stats` output on the live matrix is unchanged across 3.8–3.15 and 3.15t, all three
     platforms — the core column set, which is what almost every operator sees.
  4. `gc-stats` output on the extended build is unchanged: the wide column set, in order,
     with a missing `+inc` field still printing `0` in its own column rather than shifting
     the row.
  5. The empty case still prints `No GC stats found.` and no header.
  6. `cargo test` still runs with no Python installed, in under a second.

## 6. Out of scope

- **Any change to a column** — its content, width, ordering, or the rule that selects the
  wide set. That is ADR 0007's decision and a behavior change; entangling it here destroys
  the only property that makes this reviewable.
- **The `Command::GcStats` handler**, per §4. Its own change, after this one.
- **A second output format** (`--format json`, a summary line, a Probe-aware wider table).
  This spec exists to make that cheap and is not the place to start it.
- **`GcStat` itself** — the decode primitive, its accessors, `iter_fields`, `is_complete`,
  and every test of them stay exactly where they are. This change must not touch the reading
  path at all.
- **The TUI's GC renderers** (`tui::gc_view`) and `snapshot::collect`'s display projection.
  They are separate consumers with separate shapes, which ADR 0008 says is correct; unifying
  them is not implied by this and is not wanted.
- **The hex-dump duplication** — that is [spec 0008](0008-shared-formatters-and-pid-table.md)'s
  second half, and the two changes touch no common file.
- **New tests.** The four that move are the coverage; adding more here would mean editing
  tests in a change whose whole claim is that nothing changed.

## 7. Further notes

Priority is low and should be stated as such: there is no divergence today and no operator
is affected. It earns a spec on the same grounds as
[spec 0008](0008-shared-formatters-and-pid-table.md) — the cost of doing it is fixed, small,
and known now, while the cost of the divergence it forecloses is unbounded and arrives
without warning, usually inside the change that adds the second format.

The two specs are siblings under the same ADR and share no file, so they can land in either
order or in parallel.

A reasonable trigger to promote it: the first time someone opens a spec for a second
`gc-stats` rendering. At that point this stops being cleanup and becomes a prerequisite.
