# 0003 — Layout-driven GC-stats decode (pre-3.13 reuses the inline path)

**Status:** Accepted — implemented 2026-07-19 … 07-20. (Supersedes the old
`docs/legacy-gc-stats-plan.md`.)

## Context

The GC-stats memory layout changes across versions: 3.13/3.14 store
`generation_stats` **inline** in `_gc_runtime_state`; 3.15+ use a **ring buffer**
reached through a pointer; free-threaded vs. GIL builds differ in slot counts.
Hardcoding field positions per version in call sites does not scale.

The key insight for pre-3.13: the `gc_generation_stats` item — `collections@0`,
`collected@8`, `uncollectable@16`, 24 bytes — **and** the inline
`generation_stats[]` position (`0x80` within `_gc_runtime_state`) are **identical
across 3.8–3.13**. So pre-3.13 needs no new decode logic, only the same inline
path 3.13/3.14 already use.

## Decision

Decode is keyed by a **`GcStatsKind` { None, InlineArray, RingBuffer }** on the flat
`OffsetTable`, with a `GcItemLayout` mapping field names → byte offsets within one
slot. Both the stats loop (`PySession::gc_stats`) and the diagram (`collect.rs`)
branch on the **kind**, never on the version.

- **Pre-3.13 (3.8–3.12)** decode through `InlineArray` with a hand-written
  `LEGACY_GC_LAYOUT` (no bindgen struct to derive offsets from).
- **3.9–3.12** have per-interpreter GC state (`interp_gc`); **3.8** keeps GC state
  global in `_PyRuntime`, resolved by a global-GC branch — when `interp_gc` is
  `None`, read once at `runtime_addr + runtime_gc + gc_stats_inline_off` instead of
  per-interpreter.
- Capability is exposed as **`PySession::supports_gc_stats()`**
  (`gc_stats_kind != None`), which replaced the tier-based flag. `list-pids`' `S`
  column and the TUI picker's selectability read it, so 3.8–3.12 are selectable.
  (This is what let the earlier `Tier` enum be removed — see
  [ADR 0001](0001-pysession-resolve-once-facade.md).)

## Consequences

- GC stats decode across **all of 3.8–3.16** (`gc-stats`, `monitor`, `run`,
  `ascii`, `tui`). Verified live: pre-3.13 pyramids match each build's semantics —
  including 3.8's rarer full (gen-2) collections, which fire off the
  `long_lived_pending/total` heuristic, not a fixed 10× counter.
- The diagram has no `_Py_DebugOffsets` struct to visualize pre-3.13, so it renders
  a **focused GC-stats-only view** for `Legacy`; the obsolete SVG `render_svg`
  builder and the `diagram` subcommand were removed (the shared tree helpers stay).
- Adding a version's GC layout = set the kind + provide a `GcItemLayout`/geometry;
  no call-site edits.
- Gotcha this exposed: the CLI (`PySession::gc_stats`) and the diagram
  (`collect.rs`) each compute the stats address independently, so the 3.8 global-GC
  branch had to be added in **both** — fixing only one left the diagram reading
  garbage at `interp_head + 0x80`.
