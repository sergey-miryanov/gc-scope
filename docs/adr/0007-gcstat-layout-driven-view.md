# 0007 — GcStat is a layout-driven view, not a fixed field superset

**Status:** Accepted — implemented 2026-07-21. (Extends
[ADR 0003](0003-layout-driven-gc-stats-decode.md), which keyed *decode* by layout kind;
this decides how one decoded slot is *represented*.)

## Context

ADR 0003 made decode layout-driven at the geometry level: a `GcItemLayout` maps field
names → byte offsets within one slot, keyed by `GcStatsKind`. But the decoded slot itself
was a **fat `GcStat` struct** — the 11 core counters plus **18 `Option<i64>`** enumerating
every `+inc`/extended field (`increment_size`, each per-phase `ts_*`, …). `decode_gc_stats`
copied each field out of the raw bytes by name into that struct. Three forces made this the
wrong shape:

- **It's a closed superset.** A custom CPython carrying a field not in that enum is
  undecodable — even though the build's `GcItemLayout` already knows the field's offset.
  Different builds carry different sets; the field set is inherently open-ended and belongs
  to the layout, not to named struct fields.
- **Two parallel decoders of the same bytes.** The TUI right-side detail panel already did
  the *right* thing — iterate the layout and read each field straight from the raw slot —
  while the Chrome exporter consumed the fat struct. Two code paths over identical bytes,
  free to drift.
- **A regular build's field set is essentially `GcSlot`-sized**; only custom builds carry
  the extras. Enumerating the union in a struct pessimizes the common case and still can't
  represent the uncommon one.

## Decision

1. **`GcStat` is a lean view.** It holds the always-present identity (`generation`, `slot`,
   `interpreter_id`), the slot's **owned raw bytes**, and a `&'static GcItemLayout`. Fields
   are read by name — `get`/`get_f64`/`has`/`iter_fields` — with typed convenience accessors
   (`ts_start()`, `collections()`, …) for the always-present core that dedup, summaries, and
   the exporter core use.
2. **Absent means `None`, not `Some(0)`.** `get` resolves the offset through the layout and
   bounds-checks the 8-byte read (a short slot returns `None`, never panics). A field the
   build's layout lacks returns `None` — the signal that distinguishes genuinely-absent from
   present-and-zero. Reading an extended field from a standard-set buffer is unit-tested to
   return `None`, never garbage read past the field list.
3. **One decode primitive.** `decode_gc_stats` becomes trivial: slice each slot's
   `item_size` window and wrap it in a `GcStat`. This single primitive backs **both** the
   Chrome exporter and the TUI right panel; the panel's ad-hoc raw read is retired
   (`GcStat::from_slot` + `iter_fields`).
4. **The exporter's GC-phase wiring is a data-driven `PHASES` table.** Each phase is a row
   (`Start::Explicit` | `Start::Chained`, stop field, arg fields) iterated in emission
   order and resolved via `s.get()`; output is byte-for-byte identical to the old
   hand-written blocks. The irregular implicit-start chaining is expressed as `Start::Chained`
   data, not eliminated — the caveat stays honest.
5. **`layout: &'static` is load-bearing.** Production layouts are `const`/`static`; the view
   borrows, it never copies the field table. Tests build throwaway layouts via a `seq_layout`
   helper (`Box::leak`) and slots via a `from_fields` builder under `test`/`test-hooks`.

`GcSlot` (the TUI's 11-field projection that drops torn ring slots) and the left summary
table are **unchanged** — the view refactor is confined to the exporter/right-panel decode.

## Consequences

- A custom build's fields flow through automatically — there is no enum to extend when a new
  `+inc`-style field appears; add it to the `GcItemLayout` and every consumer sees it.
- A decode fix lands once and benefits the exporter and the TUI panel by construction; the
  two-paths-over-one-buffer drift risk is gone.
- Field access is a by-name linear scan over ≤~28 fields. This is fine: export/print are not
  hot, and the monitor's dedup calls `ts_start()` once per slot per tick — the syscall/flush
  cost dominates, not the scan (see the perf reasoning that declined a benchmark here).
- `print_stats` keeps its two column sets, selected by `s.has("increment_size")` rather than
  by a struct field — the same open/closed logic, now layout-driven.
