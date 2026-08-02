# 0008 — One PID-row assembly shared by the CLI table and the TUI picker

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** M
- **Origin:** 2026-07-18 review (findings R3 and R4); both partially reduced when the TUI was
  split and the verify column removed.
- **Respects:** [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)

## 1. Problem statement

The PID table exists twice — once as CLI output from `list-pids`, once as the TUI's picker —
and the two are maintained separately even though they are meant to show the same thing in
the same columns. Today they agree. Nothing keeps them agreeing: a column added to one is
simply absent from the other, and an operator who uses both surfaces has no way to know
which is current. The same is true, more mildly, of the two hex-dump renderers.

This is the kind of duplication that costs nothing until the day it silently costs a
release.

## 2. Solution

One definition of what a PID row *is* — its columns, its supported-ness, its tree indent —
consumed by both surfaces. The CLI keeps printing text, the TUI keeps applying its own
styling and scrolling; neither decides independently what a row contains. Output is
byte-identical the day it lands; the benefit is that it stays identical afterwards.

## 3. User stories

1. As an operator, I want `list-pids` and the TUI picker to show the same columns for the
   same process, so that I can move between them without re-reading the header.
2. As an operator, I want a process that is attachable to look attachable in both surfaces,
   so that the picker's dimming and the table's `Y`/`N` never tell me different things.
3. As a maintainer adding a column, I want to add it once, so that I cannot ship a half
   change.
4. As a maintainer, I want the change that introduces sharing to alter no output at all, so
   that reviewing it is a diff of structure rather than of behavior.
5. As a maintainer, I want the TUI to keep its own styling and cmdline scrolling, so that
   sharing the row model does not flatten the two surfaces into a worse version of both.

## 4. Implementation decisions

### PID rows — the substantive half

`prefix_depth` is defined identically in `list_pids` and in `tui::pid_dialog`. Around it,
the row assembly is duplicated in full: `list_pids::write_row` and the dialog's render loop
each compute the display name (version-or-name), the `R` and `S` characters, and the depth
indent, then format with the same literal — `"{:>8}  {}  {}  {:<22}    {}"`.

Introduce one column-assembly function that returns **data, not a formatted string**, so
the TUI can style per row:

```rust
impl FlatRow {
    fn columns(&self, no_cmdline: bool) -> RowColumns;  // pid, r, s, name, cmdline
    fn is_supported(&self) -> bool;                     // drives the picker's dim style
}
```

It belongs on `FlatRow` in `list_pids`, which already owns the row model; `tui::pid_dialog`
consumes it. That direction respects the package layering of ADR 0008 — the TUI is a
consumer, not a peer. `prefix_depth` moves alongside `FlatRow` and the TUI copy is deleted.
Cmdline scrolling stays in the dialog: it is genuinely TUI-only state.

The verify column and its `v_char` helper — the third copy the original finding listed —
are already gone with the verify flag, so this is now a clean two-way merge.

### Hex dumps — do only if it stays simple

`memory::dump::hex_dump` prints to stdout; `tui::layout::hex_dump_rows` builds styled
ratatui lines with byte highlighting. Down from three copies (the `ascii.rs` one left with
the `ascii` subcommand). They differ in output *type*, not in layout logic — both do the
16-bytes-per-row split, the hex column and the ASCII gutter.

One layout function in `memory::dump` yielding `(addr, bytes, ascii)` per row, rendered
twice, would remove the remaining overlap. **But** threading the TUI's highlight ranges
through that seam may cost more complexity than the duplication does. Decide during
implementation, and if the answer is no, record that here as decided-against rather than
leaving it open — a permanently deferred item is worse than a closed one.

## 5. Seams and testing decisions

- **Seam:** the CLI itself for the table (`list-pids` output as text) and
  `gcscope::tui::render_snapshot` for anything rendered. Both already exist; this change
  needs no new observation point, which is the strongest possible position for a refactor.
- **New seam needed:** none. If `RowColumns` turns out to want its own unit test, that is a
  pure function over a `FlatRow` and needs no seam at all.
- **What makes a good test here:** **byte-identical output** before and after, captured as
  a baseline first. For a pure de-duplication that is the whole specification — any
  assertion about internal structure would test the refactor rather than the behavior, and
  would need rewriting the next time the structure moves.
- **Prior art:** the `hex_dump_rows` unit tests in `tui::layout` (extend them to the shared
  layout function rather than replacing them); `tests/tui.rs` for the rendered-frame
  comparison.
- **Cases:**
  1. `list-pids`, `list-pids --tree`, each with and without `--no-cmdline`: output
     byte-identical to the captured baseline.
  2. The TUI picker: identical columns, with dim and selected styling still keyed on
     supported-ness.
  3. `read` and `read-runtime` hex output byte-identical (whether or not the hex half is
     done).
  4. A process tree deep enough to exercise `prefix_depth` at two or more levels — the
     helper being merged is the indent one, so a flat list would not prove the merge.

## 6. Out of scope

- Restructuring `FlatRow` itself, or changing any column's content, width or ordering.
  Anything that changes output belongs in its own change, precisely so that *this* one can
  be reviewed as "no output changed".
- Adding columns that have been wanted (thread count, RSS, uptime). Easier after this
  lands, which is part of the argument for it.
- The tree-drawing prefixes themselves — see
  [spec 0010](0010-tree-last-child-connector.md), which changes rendered output and must
  not be entangled with a no-op refactor.

## 7. Further notes

Lowest priority in the folder: there is no known divergence between the two copies today.
It earns its place because the cost of the merge is fixed and small, while the cost of the
divergence it prevents is unbounded and arrives unannounced.
