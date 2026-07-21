# 0009 — Performance is guarded by shape, not benchmarks (for now)

**Status:** Accepted — decided 2026-07-21. (Records a deliberate non-action and its trigger.
Complements [ADR 0005](0005-testing-strategy.md); prompted by the refactors in
[ADR 0007](0007-gcstat-layout-driven-view.md) and [ADR 0008](0008-reader-consumer-package-layering.md).)

## Context

The `GcStat` view (0007) and the package reorg (0008) raised the natural question: do these
help or hurt performance, and should a benchmark lock that in?

- **The refactor is roughly perf-neutral.** A smaller struct and lazy by-name decode help the
  monitor's per-tick path; a per-entry `Vec` allocation and a by-name linear field scan offset
  them; none of it is on a hot path. There is no perf *win* to claim and nothing regressed —
  so the refactors are not sold as performance work.
- **The only plausible scaling axis is the monitor/`run` loop** against a process tree with
  tens of children (multiprocessing pools, workers) — not decode of a single entry, and not the
  TUI. So if anything were benchmarked, that loop is the right scope.

But a wall-clock benchmark of that loop is the wrong instrument:

- Its wall-clock is dominated by **syscalls** (ptrace / read-process-memory), **exporter
  flush**, and **process-tree enumeration** — OS and scheduler cost, not gcscope's algorithm.
  A benchmark would mostly measure the platform.
- There is **no SLA**. Without a target, an absolute millisecond number can't be judged
  pass/fail; it just drifts and produces flaky CI.
- Micro-benchmarking the pure decode would measure the fast, non-dominant part and give false
  confidence.

## Decision

1. **No benchmarks now.** Correctness and decoded *shape* are the gate (ADR 0005), not speed.
   The refactors are not presented as optimizations.
2. **When regression protection is genuinely needed, guard *shape*, not wall-clock.** Express
   the loop's cost as **complexity invariants**: count attaches / memory-reads / allocations as
   a function of (N PIDs × entries) and assert them — linear in PIDs, **one attach per PID reused
   across ticks** (no per-tick re-attach), no quadratic tree-diff or dedup. These invariants are
   stable across machines; a wall-clock number is not.
3. **Threshold for adding any benchmark** — all three must hold: (1) a real scaling axis exists,
   (2) the cost being measured is *our algorithm*, not syscalls, and (3) it is expressible as a
   stable invariant (or an SLA exists to anchor an absolute number).
4. **Trigger to revisit**: actually running the monitor against a tens-of-PIDs tree and seeing
   per-tick cost matter, **or** restructuring the tree-discovery/dedup path (where a quadratic
   could sneak in). Until then this is a documented deferral, not an oversight.

## Consequences

- CI stays free of a platform-dominated, flaky timing test.
- The instrument is **pre-chosen**: if the trigger fires, add op-counters to `MonitorContext` /
  `run_loop` and assert them against N — no reaching for `cargo bench` and an arbitrary
  threshold.
- If an SLA ever appears (e.g. "monitor N=50 at a 100 ms rate without falling behind"), that
  reframes the question entirely and this ADR should be revisited — an absolute target makes a
  wall-clock benchmark meaningful for the first time.
