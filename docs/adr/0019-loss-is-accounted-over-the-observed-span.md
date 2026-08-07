# 0019 — Loss is accounted over the observed span

**Status:** Accepted — implemented 2026-08-07. Delivers the Loss arithmetic of
[spec 0011](../../specs/0011-loss-reconstruction-and-gc-statistics.md) §4, on top of the
tiers [ADR 0017](0017-monitoring-tiers-follow-the-entry-layout.md) draws.

## Context

A 3.15 interpreter publishes each Collection through a ring of 11 Entries for generation 0 and
3 for the others. Collecting faster than gcscope polls, it overwrites Records before anyone
reads them, and what survives is a *biased* sample: a long Collection holds its Entry longer
and is likelier to be caught. An operator looking at 40 pauses cannot tell 40 out of 40 from 40
out of 4,000.

Two cumulative fields make the rest recoverable. `collections` counts the Collections a
generation has run and `duration` totals the pause they took, and both ride on every Record. So
the difference between what the counters say and what gcscope read is exact, whatever the poll
interval was — a run at a 1000 ms poll reads a quarter of `spin.py`'s gen-0 Records and still
reports the same pause total a 50 ms run measures directly.

That leaves the question of *what* to difference against. A ring gives no answer for the
Collections that ran before gcscope arrived: at attach the ring is already full of them, and
nothing in an Entry says whether it was overwritten unread or simply predates the Observer.

## Decision

1. **The accounted span runs from the first Record read of a ring to the last.** Everything
   before it is outside the account. Charging it as Loss would report Loss against every
   attach, and against a counter standing at 900 it would report 899.
2. **Both ends of the span count, on the tier where an Entry is a Collection.** A ring Entry
   describes one, so the Record that opened the span is one of them and the exact count is
   `last - first + 1`. An inline Entry is a snapshot of running totals and describes none, so
   there the count is the rise alone. This is the same field-presence test ADR 0017 draws the
   tier with, not a version comparison.
3. **Coverage is the observed share of the span,** and `0` on the counter-only tier: nothing
   those builds publish is per-Collection, so the counts stand alone with no distribution
   behind them. A ring nothing ran on reports `1.0` — it lost none of the nothing it covers,
   and every call site would otherwise guard a division.
4. **The exact pause cannot fall below the pause actually measured.** CPython accumulates
   `duration` as a float of seconds while timestamps are integer nanoseconds, so a generation
   whose running total has outgrown its own precision subtracts to a hair under what gcscope
   watched with its own eyes. Flooring there is what keeps the lost pause off negative.
5. **The scale factor corrects a figure that partitions the pause, never a percentile.**
   Sub-phase totals have no cumulative counterpart in CPython but add up to the pause, so
   scaling their measured sum estimates the whole. A percentile describes the shape of a
   distribution rather than its total; the sample it comes from is biased, and multiplying it
   makes the bias look like a measurement. Percentiles stay sampled, with Coverage beside them.

## Consequences

- A total is correct under arbitrary Loss and only the distribution stays sampled, which is
  what makes "how much time went to GC" have one answer whatever `-r` the operator passed.
- A run reports Collections it has no Record for, so the trace and the summary disagree by
  design. `records` and `coverage` are what say by how much.
- The first poll of a ring build admits every complete Entry the ring still holds, so
  Collections from interpreter startup land inside the span and are counted. They are not Loss
  — every one of them was read — but a 4-second run of a script calling `gc.disable()` reports
  the handful that ran during startup. Excluding them needs a moment to compare against, and
  the target's clock is not the Observer's; that is a separate decision from this one.
- The arithmetic is derived from the accumulator rather than stored beside it, so a figure
  cannot drift from the Records it came out of. It is reachable through the poll seam with
  scripted batches, and pinned by property tests over a simulated ring driven past its own
  capacity (ADR 0005's trigger: a total over a large input space with stated invariants).
- Nothing here needs the geometry that draws Loss as intervals — window bounding, merging
  across generations, splitting around observed Collections. Those wait for the exporter that
  can give them a track of their own (spec 0011 §6).
