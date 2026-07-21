# 0008 — Reader/consumer package layering

**Status:** Accepted — implemented 2026-07-21. (Realizes the long-standing "the TUI is a
renderer only; one source of truth for reading" direction. The renderer package was named
`diagram` at the time of this ADR and later renamed to `tui`; names below are updated to the
current `tui`. Relates to
[ADR 0007](0007-gcstat-layout-driven-view.md), the decode primitive this layering centralizes,
and [ADR 0001](0001-pysession-resolve-once-facade.md), the `PySession` facade consumers attach
through.)

## Context

The guiding principle predates this change: the **TUI renders only**; all data *reading*
lives in one reader-layer source of truth shared by the monitor and the TUI, so a bugfix
in reading benefits both. The module tree, however, was flat under `src/` and hid the layering:

- `collect` and `poller` lived **inside `remote_debugging`** (the runtime model) even though
  they are consumers of it — orchestrating `session` + `offsets` into a renderer-shaped
  snapshot. (An earlier step had moved them *into* `remote_debugging`; that turned out to be
  the wrong side of the boundary.)
- `monitor`, `monitor_loop`, and `exporters/` were scattered top-level siblings.
- CLI handlers (`cli`, `cli_monitor`, `cli_monitor_options`) sat flat next to the subsystems,
  so a subsystem could — and `exporters`/`monitor` did — get tangled with argument parsing.

Two distinct data shapes exist over the same bytes, and conflating them was the trap:

- The **snapshot** consumer wants a fully-owned `CollectedData` (the `GcEntry` projection,
  torn ring entries dropped, renderer-shaped) — one picture per call.
- The **monitor** consumer wants `Vec<GcStat>` deltas (deduped by `ts_start`, per-interpreter,
  streamed) — and never consumes `CollectedData`.

So the genuinely shared thing is **not** the shaped output type; it is the *decode primitive*
(`OffsetTable::decode_gc_stats` + `PySession::gc_stats_region_addr` + the `GcStat` view of
[ADR 0007](0007-gcstat-layout-driven-view.md)). The reorg had to share that and let the two
shapes diverge above it.

## Decision

Make the layering **structural**, with dependency arrows pointing one way:

```
memory → remote_debugging → { snapshot, monitor, tui } → cli
```

1. **`memory` (L1)** — read the target's process memory and parse its binary images.
2. **`remote_debugging` (L2)** — the CPython runtime model (`version`, `offsets`, `session`,
   `gc_stats`, `check_interpreter`) **and the single decode primitive**. The one source of
   truth for reading the runtime; it holds no consumer-shaped types.
3. **`snapshot`** — the one-shot consumer: `collect` (`CollectedData`) + `poller`
   (`SnapshotPoller`), **lifted out of `remote_debugging`**. Consumed by `tui`, which
   stays a pure renderer.
4. **`monitor`** — the streaming consumer: `context` (`MonitorContext`), `run_loop`, and
   `exporters/`. `monitor/mod.rs` re-exports `MonitorContext`/`run_loop`/`PollStatus`/… so
   caller paths stay short.
5. **`cli`** — command definitions (clap) and handlers only. Subsystems stay **clap-free**,
   so they remain reusable libraries; `main.rs` is the thin dispatcher.

Supporting decisions:

- **`snapshot` and `monitor` are parallel consumers**, not layered on each other. Their
  sharing is at the decode primitive, never at the shaped type — which is *why* the monitor
  can't and shouldn't route through `collect_data`.
- **Per-layer snapshot reads via `CollectRequest`** (`debug_offsets` / `gc_state` /
  `gc_stats`, with `all()`/`tui()`/`gc_stats_only()` presets). A caller declares which
  heavy payload layers `collect_data` reads; a skipped layer comes back empty — indistinguishable
  from a version that legitimately lacks it, which the renderers already tolerate. The cheap
  navigation reads and layout scalars are always collected, so a valid skeleton snapshot is
  produced regardless of the request. `collect_data` is split into one function per chunk,
  with the request-gating kept visible in the orchestrator rather than buried in helpers.
- **`list_pids` stays a top-level shared utility.** It is consumed by both the `ListPids`
  command *and* the TUI's interactive PID picker (`pid_dialog`, `frame`); filing it under
  `cli/` would invert the arrow (`tui → cli`). It is process-discovery over
  `memory` + `remote_debugging`, so it sits as a peer.

## Consequences

- The layer boundary is now enforced by module structure, not convention: a subsystem cannot
  reach a sibling's private internals, and only `cli` depends on clap.
- A reading fix in the `remote_debugging` decode primitive benefits both `snapshot` and
  `monitor` by construction — the reader-layer principle is now load-bearing, not aspirational.
- Introducing `CollectRequest` also let the **dead L2 payload go**: the old 256-byte
  interpreter dump no renderer read was deleted.
- All moves were `git mv` renames (`git blame`/history follows each file); `exporters/` moved
  with **zero internal edits** because its cross-references were all `super::`-relative.
- Gotcha preserved from [ADR 0003](0003-layout-driven-gc-stats-decode.md): the CLI stats path
  and the snapshot collector each resolve the stats region independently. That independence is
  now intentional — both call the *same* `gc_stats_region_addr` primitive, so the two shapes
  share the address math they must agree on while keeping their divergent output types.
