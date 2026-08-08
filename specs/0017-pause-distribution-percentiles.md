# 0017 — Report the pause distribution, not only its total

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** M
- **Origin:** Split out of spec 0011 (now deleted), which shipped the totals and Coverage and
  left percentiles unbuilt. Grilling session 2026-08-08.
- **Respects:** [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) (`GcStat` is a
  layout-driven view), [ADR 0017](../docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md)
  (tiers follow the Entry layout), [ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md)
  (decision 7 in particular: the scale factor never touches a percentile),
  [ADR 0020](../docs/adr/0020-monitor-reads-every-interpreter.md) (what the accumulators cost
  per process)

## 1. Problem statement

`--summary` answers "how much time went to GC" exactly, and says nothing about how that time
was distributed. An operator seeing 30.9 ms of gen-0 pause over 205 Collections gets a mean of
151 µs and no way to tell a flat 151 µs from 204 Collections at 20 µs and one at 27 ms. The
second is a latency incident and the first is background noise, and the summary renders them
identically.

The trace holds the individual pauses, so the answer exists — behind an export, a viewer and
manual aggregation, which is the workflow `--summary` shipped to avoid. A CI job cannot do it
at all: `--summary-json` carries no per-Collection figure, so a p99 regression check has
nothing to threshold on.

Coverage already sits in both surfaces telling an operator whether a distribution beside it
would be trustworthy. There is no distribution beside it.

## 2. Solution

Both summary surfaces gain per-generation pause percentiles — p50, p90, p95, p99 — on builds
that time their Collections. They describe the Records gcscope actually read, which is a
biased sample whenever Coverage is below `1.0`, and they are never scaled to compensate.
Coverage is already printed and published next to them, which is what makes them honest.

On a build with no per-Collection timing there is no distribution to describe, and the keys
are absent rather than zero — the same rule the pause figures follow today.

## 3. User stories

1. As an operator, I want p50 through p99 of GC pause per generation, so that I can tell a
   flat distribution from one long Collection hiding inside a mean.
2. As a CI job, I want those percentiles in the JSON document, so that a latency regression
   fails a build without anyone exporting a trace.
3. As an operator on a pre-3.15 build, I want the percentile keys absent rather than zero, so
   that a threshold check cannot pass against a distribution the build never published.
4. As an operator whose Coverage is `0.43`, I want the percentiles reported as-is beside that
   Coverage rather than scaled up to the exact pause, so that I am reading a measurement and
   not an extrapolation.
5. As an operator, I want the table to stay readable at 100 columns, so that adding four
   figures does not cost me the columns I already use.
6. As a gcscope maintainer, I want the distribution to cost bounded memory per interpreter, so
   that a process running many sub-interpreters does not trade a bounded accumulator for an
   unbounded one.
7. As a gcscope maintainer, I want no new runtime dependency for this, so that the crate's
   dependency surface stays what it is.
8. As an operator already parsing the JSON document, I want new keys added without the
   existing ones moving or changing meaning, so that my consumer keeps working.

## 4. Implementation decisions

### Where the sample lives

`RingObservation` in `monitor/cursor.rs` gains the distribution beside the figures it already
folds. It is the only place a Record is seen once and only once, and it is already keyed
`(pid, interpreter, generation)`, which is the grain the percentiles are reported at.

**The sample is bounded, and the bound multiplies.** ADR 0020 caps a process at
`MAX_RETAINED_INTERPRETERS` accumulators, so any per-accumulator sample is paid 1024 times
over, times three generations. A 1024-entry reservoir of `i64` costs 24 MB per process at that
ceiling, which is not payable. Either the reservoir is small enough to survive the
multiplication, or the sample is held per interpreter rather than per generation, or the
retained-history bound is lowered for interpreters with a sample attached. Decide this before
writing anything: it is the constraint that picks the data structure, not a detail of it.

### Which estimator

Two candidates, both zero-dependency:

- **A fixed-size reservoir** of pause values, sorted on demand. gcmon's fallback path, at 1024
  entries. Exact for runs shorter than the reservoir, and a uniform sample past it.
- **A fixed-bucket histogram** over log-spaced pause ranges. Constant memory regardless of
  count, error bounded by bucket width, and no allocation after construction.

*Rejected:* a sketch crate (`ddsketch` or similar). gcmon makes it an optional extra and falls
back to a reservoir when it is absent, which means gcmon already ships the answer for how good
"good enough" is. Adding a runtime dependency to match an accuracy nobody has asked for is not
the trade this crate makes.

### The figures are sampled and stay sampled

`GenerationSummary` gains `pause_p50_ns` … `pause_p99_ns` as `Option<i64>`, gated on the same
field presence that gates `pause_measured_ns` — a percentile needs a per-Collection pause, and
that needs both timestamps.

`scale_factor` must not touch them. ADR 0019 decision 7 states why: a percentile describes a
distribution's shape rather than its total, the sample behind it is biased toward long
Collections, and multiplying it makes the bias look like a measurement. The type should make
that hard to get wrong rather than the comment.

### The table has run out of width

A timed row is already near 100 characters (ticket 07 dropped `observed` and `lost` from the
table for exactly this reason, keeping them in the JSON). Four more columns do not fit. Three
ways out, to be decided when this is picked up: a second row per generation, a `--summary-wide`
flag, or percentiles in the JSON only with the table keeping the mean. The JSON is the surface
story 2 needs and the table is the one story 5 protects, so they may honestly diverge here.

### The JSON grows, the schema does not

Adding a key is not a schema-version bump; renaming or removing one is. `docs/summary-json.md`
gains the new rows and the absence rule already covers their omission on the counter-only
tier. The byte-for-byte pin in `summary_json.rs` makes the addition deliberate.

## 5. Seams and testing decisions

- **Seam:** the poll seam — scripted batches of `GcStat` in, the folded
  `Vec<InterpreterSummary>` out. Same seam the totals and Coverage are tested through.
- **New seam needed:** none.
- **What makes a good test here:** the estimator's accuracy against a known distribution, and
  the surfaces' behaviour against a build without timing. Assert percentile *bounds* for an
  approximate estimator rather than exact values, so the test states the accuracy the design
  promises.
- **Prior art:** `statistics.rs`'s existing summary tests; ADR 0005's property-test trigger
  applies here (a total over a large input space with stated invariants).
- **Cases:**
  1. A known distribution of pauses folded through the accumulator reports percentiles within
     the estimator's stated error.
  2. Ordering holds: `p50 <= p90 <= p95 <= p99 <= max`, under any input.
  3. A counter-only layout produces no percentile keys in the JSON and no percentile columns
     in the table.
  4. A generation with one Record reports that Record's pause at every percentile.
  5. The regression guard: an existing JSON document's other keys are byte-identical, with
     only the new keys added.
  6. Memory per accumulator is bounded and stated, exercised by folding far more Records than
     the sample can hold.

## 6. Out of scope

- **Percentiles over anything but pause** — heap size, objects collected, sub-phase durations.
  Same machinery, no demand stated.
- **Reconstructing the lost part of the distribution.** It is not reconstructible: Loss is
  exact in count and total precisely because CPython accumulates those two and nothing else.
- **Statistics replayed from a written trace,** which arrives with the JSONL exporter.
- **Percentiles in the trace.** They are an end-of-run figure.

## 7. Further notes

The memory question in §4 is the one that must be settled first, and it may send this spec
back to ADR 0020's bound rather than forward to an estimator.
