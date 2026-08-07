# 0016 — The `gc-stats` table moves to the CLI layer

- **Status:** Not started — `ready-for-agent`
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** 2026-08-07 conversation reviewing `remote_debugging::gc_stats` layering.
- **Respects:** [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md) (the layering
  this completes), [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) (the column-set
  rule this leaves alone), [ADR 0005](../docs/adr/0005-testing-strategy.md) (why no new test
  surface appears)

## 1. Problem statement

An operator sees nothing wrong today. `gc-stats` prints the right table, in the right
columns, on every supported build.

The cost lands on whoever writes the second rendering of those numbers. The column set,
their order, and the rule widening them for an extended (`+inc`) build all sit inside the
layer that reads the target rather than the layer that presents it. A `--format json`, a
Probe-aware wider table or a summary line must either import the reader's printer and
inherit its column decisions or restate them and let the two drift. gcmon shipped that
second outcome: two renderings of one run that disagreed, recorded in its ADR 0007. This
repo already guards against it elsewhere by keeping `monitor::convert` (the
format-independent model) apart from `exporters` (which encodes what it is handed).

[ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md) states that
`remote_debugging` "holds no consumer-shaped types" and `cli` holds "command definitions and
handlers only", and it lifted `collect` and `poller` out of the reading layer on that basis.
The `gc-stats` table stayed behind. `print_stats` is the only `println!` in the layer; the
three other writers there are `eprintln!` diagnostics.

## 2. Solution

The table becomes the CLI's, like every other rendering in the tree. The reading layer
answers what the interpreter's GC state says. How that looks as a table is decided one layer
up, beside the other command handlers.

Nothing changes for an operator: same columns, same widths, same ordering, same
`No GC stats found.` on an empty read, byte-identical on every supported version and both
column sets. The benefit arrives later, when the next output format is written against the
shared decode primitive instead of against a printer in the wrong place.

## 3. User stories

1. As an operator running `gc-stats`, I want the table unchanged, so that a refactor costs me
   no relearning.
2. As an operator on an extended (`+inc`) build, I want the wide column set still selected by
   what my build publishes, so that a structural change does not narrow my output.
3. As an operator scripting around `gc-stats`, I want column positions stable, so that an
   `awk` field index written last month still works.
4. As an operator whose target has not collected yet, I want `No GC stats found.` rather than
   an empty table, so that the absence reads as an answer.
5. As a maintainer adding a Python version, I want the reading layer to hold only reading, so
   that registering a layout never means reading table-formatting code.
6. As a maintainer adding a `+inc` field, I want one place deciding whether it becomes a
   column, so that adding it cannot half-land.
7. As a maintainer adding a second output format, I want the existing table to be a peer
   consumer of the decode primitive, so that I choose neither duplication nor inheritance.
8. As a maintainer, I want no function to gain visibility for this, so that the crate's public
   surface does not grow to accommodate a move.
9. As a reviewer, I want this to read as a rename, so that I can check it by looking for
   changed characters.
10. As a CI job, I want `cargo test` to stay Python-free and sub-second, and the live matrix to
    need no edits, so that a test moving with the code cannot mask a regression.

## 4. Implementation decisions

### What moves

Four functions and their tests, as one unit:

- `print_stats`, the only public item of the new module and the only one `main` calls.
- `format_header`, `format_row`, already pure and already returning `String`.
- `has_extended`. It is the table's column-set decision rather than a decode helper, and
  splitting it from the formatters would leave that decision straddling the boundary this
  change draws.

The four `#[cfg(test)]` tests covering them move too: the `has_extended` test and the three
row/column tests. The six `GcStat` decode tests stay. Both test sets need the two `LazyLock`
entry-layout statics, which get duplicated.

### Where it lands

A new module in the `cli` layer. `main` keeps its `Command::GcStats` handler and changes only
the path it calls.

ADR 0008 has one documented exception worth checking against: `list_pids` stayed a top-level
peer because the TUI's picker consumes it, and `tui → cli` would invert the layering. The
stats table has no second consumer. The TUI renders GC entries through `tui::gc_view` and
`snapshot::collect` builds its own display projection, so neither reaches for `print_stats`.
Single consumer, so `cli` is the right home and no inversion appears.

### What stays put

The column-set rule belongs to [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) and
survives verbatim: the wide set follows the presence of `increment_size` in any entry's
`GcItemLayout`, keyed on field presence rather than on a version. So does the zero-fallback on
the extended path, where each `+inc` column carries a `0` default so an entry missing one
prints `0` instead of dropping a column and misaligning the table. Both are behavior. This
spec moves code.

