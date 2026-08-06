# gcmon feature inventory and portability assessment

Research notes for porting features from `gcmon` (Python, `X:\Work\gc-monitor\gcmon`)
into `gcscope` (Rust, this repo). Everything below was read out of gcmon's own source,
manifests, docs and git history on **2026-08-05**, against gcmon `main` at `150f3e7`
("Update ADR") and gcscope `main` at `24c4364`.

> **On citations.** [`specs/README.md`](../../specs/README.md) tells specs to anchor on
> symbols, never line numbers, because line numbers rot. That rule is about *this* repo's
> code, which moves under us. gcmon is an external, frozen-at-a-commit source here, so
> line ranges are the cheaper and more precise handle. They are pinned to `150f3e7`;
> re-check them if you read a newer gcmon.

---

## Summary

gcmon is the **older sibling, not an ancestor** — the two repos share no git history
(`git log --format=%H` intersection is empty; gcmon's initial commit is 2026-03-19,
gcscope's 2026-04-10). It is a pure-Python 3.15+ CLI and library by the same author,
solving the same problem from the other end: where gcscope reads CPython's GC ring out of
process memory *itself* (goblin + `_PyRuntime` + per-version `_Py_DebugOffsets`), gcmon
delegates all of that to CPython 3.15's stdlib `_remote_debugging.get_gc_stats()` and
spends its 6,929 lines on everything *downstream* of the read: exporters, trace shaping,
statistics, loss reconstruction, process lifetime, an IPC control plane and a pyperf hook.

That division is the whole opportunity. **gcscope has the reader gcmon lacks; gcmon has
the consumer stack gcscope lacks.** gcscope's `ChromeTraceExporter`
(`src/monitor/exporters/chrome.rs`) is already a near-exact reimplementation of gcmon's
`trace_converter.py` — same slice names, same categories, same arg keys, same `G{gen}` and
`heap_size` counter tracks — so the two event models are already compatible. Almost
everything gcmon has beyond that point is portable.

Three things are **not** portable as-is: the control plane (its wire format is Python
`pickle`), the pyperf hook (it must be an installable Python entry point), and any feature
that assumes ring-layout GC fields on builds where gcscope supports only inline layouts.

### Feature → portability verdict

| # | Feature | gcmon files | ~LOC | Verdict |
|---|---------|-------------|------|---------|
| 1 | **Perfetto binary protobuf exporter** (tracks, counters, groups, ordering, Y-axis sharing) | `exporters/perfetto_*.py`, `protobuf_encoder.py` | ~1,320 | **Reshape — biggest prize.** Self-contained, no Python-only deps. Full Rust rewrite; hand-rolled varint encoder is 62 lines. |
| 2 | **GC Loss reconstruction** (ring-overwrite detection, windows, merge, split, apportion) | `loss.py` + `monitor.py:124-199` | ~410 | **Reshape — highest analytical value.** Pure arithmetic, trivially portable, but requires switching gcscope's dedup key from `ts_start` to `collections`, and must be gated on ring layouts. |
| 3 | **Statistics table** (`--stats`, per-gen p50/p90/p95/p99, coverage, scale factor, read-time row) | `stats.py`, `stats_output.py` | 635 | **Reshape.** No gcscope equivalent. Percentiles need either a Rust sketch crate or gcmon's own 1024-sample fallback (zero deps). |
| 4 | **JSONL + stdout exporters** | `jsonl_exporter.py`, `stdout_exporter.py` | 161 | **Direct lift.** Needs `EventsExporter` widened first. |
| 5 | **`--format` / multi-exporter fan-out** (`chrome+perfetto`) | `exporter_factory.py`, `combined_exporter.py` | 121 | **Direct lift.** |
| 6 | **RSS sampling** (`--rss`, `--rss-interval`) | `rss_sampler.py` + exporter hooks | ~110 | **Direct lift.** gcscope already depends on `sysinfo`; no new dependency. |
| 7 | **Process-liveness → `Processes` track** | `perfetto_process_lifetime.py`, `perfetto_track_state.py:98-144` | ~340 | **Reshape.** Depends on (1). gcscope's `mark_process_lifecycle` hook exists but is passed `ts_ns = 0` at all three call sites. |
| 8 | **`combine` subcommand** (merge traces, per-PID timestamp normalization, format conversion) | `commands/convert_cmd.py`, `exporters/chrome_trace_io.py` | 355 | **Reshape.** Needs a Chrome-JSON *reader*, which gcscope has none of. |
| 9 | **Env-var defaults for every flag** | `_env.py` | 256 | **Direct lift.** Needs clap's `env` feature (not currently enabled). |
| 10 | **Monitoring option surface** (`-d/--duration`, `-v` count, `--flush-threshold`, `--table-format`) | `commands/monitoring_options.py` | 229 | **Direct lift.** gcscope's `MonitorOptions` currently has only `--rate` and `--output`. |
| 11 | **In-flight / torn-entry handling** | `monitor.py:30-49,146-151` | ~25 | **Direct lift — likely fixes a latent gcscope bug.** |
| 12 | **Child-PID discovery + cursor pruning semantics** | `monitor.py:67-122`, `monitor_loop.py:49-59` | ~60 | **Overlaps.** gcscope has an equivalent; adopt the "listing failed ⇒ skip pruning" nuance only. |
| 13 | **Wait/run policies** (`StartupTimeoutPolicy`, duration runner) | `wait_policy.py`, `run_policy.py` | 89 | **Overlaps — already ported.** `src/monitor/run_loop.rs:20-68` is a line-for-line equivalent. |
| 14 | **Chrome Trace exporter** | `trace_converter.py`, `_buffered_exporter.py`, `encoder.py` | ~660 | **Overlaps — already ported.** `src/monitor/exporters/chrome.rs`. |
| 15 | **Child process spawn + graceful termination ladder** | `child_process_runner.py`, `utils/process_terminator.py` | 455 | **Overlaps, partially.** gcscope has spawn (`src/cli/monitor.rs`); the SIGINT→SIGTERM→SIGKILL escalation is worth lifting. |
| 16 | **Control plane** (`ControlClient`, start/stop/pause, instant markers) | `control/*.py` | 416 | **Hard blocker as designed.** Wire format is Python pickle over `multiprocessing.connection`. |
| 17 | **pyperf hook** (entry point, external-process model, metadata injection) | `pyperf/hook.py` | 336 | **Hard blocker as designed.** Requires an installable Python package with a `pyperf.hook` entry point. |

---

## 1. What gcmon is

**gcmon** is a pure-Python CLI + library (`pyproject.toml:9-16`, Poetry, MIT, v0.4.0,
published to PyPI) that watches a running CPython process's garbage collector from outside
it and exports the events to Chrome Trace, Perfetto binary protobuf, or JSONL. It requires
**CPython 3.15 or newer for both the monitor and the target** (`pyproject.toml:36`,
`README.md:20-23`), because it does no memory reading of its own: every GC read is one call
to the 3.15 stdlib module `_remote_debugging` — `get_gc_stats(pid, all_interpreters=True)`
and `get_child_pids(pid, recursive=True)` (`src/gcmon/monitor.py:5,74,91`; the local
type stub is `stubs/_remote_debugging.pyi:18-19`). It is invoked as `gcmon <pid>` /
`gcmon monitor <pid>` / `gcmon run -s script.py` / `gcmon combine …`
(`pyproject.toml:29-30` → `gcmon.cli:main`), and also as a pyperf hook via the
`pyperf.hook` entry point (`pyproject.toml:32-33`).

**Relationship to gcscope: siblings, not fork and upstream.** No shared commits. Same
author, same email, overlapping development windows (gcmon 2026-03-19 → 2026-08-05, 127
commits; gcscope 2026-04-10 → present, 52 commits). The kinship is visible in the code
rather than the history, and it is one-directional: gcscope's default monitor output
filename is literally `gcmon_trace.json` (`src/cli/monitor_options.rs:11`), and gcscope's
`ChromeTraceExporter` emits byte-compatible events with gcmon's `trace_converter`. gcscope
reimplemented gcmon's trace core in Rust on top of its own reader; it did not inherit the
repository.

The functional split is the thing to keep in mind throughout:

| | gcscope | gcmon |
|---|---|---|
| GC read | own: goblin + `_PyRuntime` + `_Py_DebugOffsets` per version | stdlib `_remote_debugging` |
| CPython versions | 3.8 – 3.16 | 3.15+ only, and monitor/target must be the **same build** (`README.md:118-129`) |
| Sub-phase timestamps | only on the `+inc` custom-build layout (`offsets/v_3_15_0b1_gcinc.rs`) | only on a "custom CPython build with enhanced GC instrumentation" (`README.md:131-142`) — the same constraint, expressed from the other side |
| Downstream | one Chrome-JSON exporter | 4 formats, stats, loss, RSS, lifetime, control plane, pyperf |

---

## 2. Feature inventory

### 2.1 CLI

Three subcommands, dispatched via `args.func` set by each subparser's `set_defaults`
(`src/gcmon/cli.py:16-28,113-119`). With no subcommand, `monitor` is the default
(`cli.py:117-119`).

- **`monitor <pid>`** — `commands/monitor_cmd.py:17-57`. `pid == -1` means self
  (`monitor_cmd.py:42-43`) — the same convention as gcscope's `resolve_pid`.
- **`run -s <script> | -m <module> [args…]`** — `commands/run_cmd.py:18-92`. Argument
  splitting is done *manually* before argparse ever sees it: `cli._split_run_args`
  (`cli.py:62-83`) cuts the argv at the first `-m`/`-s`/`--module=`/`--script=` and passes
  the tail verbatim.
- **`combine <inputs…> -o <out>`** — `commands/convert_cmd.py:15-116`. Merges Chrome or
  JSONL inputs into Chrome, JSONL, or Perfetto output, with optional `-n/--normalize`
  (per-PID timestamps rebased to 0). `chrome → jsonl` is explicitly refused
  (`convert_cmd.py:90-95`, enforced again in `chrome_trace_io.combine_files:185-189`).

Shared monitoring options (`commands/monitoring_options.py:49-127`): `-o/--output`,
`-r/--rate` (**seconds, float, default 0.1**), `-d/--duration`, `-v/--verbose` (counting),
`--format` (`chrome|perfetto|stdout|jsonl|chrome+perfetto`), `--flush-threshold`,
`--stats`, `--table-format`, `--control-name`, `--rss`, `--rss-interval`. Every one has an
environment-variable default (`_env.py:8-23` names, `:58-…` getters), CLI flag wins.
Validation and the RSS/rate advisory warnings live in `get_monitoring_options`
(`monitoring_options.py:158-229`).

### 2.2 Monitoring loop

`commands/monitoring_base.py:23-95` wires one `ExitStack`: exporter → control server →
process runner → `StreamingStats` → `EventsMonitor` → `RssSampler` → `MonitorLoop`, with
`replace_signals` installing SIGINT/SIGTERM handlers that just close the loop
(`monitoring_base.py:69-72`, `utils/replace_signals.py:6-20`).

`MonitorLoop.run` (`monitor_loop.py:40-99`) is a three-phase tick:

1. **GC poll** — for `[main_pid, *children]`, skipping any PID the control server has
   disabled, applying a per-PID `WaitPolicy` to the result (`monitor_loop.py:61-83`).
2. **Liveness** — one batched `exporter.add_process_liveness(live_pids, now_ns)`
   (`monitor_loop.py:88-89`). This is the *only* place that knows a process existed but
   never collected.
3. **RSS** — `rss_sampler.tick(now, live_pids)` (`monitor_loop.py:92-93`).

Children come from a single `get_child_pids(pid, recursive=True)` per tick
(`monitor.py:67-78`); the wrapper returns `None` on failure, and the loop treats `None` as
"listing failed, do not prune" rather than "no children" (`monitor.py:69-72`,
`monitor_loop.py:54-59`). Dead PIDs get their cursors dropped by `retain`
(`monitor.py:113-122`).

### 2.3 Event ingestion, dedup and in-flight handling

`EventsMonitor._ingest` (`monitor.py:124-199`) is the heart of it, and every line of it is
load-bearing:

- **Completeness test** — `_is_complete(event) := event.ts_start < event.ts_stop`
  (`monitor.py:30-33`). A ring entry with `ts_start` published and `ts_stop` not yet is
  mid-write.
- **In-flight tracking** — `_in_flight` (`monitor.py:36-49`) takes the newest incomplete
  `ts_start` per interpreter. That timestamp is carried to the *next* poll as a lower
  bound on the loss window (`monitor.py:146-151`), because collections in an interpreter
  are serialized.
- **Ordering** — entries arrive rotated around the ring's write position with generations
  concatenated, so they are re-sorted by `(iid, gen, collections)` before folding
  (`monitor.py:156-159`).
- **Dedup key is `collections`, not `ts_start`** (`monitor.py:165-170`). Keying on the
  cumulative counter also drops the duplicate the target writes when copying a record
  ahead of overwriting it — "both entries report the same counter, so no threshold tells
  them apart" (`monitor.py:167-169`).
- Loss windows are merged per interpreter and split around observed collections before
  export (`monitor.py:187-192`).
- Events go out sorted by `(iid, ts_start)` (`monitor.py:197-199`).

### 2.4 GC Loss reconstruction

`loss.py` (334 lines) plus the `_ingest` glue above. The idea: CPython's `collections` and
`duration` are cumulative per ring, so what the ring overwrote can be counted and its pause
time recovered even though the records are gone.

- `KeyAccumulator` (`loss.py:59-186`) — one per `(pid, iid, gen)`. `observe_batch`
  (`:75-105`) folds a poll's run; `_open_run` (`:107-135`) computes
  `lost = first.collections - self.last - 1` and derives the lost pause from the `duration`
  delta minus the bounding record's own pause, floored at zero because the two come from
  different clocks (`:126-134`). Exposes `exact_count`, `exact_pause_ns`, `coverage`,
  `scale_factor` (`:137-186`).
- `merge_windows` (`:189-218`) collapses one poll's cross-generation windows into disjoint
  spans so the track stays laminar.
- `split_around` + `_cut` + `_apportion` (`:221-298`) cut a span around collections that
  *were* observed (no lost record can have run during one that was seen) and share the
  counts across the pieces by width, largest-remainder so they add back up.
- `to_loss_msg` (`:301-319`) flattens to the wire record; `confirmed_by_interpreter`
  (`:322-333`) supplies the "nothing was lost before this read" bound.

Rendered as a `GC Loss {iid}` track under the process, one row per interpreter
(`trace_event.py:40-50` reserves tids `-2, -3, …`; `trace_converter.py:303-334` builds the
slices; `perfetto_format.py:254-284` describes the track). The slice width is the *blind
interval*, not the pause — read `lost_pause_total` from the args
(`trace_converter.py:304-318`, `docs/formats.md` §"GC Loss slices").

### 2.5 Statistics

`stats.py` (425) + `stats_output.py` (210).

- `Stats` (`stats.py:43-113`) — a DDSketch when `ddsketch` is installed, plus a
  1024-entry `deque` reservoir; `percentile` prefers the sketch only once the count exceeds
  the buffer (`:86-95`). `materialize()` freezes p50/90/95/99 and frees both, which is how
  per-PID stats get evicted past `MAX_ACTIVE_PIDS = 64` (`:68-79`, `:270-277`).
- Nine metrics, one per GC phase, each a tiny object with a `get_values(item) -> (start,
  stop)` (`stats.py:116-222`). The three phases CPython gives a stop but no start
  (`finalize_garbage`, `handle_resurrected`, `clear_weakrefs`) chain off the previous
  phase's stop (`:172-199`) — **the same chaining gcscope encodes as `Start::Chained` in
  `chrome.rs`**.
- `StreamingStats` (`:238-426`) aggregates globally and per PID, folds loss increments
  (`record_loss:292-309`, which also fires the one-shot coverage advisory), records
  lifetime totals (`record_lifetime:311-317`), and times the reads themselves
  (`record_read_time:288-290`, called around the `get_gc_stats` call at
  `monitor.py:90-93`).
- `stats_output.print_stats` (`:142-185`) prints the `PID | Metric | Count | Sum | Avg |
  P50 | P90 | P95 | P99 | Cov | F` table in plain or markdown. Cells show `sampled/exact`
  where they differ, `~`-marked for sub-phases whose "exact" is only the sampled value
  scaled by `F` (`:88-123`). `_coverage_cell` / `_factor_cell` (`:126-139`) refuse to round
  to `100.0%` / `1.000` when anything was lost — they print `<100.0%` / `>1.000`.

### 2.6 Exporters

Layered: `EventsExporter` ABC (`exporters/exporter.py:11-42`) → `BufferedTraceExporter`
(`_buffered_exporter.py:18-98`, buffer + threshold + `(pid, iid)` meta dedup + two locks)
→ a pluggable `EventEncoder` (`encoder.py:41-51`) with two implementations,
`JsonEventEncoder` (`:54-92`) and `ProtobufEventEncoder` (`:95-207`).

The trait surface is wider than gcscope's:

```python
add_event(pid, item)              # abstract
add_instant_event(pid, item)      # abstract
close()                           # abstract
add_rss_sample(pid, rss_bytes, ts_ns)      # no-op default
add_loss_event(pid, item)                  # no-op default
add_process_liveness(pids, ts_ns)          # no-op default
```
(`exporter.py:14-43`.)

Concrete exporters: `TraceExporter` (Chrome JSON, `chrome_trace_exporter.py:13-29`),
`PerfettoExporter` (`perfetto_exporter.py:15-43`), `JsonlExporter`
(`jsonl_exporter.py:19-119`), `StdoutExporter` (subclass swapping the writer,
`stdout_exporter.py:16-41`), `CombinedTraceExporter` (fans out to two,
`combined_exporter.py:37-88`). Selected by `EventsExporterFactory`
(`exporter_factory.py:11-33`).

Timestamps are **nanoseconds everywhere internally**; the only µs conversion is in
`JsonEventEncoder.write_events` (`encoder.py:72-74`, via `data.ts_to_us`). Perfetto keeps
ns natively. This matches gcscope exactly (`src/monitor/exporters/timing.rs`).

### 2.7 Perfetto protobuf stack

Six modules, ~1,320 lines, no protobuf library — hand-rolled per gcmon's ADR-0001.

- `protobuf_encoder.py` (62) — varint / zigzag / fixed64 / string / bytes / double field
  writers. Nothing else.
- `perfetto_proto.py` (120) — hand-maintained field numbers and enums, guarded by a
  dedicated test. Carries a load-bearing warning: `DebugAnnotation.name` is field **10**,
  not 1, because field 1 became an interned uint64 IID (`:103-110`).
- `perfetto_builders.py` (241) — pure "values in, wire bytes out" builders.
- `perfetto_track_state.py` (150) — per-trace uuid allocation, descriptor dedup, and the
  process-lifetime min/max accumulator (`:98-144`).
- `perfetto_format.py` (512) — the layout policy: a root uuid-0 descriptor enabling
  explicit process/thread ordering (`:122-153`), process descriptors carrying cmdline +
  `start_timestamp_ns` + `sibling_order_rank` (`:156-195`), a `Start Process` instant so a
  silent process's track is not hidden by the UI (`:198-228`), a non-OS-scoped
  `GC Metrics` group track that exists purely because trace-processor ignores
  `sibling_order_rank` on process/thread tracks (`:99-105`, `:287-311`), per-metric
  `sibling_order_rank` (`_COUNTER_RANKS`, `:78-90`), `heap_size`/`rss` promoted to
  top-level (`:92-97`), and `y_axis_share_key` so `G0/G1/G2 collected` share an axis
  (`:359`).
- `perfetto_process_lifetime.py` (189) — the shared `Processes` track: one BEGIN/END pair
  per pid, clipped by `_clip_spans_to_laminar` (`:114-142`) so overlapping spans nest, with
  `real_start_ts`/`real_end_ts` annotations on **every** slice so a consumer never has to
  ask whether a clip happened (`:47-70`).

### 2.8 RSS sampling

`rss_sampler.py` (85). `RssSampler.tick(now, live_pids)` (`:54-62`) rate-limits to
`--rss-interval` and emits `exporter.add_rss_sample(pid, rss, monotonic_ns())` (`:64-72`).
The provider is injectable; the default is psutil, and its absence disables RSS silently
rather than failing (`:42-52`). Only the Chrome and Perfetto exporters implement the sink
(`_buffered_exporter.py:73-79`); the others inherit the no-op, which
`monitoring_options.RSS_CAPABLE_FORMATS` (`:37`, `:186-190`) warns about up front.

### 2.9 Control plane

`control/control_server.py` (296) + `control/control_client.py` (120).

Parent side opens a `multiprocessing.connection.Listener` on `\\.\pipe\gcmon-<name>`
(Windows) or `/tmp/gcmon-<name>` (POSIX) (`control_server.py:28-49`), runs an accept
thread and a reader thread (`:114-159`), and handles `start` / `stop` messages by
toggling a per-PID enable flag that the monitor loop consults
(`:173-187`, `monitor_loop.py:67-68`). Every message also becomes an instant event in the
trace (`:212-214`). Child side is a Python API the user's app imports:
`ControlClient.start_monitoring()` / `stop_monitoring()` / `pause_monitoring()` context
manager / `instant_msg()` (`control_client.py:88-106`), auto-discovering the address from
`GCMON_CONTROL_ADDRESS` (`:59`), which `ChildProcessRunner._build_env` injects
(`child_process_runner.py:89-92`).

### 2.10 pyperf hook

`pyperf/hook.py` (336). Registered as `[tool.poetry.plugins."pyperf.hook"] gcmon =
"gcmon.pyperf.hook:gcmon_hook"` (`pyproject.toml:32-33`). The hook spawns
`python -m gcmon monitor <self-pid> --format jsonl --flush-threshold 10 --control-name
pyperf-hook-<pid>` as a **separate process** (`hook.py:277-302`) so nothing runs in the
benchmark, brackets the measured region with control-plane start/stop
(`:217-230`), then on teardown concatenates the temp JSONL files, replays them through
`StreamingStats` (`_replay`, `:93-128`) and injects `gc_*` keys into pyperf metadata
(`:265-268`).

---

## 3. Architecture map

```
cli.py ──► commands/{monitor,run,convert}_cmd ──► monitoring_base.run_monitoring_loop
                                                        │
        ┌───────────────────────────────────────────────┼──────────────────────┐
        ▼                     ▼                         ▼                      ▼
  ControlServer        ProcessFactory              EventsMonitor          RssSampler
  (IPC, enable map)    (External | ChildProcess)   (poll + _ingest)       (psutil)
        │                                               │                      │
        └──────────────────────► EventsExporter ◄───────┴──────────────────────┘
                                       │
             ┌───────────┬─────────────┼─────────────┬──────────────┐
          Jsonl       Stdout        Trace(JSON)   Perfetto      Combined
                                       └── BufferedTraceExporter ──┘
                                                   │
                                       EventEncoder{Json, Protobuf}
                                                   │
                            trace_converter ──► trace_event ──► perfetto_*
```

**Seams, best first.** The codebase is unusually seam-rich for its size — 22,247 lines of
tests against 6,929 of source — and the seams are mostly clean:

- **`EventsExporter`** (`exporters/exporter.py`) is the widest and cleanest seam. Everything
  upstream produces events; everything downstream is a sink. gcscope has the identical
  seam, just narrower.
- **`EventEncoder`** (`encoder.py:41-51`) separates event shaping from byte format.
  `BufferedTraceExporter` knows nothing about JSON or protobuf.
- **`loss.py`** is *pure*. Two msgspec structs and six free functions over
  `TGCStatsInfo` — no I/O, no globals, no CPython knowledge. The most portable module in
  the repo.
- **`perfetto_builders.py`** is pure by explicit design (its docstring says so): plain
  values in, wire bytes out, no uuid allocation, no policy.
- **`protocol.py`** — structural `Protocol` types plus `TypeGuard` predicates
  (`has_incremental`, `has_mark_alive`, …, `:118-152`). This is gcmon's answer to the same
  problem gcscope solves with `GcStat::has(name)`: fields that exist only on instrumented
  builds. **Both projects landed on capability-probing rather than version branching** —
  the conventions already agree.
- **Injectable providers** — `cmdline_provider` (`encoder.py:100-117`) and `rss_provider`
  (`rss_sampler.py:35`) keep psutil out of the test path.

**Tangled or leaky spots** (all of them tracked in gcmon's own `specs/`, which is a useful
independent confirmation):

- `CombinedTraceExporter.chrome_path` / `.perfetto_path` reach into `_output_path` on the
  sub-exporters with `# type: ignore[attr-defined]` (`combined_exporter.py:50-56`) — gcmon
  spec 0028 covers this.
- `PerfettoExporter` holds a **second, typed handle** to the same encoder the base holds as
  an `EventEncoder`, because liveness is neither a `TraceEvent` nor bytes and so does not
  fit the protocol (`perfetto_exporter.py:30-33`, `encoder.py:145-157`). A deliberate,
  documented leak — but a leak.
- `JsonlExporter` duplicates its buffer-and-flush block three times, once per `add_*`
  method (`jsonl_exporter.py:41-97`) — gcmon spec 0029.
- `pyperf/hook.py` imports `control_server._make_address` — a private function
  (`hook.py:22`).
- `chrome_trace_io._normalize_jsonl_timestamps` (`:130-175`) hand-enumerates all 13
  sub-phase timestamp fields. Adding a phase means editing it, and nothing enforces that.

---

## 4. Portability assessment

Read against gcscope's conventions in `CLAUDE.md`: `anyhow::Result` throughout,
version-specific behaviour in a `VersionedOffsets` accessor rather than scattered `if
version` checks, lib+bin split with everything in `src/lib.rs`, layout-keyed GC decode.

### 4.1 Direct lifts

**JSONL and stdout exporters.** `EventsExporter` in gcscope is
`{open, add_event, mark_process_lifecycle, close}` (`src/monitor/exporters/mod.rs:12-17`);
gcmon's is six methods. Widen gcscope's trait first — the extra three all have no-op
defaults in gcmon, and Rust trait default methods give the same ergonomics. Then a JSONL
sink is ~80 lines. `GcStat::iter_fields()` (`src/remote_debugging/gc_stats.rs:82-87`)
already yields exactly the `(name, value)` stream a JSONL line needs, so gcscope can do
this *better* than gcmon, whose `to_mapping` (`protocol.py:170-239`) hand-enumerates every
field.

**RSS sampling.** gcscope already depends on `sysinfo 0.39`, which exposes
`Process::memory()`. No new dependency, no psutil-absent degradation path needed. The
`tick(now, live_pids)` rate-limiting shape (`rss_sampler.py:54-62`) drops straight into
`run_loop`'s existing per-tick structure. ~60 lines.

**Env-var defaults.** clap's `#[arg(env = "GCSCOPE_RATE")]` replaces all 256 lines of
`_env.py`. Requires adding `"env"` to the clap features in `Cargo.toml` (currently
`["derive"]` only).

**Option surface.** `-d/--duration`, `-v` verbosity, `--flush-threshold`, `--format`,
`--stats`, `--table-format` are additive to `MonitorOptions`
(`src/cli/monitor_options.rs:4-13`). One unit change to watch: **gcmon's `--rate` is
seconds as a float, gcscope's is milliseconds as `u64`.** Keep gcscope's; don't import the
unit.

**In-flight / torn-entry handling** (`monitor.py:30-49`). This is small and probably fixes
a real gcscope defect. gcscope's `select_fresh` (`src/monitor/context.rs:157-168`) dedups
on a strictly-increasing `ts_start` high-water mark per `(gen, index)`. An entry caught
mid-write — `ts_start` published, `ts_stop` still 0 — passes that filter and would be
emitted as a `B`/`E` pair whose end precedes its begin. gcmon's `_is_complete` guard costs
one comparison. *(Uncertain: I did not construct the failing case, and gcscope's TUI path
does drop torn entries at `src/snapshot/collect.rs:387-410`, so the codebase already knows
about the hazard on the other path. Worth a test before treating it as a confirmed bug.)*

**Graceful termination ladder** (`utils/process_terminator.py:62-136`). SIGINT/
`CTRL_BREAK_EVENT` → wait → SIGTERM → wait → SIGKILL → indefinite wait, with the Windows
`STATUS_CONTROL_C_EXIT` (`0xC000013A`) exit code treated as normal shutdown
(`:139-158`). Portable as logic; the syscalls differ.

### 4.2 Needs reshaping

**GC Loss reconstruction — the highest-value port, and the one with real prerequisites.**

`loss.py` itself is pure arithmetic and would be a near-mechanical Rust translation. Three
things have to change around it:

1. **Dedup key.** gcmon keys on `collections` (`monitor.py:165-170`); gcscope keys on
   `ts_start` per `(gen, ring index)` (`context.rs:157-168`). Loss detection *requires* the
   counter — the whole method is `lost = first.collections - last - 1`. Switching gcscope's
   dedup is a change to `MonitorContext`, and it also buys the duplicate-suppression gcmon
   documents at `monitor.py:167-169` (the copy a target makes of a record before
   overwriting it, which no timestamp threshold can distinguish).
2. **Layout gating.** Loss needs `ts_start`, `ts_stop`, `collections` and `duration`.
   Per gcscope's offset registry, `duration` and the timestamps exist **only on ring
   layouts** (3.15.0b1+); inline layouts (3.13, 3.14) and pre-3.13 expose only
   `collections`/`collected`/`uncollectable`. So loss must be gated. Per `CLAUDE.md` the
   gate belongs in a `VersionedOffsets`/`GcStatsKind` accessor or a `GcStat::has()` probe,
   **not** an `if version` at the call site. `GcStat::has(name)`
   (`src/remote_debugging/gc_stats.rs:75-77`) is exactly the right instrument, and it is
   the same discipline gcmon uses with its `has_*` TypeGuards.
3. **All interpreters.** gcmon always reads every interpreter
   (`get_gc_stats(pid, all_interpreters=True)`, `monitor.py:91`) and keys loss state per
   `(pid, iid, gen)`. gcscope's monitor calls `session.gc_stats(false)` — first interpreter
   only (`context.rs`). The loss model, and the per-interpreter `GC Loss {iid}` track,
   assume the `true` path. gcscope already supports it (`gc-stats --all`); the monitor just
   doesn't use it.

Once those three are in place, `loss.rs` is ~350 lines of Rust with no dependencies.

**Perfetto protobuf exporter.** Self-contained and dependency-free by construction: the
encoder is 62 lines of varint/fixed64 writers (`protobuf_encoder.py`), and the builders
are pure functions. A Rust port can either transliterate that (keeping gcmon's ADR-0001
"no protobuf library" stance, which suits gcscope's lean dependency list) or take `prost`.
The genuinely hard-won knowledge is *not* the encoding, it is the layout policy in
`perfetto_format.py` and `perfetto_proto.py` — the uuid-0 root descriptor, the `GC Metrics`
group track that exists solely because trace-processor ignores `sibling_order_rank` on
OS-scoped tracks, the `DebugAnnotation.name = 10` trap, the `Start Process` marker.
**Port those comments verbatim.** They are the record of what was learned by getting it
wrong.

**Statistics.** `stats.py` + `stats_output.py` translate cleanly. The one decision is
percentiles: DDSketch is optional in gcmon (`stats.py:6-11`) with a 1024-entry sorted
reservoir as the always-available fallback (`get_quantile_value`, `:30-40`). Start with the
reservoir — zero dependencies, and gcmon's own docs say percentiles are sampled and read
high regardless (`docs/statistics.md`), so sketch precision is not where the accuracy lives.
The `Cov`/`F` columns and the `<100.0%` / `>1.000` anti-rounding rules
(`stats_output.py:126-139`) are the parts that make the table honest; keep them.

**Process-liveness → `Processes` track.** gcscope's `EventsExporter::mark_process_lifecycle`
already takes a `ts_ns: i64`, but all three call sites in `context.rs` pass `0`, and
`ChromeTraceExporter` discards the call entirely (`chrome.rs:357`). gcmon feeds this from
the monitor's own `time.monotonic_ns()` (`monitor_loop.py:46,89`) while GC event timestamps
come from the *target's* `PyTime_t` clock. On one machine both are the platform monotonic
clock, so mixing them is defensible — but it is an assumption gcmon never states.
*Uncertain: I found no test or comment in gcmon pinning it.* Flagging it because gcscope
currently emits **no** host-side timestamp at all, so adopting liveness means adopting that
assumption deliberately rather than by accident.

**`combine` subcommand.** Needs a Chrome-Trace *parser* (`chrome_trace_io._parse_events`,
`:88-115`) and a JSONL parser (`read_jsonl`, `:51-63`), neither of which gcscope has —
gcscope only writes JSON, by hand, with no serde dependency. This is the port most likely
to drag in `serde`/`serde_json`. Consider deferring it until the JSONL exporter exists,
since `jsonl → perfetto` is the useful direction and `chrome → jsonl` is refused anyway.

### 4.3 Overlaps with what gcscope already has

| gcmon | gcscope equivalent | Notes |
|---|---|---|
| `trace_converter.convert_item_to_trace_format` (`:39-297`) | `src/monitor/exporters/chrome.rs` | Already ported. Same names, cats, args, counter tracks. gcscope's `static PHASES` table + `Start::Chained` is a **cleaner** expression of gcmon's nine hand-written `if has_*` blocks. Do not port backwards. |
| `data.ts_to_us` (`data.py:75-77`) | `src/monitor/exporters/timing.rs` | Identical. |
| `wait_policy.StartupTimeoutPolicy` (`:22-53`) | `run_loop::StartupTimeoutPolicy` | Line-for-line equivalent, including the "seen alive once ⇒ die immediately" rule. |
| `run_policy.{Infinity,Duration}Runner` (`:11-35`) | — | gcscope has no `--duration`; this is the missing half. |
| `monitor.get_child_pids` / `retain` (`:67-122`) | `run_loop.rs` child discovery | Different mechanisms (one recursive stdlib call vs. transitive per-tick `remoteprocess`). gcscope's reaches grandchildren a tick later. Worth adopting: the `None`-means-listing-failed distinction. |
| `protocol.has_*` TypeGuards (`:118-152`) | `GcStat::has(name)` | Same idea, both capability-probing. Conventions already agree. |
| `child_process_runner` spawn + stdout pump (`:95-141,228-255`) | `src/cli/monitor.rs` spawn + pipe forwarding | Overlapping; only the termination ladder is worth taking. |
| `cli._split_run_args` (`:62-83`) | clap `trailing_var_arg` | gcscope's is better. Skip. |

**No overlap at all:** gcmon has no memory reader, no binary parser, no version detection,
no offset system, no TUI. Every one of gcscope's distinguishing subsystems is absent from
gcmon — there is nothing to deduplicate in that direction.

### 4.4 Hard blockers

**Control plane.** The transport is `multiprocessing.connection`, whose `Connection.send`
**pickles** the payload (`control_client.py:80` sends a plain dict; `control_server.py:163`
`conn.recv()` unpickles before `msgspec.convert`). A Rust server cannot speak that wire
format without implementing a pickle codec. Worse, the *client* is a Python API the user's
own application imports (`from gcmon.control.control_client import ControlClient`,
`docs/control-plane.md`), so gcscope cannot provide it at all without shipping a Python
package. Porting the *capability* — pause/resume monitoring, inject instant markers — means
choosing a new wire format (length-prefixed JSON over a named pipe / Unix socket would do)
and accepting that gcmon clients and gcscope servers will not interoperate. That is a
product decision, not an engineering one.

**pyperf hook.** `pyperf` discovers hooks through the `pyperf.hook` entry point
(`pyproject.toml:32-33`), which requires an installed Python distribution. gcscope is a
Rust binary with no Python packaging. The external-process model itself is already
gcscope-shaped — the hook only ever shells out to `python -m gcmon monitor …
--format jsonl` (`hook.py:287-302`) and reads the resulting file — so a thin Python shim
package that shells out to the `gcscope` binary instead would work. But it depends on the
control plane (start/stop bracketing, `hook.py:223,230`) and on the JSONL exporter, so it
is last in any dependency order.

**Version reach.** Several gcmon features silently assume ring-layout fields. gcmon can
assume them because it requires CPython 3.15+. gcscope supports 3.8–3.16, so anything
touching `duration`, `heap_size`, `ts_start`/`ts_stop` or the sub-phase timestamps needs a
layout gate. This is not a blocker to porting, but it *is* a blocker to porting naively:
gcmon's code carries no version branches at all, which will read as "portable everywhere"
and is not.

**Python 3.14+ syntax.** gcmon uses PEP 758 unparenthesized multi-exception `except`
clauses — `except psutil.NoSuchProcess, psutil.AccessDenied:` (`rss_sampler.py:84`) and
`except EOFError, OSError, ConnectionError:` (`control_server.py:165`). Irrelevant to a
Rust port; relevant if anyone tries to run gcmon's source or tests on an older interpreter
for reference.

---

## 5. Dependencies

**gcmon** (`pyproject.toml:35-40`) — runtime:

| Package | Required? | Used for |
|---|---|---|
| `python >=3.15` | **hard** | `_remote_debugging` stdlib module |
| `msgspec ^0.21.1` | **hard** | structs + JSON encode/decode (`data.py`, `trace_event.py`, `loss.py`, `encoder.py`) |
| `ddsketch ^3.0.1` | optional, extra `stats` | high-accuracy percentiles; falls back to a 1024-entry reservoir |
| `psutil >=7.2` | optional, extra `cmdline` | process cmdline in Perfetto descriptors + `--rss` |

Dev-only (`:42-57`): pytest, pytest-cov, pytest-repeat, pytest-codspeed, pyperf, mypy,
pyrefly, ruff, pre-commit, numpy, twine, **perfetto ^0.57.2** and **protobuf ^7.35** —
those last two are for *verification only*, decoding gcmon's hand-written output with the
real trace processor (gcmon ADR-0014). Notably there is **no protobuf runtime dependency**:
gcmon writes the wire format itself (ADR-0001).

**gcscope** (`Cargo.toml:7-17`): clap, anyhow, read-process-memory, proc-maps, goblin,
remoteprocess, ctrlc, ratatui, crossterm, sysinfo. Dev: goblin, sysinfo, codspeed-criterion.

**Disjoint.** No package appears in both. What a port would newly require of gcscope:

| Need | Options | Cost |
|---|---|---|
| JSON *reading* (for `combine`) | `serde` + `serde_json` | 2 new deps. gcscope currently hand-writes JSON and reads none. |
| Protobuf writing | hand-rolled (mirror gcmon's 62 lines) or `prost` | Hand-rolled costs 0 deps and matches gcmon's own choice. |
| Percentiles | reservoir (gcmon's fallback) or a sketch crate | Reservoir costs 0 deps. |
| RSS | already covered by `sysinfo` | 0 |
| Env-var flag defaults | clap `env` feature | 0 new crates, 1 feature flag |
| Process cmdline for Perfetto descriptors | already covered by `sysinfo` (gcscope uses it in `list_pids.rs`) | 0 |

Going the other way, gcmon depends on nothing gcscope would need to drop — there is no
dependency conflict, only additions.

---

## 6. Where gcmon's docs and code disagree

Recorded because a porter reading the docs would import the wrong facts. In each case the
code is what I trust.

1. **`--format chrome+perfetto` is undocumented.** The code accepts five formats
   (`monitoring_options.py:81`) and `EventsExporterFactory` implements all five
   (`exporter_factory.py:18-32`). `docs/cli.md` lists four, `docs/formats.md:1-5` says
   "four formats", and `README.md` says four. The combined format is real — the Quick Start
   even doesn't mention it while `derive_combined_paths` (`combined_exporter.py:15-34`)
   carries doctests for it.
2. **`--control-name` is undocumented.** It is in the shared option set
   (`monitoring_options.py:108-112`) and the pyperf hook depends on it
   (`hook.py:300-301`), but it is absent from the `docs/cli.md` options table.
3. **The control plane does not work with `monitor` the way the docs imply.**
   `docs/control-plane.md` ends with "The control plane is only available if you start your
   app with `gcmon run` or `gcmon monitor`", and shows `ControlClient()` with "no address
   needed, auto-discovered from environment". Auto-discovery reads `GCMON_CONTROL_ADDRESS`
   (`control_client.py:59`), which is injected only by `ChildProcessRunner._build_env`
   (`child_process_runner.py:89-92`) — i.e. only by `run`. `cmd_monitor`'s factory takes
   `control_address` and **discards it** (`monitor_cmd.py:49-50`), because an
   already-running target's environment cannot be changed. Under `monitor`, a target can
   only connect by constructing `ControlClient` with an explicit address matching
   `gcmon-<--control-name>`.
4. **The `-v` sample output is not the real log format.** `docs/cli.md:14-20` shows
   `[INFO] monitoring PID 12345 (chrome trace → gcmon.json)`. The actual formatter is
   `"[%(name)s] %(levelname)s: %(message)s"` (`cli.py:53`), producing `[gcmon] INFO: …`,
   and no such message string exists in the source. Illustrative, not literal.

---

## 7. Open questions for a human

1. **How far down the version range should the ported features reach?** Loss
   reconstruction, `duration`-based statistics and `heap_size` counters need ring layouts
   (3.15.0b1+). Do 3.8–3.14 targets get a degraded `--stats` (counts only), an explicit
   "unsupported on this layout" message, or nothing? This choice shapes the gate's location
   and is exactly the kind of thing `CLAUDE.md` says belongs in a `VersionedOffsets`
   accessor rather than at a call site.

2. **Does gcscope's monitor switch to all-interpreters?** `context.rs` reads only the first
   (`gc_stats(false)`). Everything in gcmon's track model, loss model and stats model is
   keyed per `(pid, iid, gen)`. Porting them without this makes `iid` a constant.

3. **Dedup key: `collections` or `ts_start`?** They are not compatible strategies. gcmon's
   counter-keyed dedup is a prerequisite for loss and additionally suppresses the target's
   pre-overwrite duplicate copy; gcscope's `ts_start` high-water mark is what
   `MonitorContext::select_fresh` is built on today. Changing it touches the one piece of
   gcscope's monitor that is currently well-tested.

4. **Protobuf: hand-rolled or `prost`?** gcmon chose hand-rolled (ADR-0001) and validates
   against the real trace processor in CI (ADR-0014). gcscope has a very lean dependency
   list, which argues the same way — but gcscope has no equivalent of gcmon's
   `perfetto`/`protobuf` dev-dependency verification leg, and hand-rolled protobuf without
   a differential test against a real decoder is exactly the failure mode
   `docs/adr/0004-per-platform-image-layout.md` warns about: it fails **open**, producing a
   file that parses to something different rather than not parsing.

5. **Is the control-plane capability wanted at all, given it cannot interoperate?** Any
   gcscope implementation is a new protocol. If the answer is yes, it should probably be
   specced independently rather than "ported".

6. **Should gcscope ship a Python sidecar package?** It is the only route to the pyperf
   hook, and it would also give the control plane a client. That is a packaging and release
   commitment well beyond a code port.

7. **Timestamp domains.** gcmon mixes monitor-side `time.monotonic_ns()` (liveness, RSS,
   control messages) with target-side `PyTime_t` (GC events) in one trace, without stating
   the assumption anywhere I could find. Before gcscope adopts liveness or RSS, someone
   should decide whether that is sound on all three platforms — gcscope today emits no
   host-side timestamp at all, so this is a new class of value in its traces.

8. **Suggested port order** (my read, not a decision): widen `EventsExporter` → JSONL
   exporter → `--duration`/`--format`/env defaults → in-flight guard → counter-keyed dedup
   → loss → stats → Perfetto → liveness/`Processes` track → RSS → `combine`. Control plane
   and pyperf are separate projects.

---

## Appendix: where this file lives

gcscope has no existing home for research notes. `docs/` holds finished topic docs
(`testing-policy.md`, `version-support.md`, `attach-traces.md`), `docs/adr/` holds decisions
already taken, `specs/` holds specified-but-unbuilt work, and `.scratch/` is gitignored and
local to one working copy. This is none of those — it is upstream *input* to specs that
don't exist yet. It was therefore placed at a new path, `docs/research/`, per the
instruction that produced it. If a convention emerges, this file should move rather than
setting one by accident.
