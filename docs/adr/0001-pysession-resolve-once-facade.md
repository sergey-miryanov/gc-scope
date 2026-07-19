# 0001 — `PySession`: resolve a process once, tiered `Resolved`

**Status:** Accepted — implemented 2026-07-19. (Supersedes the old
`docs/pysession-plan.md`. Resolves `docs/fix-plan.md` findings A3, E1, E2, C4,
C6, C7, R1.)

## Context

Every command re-derived the same immutable per-process facts on each call, tick,
or poll: `find_runtime` (a multi-MB goblin parse), `version::detect`,
`read_offsets` (which itself called `find_runtime` again), and a fresh
`ProcessHandle` per memory read. The TUI/`collect` path did all of this ~100×/sec.

Worse, *"is this process supported?"* had no single answer. The three-tier
resolution cascade lived only inside a `verify` helper that **discarded** its
result and returned `Option<bool>`, while `gc-stats`/`collect` knew only the
bindgen tier. The same process could report supported under `--verify` yet fail
`gc-stats`. Nothing owned the resolved state of an attached process.

## Decision

Introduce **`PySession`**, attached once per PID via `PySession::attach`, as the
single "resolve this process" facade:

- `attach` runs the resolve cascade **once** and owns the `ProcessHandle`; all
  reads (`read`/`read_u64`/`read_i64`) reuse that handle — no per-read
  `OpenProcess`.
- The result is a tiered **`Resolved`** enum — `Full { offsets, table }` (3.13+
  exact bindgen), `LayoutOnly { offsets, table }` (3.13+ same-minor fallback),
  `Legacy { table }` (3.8–3.12 hardcoded). Every consumer matches on this one
  enum and degrades uniformly. **No command calls `find_runtime`,
  `version::detect`, or `read_offsets` directly** anymore — adding a command means
  `attach` + match on `Resolved`.
- Retry/wait stays out of the session: `attach` failure and read failure both map
  to `PollStatus::InvalidProcess`, and the caller's `WaitPolicy` decides retry vs.
  give-up.

### Instance vs. layout lifetime (reused/stale PID handling)

A session holds fields with different lifetimes, so caching is split in two:

- **Instance state** (`handle`, `runtime_addr`, per-`(gen,slot)` freshness) is
  per-PID and evicted through a **single** site, `mark_died`.
- **Layout** (`Resolved`) is a pure function of the binary, cached separately keyed
  by **`(interpreter-path, mtime)`**, and **survives death** — so a relaunch or a
  sibling PID on the same libpython skips the goblin parse. A one-word live
  version-word read backstops the mtime proxy; `cmdline` is the change-detector for
  soft re-attach on PID reuse (`revalidate` → `Fresh`/`Changed`/`Dead`). The layout
  key must be the interpreter/libpython module, not `argv[0]` (embedding apps).

## Consequences

- One resolve per process; the per-frame goblin-parse storm is gone.
- `collect_data` stayed a **free function taking `&PySession`** (not a method) to
  avoid a `remote_debugging`→`diagram` layering inversion.
- **Later changes** that built on this:
  - The separate `Tier` enum this introduced was **removed**; the `Resolved`
    variants plus `PySession::supports_gc_stats()` now carry the capability
    distinction (see [ADR 0003](0003-layout-driven-gc-stats-decode.md)).
  - `PySession::verify` was **removed** when the navigation heuristic left the
    user-facing surface (see [ADR 0002](0002-version-split-runtime-finding.md)).
- Open item: a `tests/` smoke harness (spawn bench interpreters, assert gc-stats
  non-empty + no hang) was planned as the regression gate but not built — steps
  were verified manually. The layout-cache-hit and soft-reattach paths remain
  unobserved by an automated test.
