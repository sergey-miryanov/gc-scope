# 0017 — Monitoring tiers follow the Entry layout

**Status:** Accepted — implemented 2026-08-07. Applies
[ADR 0003](0003-layout-driven-gc-stats-decode.md) and
[ADR 0007](0007-gcstat-layout-driven-view.md) to the monitor, which assumed every build
published what a ring build publishes. Delivers §4 of
[spec 0011](../../specs/0011-loss-reconstruction-and-gc-statistics.md).

## Context

An inline Entry (3.8–3.14) holds three Lifetime totals: collections, collected,
uncollectable. A ring Entry (3.15+) keeps those and adds per-Collection detail, including the
start and stop timestamps (`docs/version-support.md` §6). The monitor was built on the second
shape. Selection keyed on `ts_start` advancing, so where no `ts_start` exists nothing was ever
selected: `gcscope monitor` wrote a trace with no GC activity against a process collecting
constantly, and reported neither an error nor a warning.

Keying the cursor on the cumulative `collections` counter (ticket 02) got those Records as far
as the exporter, which exposed the rest. The conversion still described every Record as a
pause, so each one drew a zero-width span at the epoch reporting `duration: 0`. An operator
reads that as "this process spends no time in GC" — a wrong answer where the empty trace was
a missing one.

Two ways to draw the line:

- **By version:** below 3.15, do less. Wrong for the reason the offset registry keys on
  version only at its edge — a custom build, a pre-release with a different field set or an
  instrumented fork is described by its layout. It also puts a version comparison in the
  monitor, the conversion, and later the statistics.
- **By field presence:** ask whether this Record's layout defines `ts_start` and `ts_stop`.
  The layout is the authority on what a build carries, `None`-means-absent is the contract
  (ADR 0007), and the snapshot collector reads builds this way already.

## Decision

1. **The tier is `GcStat::has_timing()`** — does the Entry layout define both timestamps —
   decided in one place. `is_complete` reuses it, so "can this build describe a pause" has one
   answer.
2. **A build with timing is unchanged:** a span per Collection with its sub-phases, on the
   target's own clock, byte-for-byte what it emitted before. The Chrome encoder's
   byte-identity gate keeps that true.
3. **A build without timing produces counter samples**, one per Collection per generation,
   carrying the Lifetime totals CPython does publish. Their rise over a run is the GC rate.
4. **A figure the build cannot supply is absent, not zero.** No duration, no candidates, no
   heap size, and no zero-width span standing in for a pause.
5. **Counter samples sit on the Observer's clock,** passed into the conversion per poll rather
   than read inside it, so the conversion stays pure. Stamping every sample at zero would
   collapse a run to one point.

## Consequences

- `gcscope monitor` and `gcscope run` work on every build gcscope supports. A 3.12 operator
  sees GC rate per generation where they saw `[]`.
- A build whose layout gains timestamps produces spans the day that layout is registered, with
  no monitor change. A Probe ring publishing timing lands in the timed tier by the same rule.
- Both tiers are reachable through the poll seam (`MonitorContext::events_for`) as two layout
  fixtures, so no monitor test names a Python version. The live matrix asserts the tier
  against the target's reported version — the outside view of a decision the code makes from
  field presence.
- Coverage on the counter tier is `0`: counts with no distribution behind them. The statistics
  surface reports it when that lands; nothing fabricates it here.
- Pause time on these builds stays unavailable rather than estimated. Sampling the
  `collecting` flag yields an aggregate duty cycle, and splitting an aggregate per generation
  by count is fabrication (spec 0011 §4).
- One file mixing tiers — a 3.12 parent spawning a 3.15 child — carries two timelines, since
  one tier rides the Observer's clock and the other the target's. Tiers are per Record, so
  each track holds together; nothing aligns them across the split.
