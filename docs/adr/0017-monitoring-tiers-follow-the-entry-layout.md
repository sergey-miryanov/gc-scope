# 0017 — Monitoring tiers follow the Entry layout, and an absent pause stays absent

**Status:** Accepted — implemented 2026-08-07. (Applies
[ADR 0003](0003-layout-driven-gc-stats-decode.md) and
[ADR 0007](0007-gcstat-layout-driven-view.md) to the *monitor*, which until now assumed
every build published what a ring build publishes. Delivers §4 of
[spec 0011](../../specs/0011-loss-reconstruction-and-gc-statistics.md).)

## Context

gcscope inspects CPython 3.8–3.16 and, until this decision, monitored almost none of it.

An inline Entry (3.8–3.14) holds three Lifetime totals — collections, collected,
uncollectable — and no timestamps at all. A ring Entry (3.15+) keeps those and adds
per-Collection detail: a start, a stop, a duration, a heap size, a candidate count
(`docs/version-support.md` §6). Everything the monitor did was built on the second shape.
Selection keyed on `ts_start` advancing, so on a build publishing no `ts_start` nothing was
ever selected and `gcscope monitor` wrote a trace containing no GC activity — against a
process collecting constantly, with no error and no warning.

Keying the cursor on the cumulative `collections` counter instead (ticket 02) made those
Records reach the exporter, and exposed the second half of the problem. The conversion still
described every Record as a pause, so each one drew a zero-width span at the epoch and
reported `duration: 0`, `candidates: 0`, `heap_size: 0`. An empty trace had become a trace
stating pause figures the interpreter never published — and `0.0 s` in a GC trace reads as
"this process spends no time in GC", which is a wrong answer where the empty trace was
merely a missing one.

Two ways to draw the line were available:

- **By version:** below 3.15, do less. Cheap, and wrong for the same reason the offset
  registry is version-keyed only at its edge — a custom build, a pre-release with a
  different field set, or an instrumented fork is described by its layout, not by the number
  on the tin. It would also put a version comparison in the monitor, the conversion and
  later the statistics, which is three more places to update per CPython release.
- **By field presence:** ask whether this Record's Entry layout defines `ts_start` and
  `ts_stop`. The layout is already the authority on what a build carries, `None`-means-absent
  is already the contract (ADR 0007), and the snapshot collector already reads builds this
  way.

## Decision

1. **The tier is `GcStat::has_timing()`** — whether the Entry layout defines both `ts_start`
   and `ts_stop` — and it is decided in exactly one place. `is_complete` reuses the same
   predicate, so "can this build describe a pause at all" has one answer. No version is
   compared in the monitor, the cursor or the conversion.
2. **A build with timing is unchanged**: a span per Collection with its sub-phases, on the
   target's own clock, byte-for-byte what it emitted before. The tier split is invisible to
   it, which the Chrome encoder's byte-identity gate keeps true.
3. **A build without timing produces counter samples and nothing else** — one per Collection
   per generation, carrying the Lifetime totals CPython does publish. Their rise over a run
   is the GC rate, which is what these builds can honestly show, and what gcmon (reading
   3.15's stdlib API) cannot show at all.
4. **A figure the build cannot supply is absent from the output, never zero.** No duration,
   no candidates, no heap size, and no zero-width span standing in for a pause. `uncollectable`
   keeps the timed tier's rule of riding along only when non-zero.
5. **Counter samples sit on the Observer's clock** — nanoseconds since monitoring began,
   passed into the conversion per poll rather than read inside it, so the conversion stays
   pure and the seam stays testable. A build with no timestamps has no other timeline, and
   stamping every sample at zero would collapse the run to a single point, which is a rate of
   nothing.

## Consequences

- `gcscope monitor` and `gcscope run` work on every build gcscope supports. A 3.12 operator
  gets GC rate per generation over time where they used to get `[]`.
- The two tiers are a property of the data, not a mode. A build whose layout gains
  timestamps produces spans the day its layout is registered, with no monitor change; a
  Probe ring publishing timing lands in the timed tier by the same rule.
- Both tiers are exercised as two layout fixtures through the poll seam
  (`MonitorContext::events_for`: Records in, `TraceEvent`s out), so no monitor test names a
  Python version. The live matrix asserts the tier against the target's own reported
  version — the one place a version legitimately appears, since it is the outside view of a
  decision made from the inside by field presence.
- Coverage on the counter-only tier is `0` — counts with no distribution behind them — which
  the statistics surface reports when it lands. It is not fabricated here.
- Pause time on these builds stays unavailable rather than estimated. Sampling the
  `collecting` flag would yield an aggregate duty cycle, not the per-generation figures the
  trace shows, and splitting an aggregate by count is fabrication (spec 0011 §4). If it is
  ever built it is a separately-named feature, never a backfill for absent data.
- One trace mixing tiers (a 3.12 parent spawning a 3.15 child into the same file) carries two
  timelines, since one tier is on the Observer's clock and the other on the target's. Tiers
  are per Record, so each track is internally consistent; nothing aligns them across the
  split, and nothing pretends to.