### No visibility widening

Verify this before starting rather than discovering it halfway:

- The formatters read `GcStat` through its public accessors only: the typed core
  (`collections`, `collected`, `uncollectable`, `candidates`, `heap_size`, `duration`) and
  `get` for the `+inc` set.
- The moved tests build synthetic entries with `seq_layout` and `GcStat::from_fields`, both
  already `pub` under `#[cfg(any(test, feature = "test-hooks"))]`. A `#[cfg(test)]` module in
  another file of the same crate reaches them unchanged, since the `cfg` applies to the whole
  crate compilation.

All three of `has_extended`, `format_header` and `format_row` therefore stay private. If the
move appears to require a `pub`, stop: the boundary went in the wrong place.

### Rejected alternatives

- **A `print` submodule under `remote_debugging::gc_stats`.** Splits the file, which is the
  cosmetic half of the problem, and leaves presentation inside the reading layer, which is the
  substantive half.
- **Moving the `Command::GcStats` handler in the same change**, the way `cli::monitor` owns
  its handler. It changes control flow, costing this change its reviewable-as-a-rename
  property. Worth doing separately.
- **Documenting the exception in ADR 0008 instead.** The exception has one member and no
  cause.

### Follow-on edits

Two prose references to the moved symbol need updating in the same change: the mention in
[ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md), and the one in the `GcItemLayout`
doc comment in `remote_debugging::offsets::offset_table`.

`git blame` will not follow a partial extraction the way ADR 0008's whole-file renames did.
Expect that rather than contorting the change around it.

## 5. Seams and testing decisions

- **Seam:** `GcStat` + `GcItemLayout`, taking a synthetic entry layout in and a formatted line
  out. It already exists, because the current tests were written against it to make the column
  decisions testable without capturing stdout. It is also the highest seam that observes the
  extended column set: above it, the `+inc` columns need a custom-built interpreter on the
  `custom-build-smoke` leg.
- **New seam needed:** none. [Spec 0008](0008-shared-formatters-and-pid-table.md) argues from
  the same position. The four tests move with the code and keep driving private functions from
  inside the module.
- **What makes a good test here:** for a pure move the specification is byte-identical output,
  so the correct suite is the existing one, unmodified. An assertion about where a function
  now lives would test the refactor and need rewriting the next time the structure moves. A
  test edited in the change that moves it can no longer prove the move was inert.
- **Prior art:** the four formatting tests themselves; the byte-for-byte regression gate on the
  Chrome exporter, whose lesson (regenerating the fixture to make a build pass defeats its
  purpose) applies here; [spec 0008](0008-shared-formatters-and-pid-table.md) for the
  byte-identical stance on a de-duplication.
- **Cases:**
  1. The four moved tests pass unmodified: same names, same assertions, same expected strings.
  2. The six `GcStat` decode tests pass unmodified in their original home.
  3. `gc-stats` output unchanged across 3.8–3.15 and 3.15t on all three platforms, for the core
     column set.
  4. `gc-stats` output unchanged on the extended build: the wide column set in order, with a
     missing `+inc` field still printing `0` in its own column rather than shifting the row.
  5. The empty case still prints `No GC stats found.` and no header.
  6. `cargo test` still runs with no Python installed, in under a second.

## 6. Out of scope

- **Any change to a column**: content, width, ordering, or the rule selecting the wide set.
  That belongs to ADR 0007, and entangling it here destroys the property making this
  reviewable.
- **The `Command::GcStats` handler**, per §4.
- **A second output format** (`--format json`, a summary line, a Probe-aware wider table).
  This spec exists to make that cheap and is not the place to start it.
- **`GcStat` itself**: the decode primitive, its accessors, `iter_fields`, `is_complete`, and
  every test of them. This change must not touch the reading path.
- **The TUI's GC renderers** (`tui::gc_view`) and `snapshot::collect`'s display projection.
  Separate consumers with separate shapes, which ADR 0008 calls correct.
- **The hex-dump duplication**, which is
  [spec 0008](0008-shared-formatters-and-pid-table.md)'s second half. The two changes share no
  file.
- **New tests.** The four that move are the coverage.

## 7. Further notes

Low priority: nothing has diverged yet and no operator is affected. It earns a spec on
[spec 0008](0008-shared-formatters-and-pid-table.md)'s grounds, that the cost of doing it is
small and known now while the cost of the divergence it forecloses shows up inside whatever
change adds the second format.

The two specs sit under the same ADR and share no file, so they can land in either order.

Promote this the first time someone opens a spec for a second `gc-stats` rendering. It stops
being cleanup then and becomes a prerequisite.
