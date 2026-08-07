# 0019 — Loss is accounted over the observed span

**Status:** Accepted — implemented 2026-08-07. Delivers the Loss arithmetic of
[spec 0011](../../specs/0011-loss-reconstruction-and-gc-statistics.md) §4, on top of the
tiers [ADR 0017](0017-monitoring-tiers-follow-the-entry-layout.md) draws.

## Context

A 3.15 interpreter publishes each Collection through a ring of 11 Entries for generation 0 and
3 for the others. Collecting faster than gcscope polls, it overwrites Records unread, and what
survives is a biased sample: a long Collection holds its Entry longer and is likelier to be
caught. An operator looking at 40 pauses cannot tell 40 out of 40 from 40 out of 4,000.

Two cumulative fields make the rest recoverable. `collections` counts what a generation ran and
`duration` totals the pause it took, and both ride on every Record, so the difference between
what the counters say and what gcscope read is exact whatever the poll interval was. A run at a
1000 ms poll reads a quarter of `spin.py`'s gen-0 Records and reports the pause total a 50 ms
run measures directly.

That leaves what to difference against. A ring says nothing about the Collections that ran
before gcscope arrived: at attach it is already full of them, and no Entry says whether it was
overwritten unread or simply predates the Observer.

## Decision

1. **The accounted span runs from a ring's first Record read to its last.** What ran earlier is
   outside the account. Charging it as Loss reports Loss against every attach, and against a
   counter standing at 900 it reports 899.
2. **Both ends count on the tier where an Entry is a Collection.** A ring Entry describes one,
   so the Record that opened the span is one of them and the exact count is `last - first + 1`.
   An inline Entry is a snapshot of running totals and describes none, so there the count is the
   rise alone. Same field-presence test ADR 0017 draws the tier with, not a version comparison.
3. **Coverage is the observed share of the span,** and `0` on the counter-only tier, whose
   counts stand alone with no distribution behind them. That `0` is the tier's constant and
   holds for its idle generations too: a generation whose counter never moved reports `0.000`
   like its siblings, not the `1.0` that "lost none of the nothing it covers" would give. Only a
   ring no Record was ever read from reports `1.0`, which spares every call site a division
   guard.
4. **What ran is what was read plus what was lost, on both tiers.** The summary publishes all
   three, so the reconstruction is auditable against CPython's own counters. On the counter-only
   tier the Records read are not the reconciling term: two snapshots witness no Collection
   between them.
5. **The exact pause needs `duration`, asked for separately from the tier.** Timestamps price
   the Collections that were read; only the cumulative total prices the ones that were not. A
   ring carrying the first and not the second reports what it measured and leaves the exact
   figure absent. Differencing an absent field yields the measured sum and publishes it as what
   ran.
6. **The exact pause cannot fall below the pause actually measured.** CPython accumulates
   `duration` as a float of seconds while timestamps are integer nanoseconds, so a generation
   whose running total has outgrown its own precision subtracts to a hair under what gcscope
   watched directly. Flooring there keeps the lost pause off negative.
7. **The scale factor corrects figures that partition the pause, never a percentile.** Sub-phase
   totals have no cumulative counterpart in CPython but add up to the pause, so scaling their
   measured sum estimates the whole. A percentile describes a distribution's shape rather than
   its total, the sample behind it is biased, and multiplying it makes the bias look like a
   measurement. Percentiles stay sampled, with Coverage beside them.

## Consequences

- A total is correct under arbitrary Loss and only the distribution stays sampled, so "how much
  time went to GC" has one answer whatever `-r` the operator passed.
- A run reports Collections it holds no Record for, so the trace and the summary disagree by
  design. `records` and `coverage` say by how much.
- The first poll of a ring build admits every complete Entry the ring still holds, so
  Collections from interpreter startup land inside the span and are counted. They are not Loss,
  every one having been read, but a 4-second run of a script calling `gc.disable()` reports the
  handful that ran during startup. Excluding them needs a moment to compare against, and the
  target's clock is not the Observer's. That is a separate decision.
- `collections` and the pause cover the whole span while `collected` and `uncollectable` cover
  it minus the opening Record. All are running totals, and the total before the first Record was
  never read, so that Collection's objects are not recoverable the way its pause is. A span
  holding one Record reads `collections 1, collected 0`, and objects-per-collection computed off
  the table is low by `1/N`.
- Free-threaded builds write what the reconstruction reads without ordering it. The GIL writer
  publishes `ts_stop` last, with a comment saying why: "so remote readers do not select a
  partially updated stats record" (`Python/gc.c`). The free-threaded writer stores both
  timestamps, then `collections++`, then `duration +=` (`Python/gc_free_threading.c`). A poll
  landing in that window admits a Record whose counter has advanced and whose `duration` has
  not, and its young ring is one Entry deep, so the Entry is rewritten at the full collection
  rate. The error is one Collection's pause at each end of the span, the floor above catches the
  sign, and every later poll supersedes it. Nothing to fix from this side of the process
  boundary.
- The arithmetic is derived from the accumulator rather than stored beside it, so no figure can
  drift from the Records it came out of. It is reachable through the poll seam with scripted
  batches, and pinned by property tests over a simulated ring driven past its own capacity (ADR
  0005's trigger: a total over a large input space with stated invariants).
- The geometry that draws Loss as intervals stays out: window bounding, merging across
  generations, splitting around observed Collections. Those wait for the exporter that can give
  them a track of their own (spec 0011 §6).
