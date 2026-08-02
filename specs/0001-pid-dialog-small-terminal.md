# 0001 — Refuse the PID dialog on a terminal too small to hold it

- **Status:** Not started
- **Kind:** bug — crash
- **Effort:** S
- **Origin:** 2026-07-18 review (finding C9), half-fixed; the remaining half is below.
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem

Running `gcscope tui` with no PID on a short terminal — under 12 rows, which an editor's
bottom panel or a tiled split easily produces — kills the process the moment the PID
picker opens. Because the crash happens after terminal setup, it leaves the operator's
terminal in raw mode and on the alternate screen: no echo, no prompt, until they blindly
type `reset`.

## 2. Evidence

`tui::pid_dialog::popup_dims` is already safe — it saturates and clamps, and is
unit-tested:

```rust
// popup_dims
let popup_h = (num_rows as u16 + 4)
    .min(area_h.saturating_sub(4))
    .clamp(12, 30);
```

The clamp's **lower** bound is the problem: `popup_h >= 12` unconditionally, whatever the
terminal height. `render_dialog` then centers the popup with raw subtraction —
`(area.width - popup_w) / 2` and `(area.height - popup_h) / 2`. For any height in `0..12`
the second underflows: a debug build panics with *attempt to subtract with overflow*; a
release build wraps to roughly 65 000, builds a `Rect` far outside the buffer, and
ratatui panics on its own bounds check. Either way the process dies inside the
raw-mode/alt-screen region, where a panic message is invisible.

Width is incidentally safe — `popup_w` is 85 % of `area.width`, so it cannot exceed it —
but the code should not depend on that coincidence.

These two subtractions are the **only** unchecked `u16` layout arithmetic left in
`src/tui/`; everything else there already saturates.

## 3. Scope

**Affected:** `tui` with no PID argument (which opens the picker at startup), and the `p`
keypress while running. Any Python version, any platform — this is terminal geometry, not
interpreter decoding.

**Not affected:** `tui --output <file>`, which requires an explicit PID and never
constructs the dialog. No other command draws a popup.

**Why CI misses it:** the live matrix drives the CLI, and `tests/tui.rs` renders through
`render_snapshot` — neither instantiates the interactive dialog, and neither controls
terminal size. Nothing in the suite observes `render_dialog` at all.

## 4. Proposed change

1. Refuse to draw below a usable minimum, rather than clipping into a panic. Bail early in
   `render_dialog` with a centered "Terminal too small — need at least 40×12" message, so
   the operator gets an explanation and a working `q` instead of a dead terminal.
2. Center with saturating arithmetic regardless, so the guard is a courtesy message rather
   than the only thing standing between the code and a wrapped `Rect`.
3. Make `popup_dims` honest about the floor it advertises. With the guard in place, its
   lower clamp is only ever reached on a terminal that *can* hold 12 rows; its doc comment
   currently promises that "on a terminal shorter than 16 rows the result floors at 12 and
   ratatui clips the overflow", which is precisely the behavior being removed.

Key handling is untouched — this is a rendering guard only.

## 5. Seams and testing decisions

- **Seam:** a pure geometry predicate in `tui::pid_dialog`, tested in-file alongside the
  existing `popup_dims` tests. This is the highest seam available: the interactive dialog
  is not reachable from any integration test, and the decision being fixed is pure
  arithmetic over `(width, height)`.
- **New seam needed:** yes, minimal — lift the fit decision out of `render_dialog` into
  something like `fn dialog_fits(area_w: u16, area_h: u16) -> bool`, so the refusal is
  testable without a `Frame`. It sits at the top of the render path, next to the geometry
  helpers already extracted for exactly this reason.
- **Rejected:** a `TestBackend` + scripted-event harness for the whole dialog. It would
  cover more, but the interactive event loop was deliberately left uncovered when the TUI
  was split (only the thin I/O shell remains untested), and this defect is a subtraction,
  not a control-flow bug. Not worth reversing that call for one guard.
- **What makes a good test here:** assert the observable decision — for a given terminal
  size, does the dialog draw or refuse — not the intermediate `Rect`. No live process is
  involved, so this is one of the rare cases where a unit test genuinely closes the gap.
- **Prior art:** `popup_dims_clamp_height_between_12_and_30` and the `capacity_of` tests in
  the same module.
- **Cases:**
  1. Heights 0, 4, 8 and 11 refuse; 12 and above draw. Heights 4–11 are the ones that panic
     today.
  2. Widths below the minimum refuse, exercising the axis that is safe by coincidence.
  3. Regression: `popup_dims` output unchanged for every size at or above the minimum.
  4. Manual, both profiles: `cargo run -- tui` in an 8-row terminal — expect the message, a
     clean `q`, and a restored terminal. The release build must be checked separately; it
     wraps rather than panicking on the subtraction, so it fails further downstream.

## 6. Out of scope

A compact layout that makes the picker usable at 8 rows. Refusing to draw is the bug fix;
a small-terminal layout is a feature, and would need its own decisions about which columns
survive.

## 7. Further notes

The `p` keypress path re-enters the same render, so a terminal resized down *while* the
picker is open hits the same guard on the next frame — worth checking manually, since it
is the path an operator is most likely to reach by accident.
