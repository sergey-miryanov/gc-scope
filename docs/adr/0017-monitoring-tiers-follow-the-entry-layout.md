# 0017 — Monitoring tiers follow the Entry layout

**Status:** Accepted — implemented 2026-08-07. Applies
[ADR 0003](0003-layout-driven-gc-stats-decode.md) and
[ADR 0007](0007-gcstat-layout-driven-view.md) to the monitor, which assumed every build
published what a ring build publishes. Delivers §4 of spec 0011, now deleted.

## Context

An inline Entry (3.8–3.14) holds three Lifetime totals: collections, collected,
uncollectable. A ring Entry (3.15+) keeps those and adds per-Collection detail, the start and
stop timestamps among it (`docs/version-support.md` §6). The monitor was built on the second
shape. Selection keyed on `ts_start` advancing, so where no `ts_start` exists nothing was ever
selected: `gcscope monitor` wrote a trace with no GC activity against a process collecting
constantly, with no error and no warning.

Keying the cursor on the cumulative `collections` counter (ticket 02) got those Records as far
as the exporter, which exposed the rest. The conversion still described every Record as a
pause, so each drew a zero-width span at the epoch reporting `duration: 0`. An operator reads
that as "this process spends no time in GC", which is worse than the empty trace.

Two ways to draw the line:

- **By version:** below 3.15, do less. Wrong for the reason the offset registry keys on
  version only at its edge: a custom build, an odd pre-release or an instrumented fork is
  described by its layout, not by the number on it. It also puts a version comparison in the
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
3. **A build without timing produces counter samples**, one per generation per poll whose
   count advanced, carrying the Lifetime totals CPython does publish. Their rise over a run is
   the GC rate. None describes a single Collection: an inline Entry holds a running total, so
   many Collections between two polls arrive as one step.
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
  against the target's reported version, which is the outside view of a decision the code
  makes from field presence.
- Coverage on the counter tier is `0`: counts with no distribution behind them, since nothing
  these builds publish is per-Collection. The statistics surface reports that when it lands.
- Pause time on these builds stays unavailable rather than estimated. Sampling the
  `collecting` flag yields an aggregate duty cycle, and splitting one per generation by count
  is fabrication. Worth revisiting as a separately-named feature, never as a backfill for
  absent data.
- One file mixing tiers, a 3.12 parent spawning a 3.15 child, carries two timelines: one tier
  rides the Observer's clock and the other the target's. Tiers are per Record, so each track
  holds together, and nothing aligns them across the split.
