# 0011 — Reconstruct lost collections and report exact GC statistics

- **Status:** In progress. The `TraceEvent` extraction, the counter-keyed cursor, the
  counter-only tier, the `--summary` table, the Loss arithmetic, the JSON form and reading
  every interpreter have all landed
  ([ADR 0017](../docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md),
  [ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md),
  [`docs/summary-json.md`](../docs/summary-json.md)). Percentiles and drawn Loss spans have
  not; §7's two open questions are still open.
- **Kind:** feature — enhancement
- **Effort:** L
- **Origin:** Grilling session 2026-08-05 on porting gcmon's consumer stack into gcscope.
  gcmon (`X:/Work/gc-monitor/gcmon`) is a sibling tool, not an ancestor: it reads GC
  Records through CPython 3.15's stdlib `_remote_debugging.get_gc_stats()` and therefore
  cannot run below 3.15, but everything it does *downstream* of that read is the
  complement of what gcscope has. Background inventory:
  [`docs/research/gcmon-inventory.md`](../docs/research/gcmon-inventory.md).
- **Respects:** [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md) (decode keyed
  by layout, not version), [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) (`GcStat` is a layout-driven
  view), [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)
- **Blocked by:** nothing. The in-flight-entry fix shipped first and standalone in `ce747cb`,
  as `GcStat::is_complete` (`ts_start < ts_stop`, gated on the layout) filtering both
  `select_fresh` and `parse_gc_entries`. §4 below still replaces that cursor outright.

## 1. Problem statement

An operator monitoring a CPython process gets a trace that is wrong in two different ways
depending on which interpreter they attached to, and gcscope tells them neither.

**Below 3.15, the trace is empty.** Those builds publish `collections`, `collected` and
`uncollectable` per generation and no timestamps at all. Selection requires
`ts_start` to advance, so nothing is ever selected and `gcscope monitor` writes a trace
containing no GC activity — against a process that is collecting constantly. There is no
error and no warning. gcscope supports 3.8 through 3.16 for inspection, and monitors none
of it below 3.15.

**From 3.15 the trace is incomplete, by an unreported amount.** Records are published
through a fixed-size sequence — 11 Entries for generation 0, 3 for the others — so an
interpreter collecting faster than gcscope polls overwrites Records before anyone reads
them. The trace shows the Collections that survived to a poll, which is a *biased* sample:
a long Collection occupies its Entry for longer and is likelier to be seen. An operator
looking at 40 GC pauses cannot tell whether that is 40 out of 40 or 40 out of 4,000, and
any total computed from them is an undercount of unknown size.

There is also no statistics surface of any kind. An operator who wants "how much time did
this process spend in GC" must export a trace and aggregate it themselves.

## 2. Solution

Monitoring works on every build gcscope supports, and reports its own fidelity.

**On 3.15 and later**, gcscope reconstructs what it missed. CPython's cumulative
`collections` and `duration` counters ride on every Record, so the difference between what
the counters say ran and what gcscope actually read gives the exact number of lost
Collections and the exact pause time they consumed. Totals become correct even under heavy
Loss; only the distribution stays sampled. Every summary figure is accompanied by
**Coverage** — the share of Collections actually read — so an operator knows whether a
percentile beside it is the real distribution or the tail of a biased sample.

**Below 3.15**, gcscope monitors for the first time. Those builds have no per-Collection
Record and no timing, so there are no spans; what they do have is an exact cumulative count
per generation, which becomes counter tracks showing GC rate over time. Coverage there is
`0` — the counts stand alone, with no distribution behind them — and pause figures are
reported as **absent**, never as zero.

**Both tiers** gain an end-of-run summary: collections, collected, uncollectable and
coverage per generation, plus pause totals where the build has them. Available as a human
table or as JSON.

## 3. User stories

1. As an operator attaching to a production interpreter on 3.12, I want `gcscope monitor`
   to produce a trace containing GC activity, so that the command is usable on the build I
   actually run.
2. As an operator on a pre-3.15 build, I want to see GC rate per generation over time, so
   that I can correlate collection frequency with load without needing pause timing.
3. As an operator on a pre-3.15 build, I want pause statistics reported as absent rather
   than `0.0`, so that I do not conclude the process spends no time in GC.
4. As an operator on 3.15+, I want the reported collection count to be the number that
   actually ran, so that I can trust a total without knowing gcscope's poll interval.
5. As an operator on 3.15+, I want total GC pause time to be exact even when Records were
   overwritten, so that "how much time went to GC" has one correct answer.
6. As an operator, I want Coverage reported next to every sampled figure, so that I know
   whether a p99 is a real distribution or a biased tail.
7. As an operator whose process runs sub-interpreters, I want Collections from every
   interpreter recorded, so that activity outside interpreter zero is not silently
   dropped.
8. As an operator whose process runs sub-interpreters, I want each interpreter's Loss
   accounted separately, so that a busy interpreter's Loss is not attributed to a quiet
   one.
9. As a developer profiling my own script, I want an end-of-run summary without opening a
   trace viewer, so that a quick question has a quick answer.
10. As a developer, I want that summary as JSON, so that I can feed it to another tool.
11. As a CI job, I want the JSON summary to omit fields the build cannot supply rather than
    emit placeholder zeros, so that a threshold check cannot silently pass against absent
    data.
12. As an operator, I want a Collection that was in progress at the moment of a poll to be
    recorded once it completes, so that polling does not itself destroy data.
13. As an operator, I want a Collection in progress to bound the Loss interval rather than
    be discarded, so that reconstruction uses the strongest evidence available about when
    gcscope last had certainty.
14. As an operator already using `gcscope monitor` on 3.15+, I want my existing Chrome
    traces to keep the same slice names, categories and argument keys, so that saved
    analyses keep working.
15. As a gcscope maintainer, I want the tier chosen by which fields the Entry layout
    carries rather than by comparing version numbers, so that a new CPython layout is
    handled by registering it and nothing else.
16. As a gcscope maintainer, I want one conversion from Records to trace events shared by
    every output format, so that adding a sub-phase or a record type is one edit.
17. As a gcscope maintainer, I want the Loss arithmetic reachable without a live
    interpreter, so that its correctness is pinned by fast tests rather than the CI matrix.
18. As a gcscope maintainer adding a Python version, I want no per-version branch in the
    monitor, statistics or conversion code, so that the offset registry stays the only
    place versions are enumerated.
19. As an operator, I want reconstructed Loss totals to add up to the difference between
    observed and cumulative counts exactly, so that the arithmetic is auditable against
    CPython's own counters.
20. As an operator monitoring a process tree, I want per-process cursors dropped when a
    process exits, so that a reused PID does not inherit a dead process's state.

## 4. Implementation decisions

### A format-independent event model, extracted first

A `TraceEvent` model — begin, end, instant, counter, process metadata, thread metadata —
becomes the contract between production and encoding. One conversion turns a `GcStat`, a
Loss record or an instant message into events; each output format does nothing but encode
them.

This is extracted **before** any new format is written, with the Chrome encoder refactored
onto it and its output proven byte-identical for the same input. gcmon reached the same
conclusion the expensive way: its Chrome and Perfetto paths each converted independently,
reimplemented the same sub-phase discovery, names, categories and counter collection, and
drifted until they produced two disagreeing traces of one run (gcmon ADR-0007).

gcscope starts from a better position: the sub-phase policy is already data, in the
`PHASES` table the Chrome encoder walks. Extraction is largely redirecting that table's
consumer.

*Rejected:* letting a second format convert from `GcStat` directly and extracting later.
That is precisely the state gcmon had to undo.

### The cursor becomes a per-ring accumulator keyed on `collections`

`select_fresh`'s per-`(generation, entry)` high-water mark on `ts_start` is replaced by one
accumulator per `(pid, interpreter, generation)`, cursored on the cumulative `collections`
counter. Forced twice over: pre-3.15 builds have no `ts_start` to key on, and a per-Entry
high-water mark cannot detect a *gap*, which is the whole of Loss detection.

Each accumulator holds what its ring did against what gcscope saw of it: the counter and
cumulative duration at the first and last observed Record, the number of Records sampled,
and the pause time measured across them. From those, `exact_count`, `exact_pause`,
`lost_count`, `coverage` and `scale_factor` are derived rather than stored.

The monitor reads **all** interpreters rather than only the first, since the interpreter is
part of the key. Cheap enough to do every tick: the layout, the version and the runtime
address are resolved once at attach, so a tick costs each interpreter its id, its `next` link
and one read of its stats region.

CPython links a new interpreter at the *head* of the chain and never reuses an id. Both facts
shape this. The first is why a head-only read looks like it works — the head changes, so
several interpreters do reach a long capture, each in a window of its own — and so the live
check is that every interpreter is read *throughout* the run rather than merely present. The
second is why the accumulators are capped per process: without one, a workload creating and
destroying sub-interpreters holds an accumulator per `(interpreter, generation)` until the
process exits. The cap makes room by dropping the interpreter seen least recently, so what
goes is one that stopped appearing in the chain and never one still collecting. The run says
how many went.

Walking that chain every tick, without the lock CPython holds over it, is also what makes a
torn read reachable: the walk ends on an address it has already visited, skips a link whose
id reads back negative, and lets a failure past the head cost that interpreter rather than
the poll — an interpreter torn down mid-walk would otherwise take every other interpreter's
Records with it and route a live process into the give-up ladder.

### Completeness is a producer-side filter, and the excluded Entry is evidence

An Entry whose Collection is still running has `ts_start` published and `ts_stop` not yet
written. Such an Entry is not a Record and must not reach any encoder — filtered once at
the producer, not per format. gcmon's ADR-0007 moved this filter producer-side for the same
reason two copies of it had already caused divergence.

Excluding it is not enough: its `ts_start` is the strongest available proof of when gcscope
last had certainty about that interpreter, stronger than the newest *finished* Record,
because a Collection that had started is later evidence than one that had ended. It
therefore bounds any Loss window opened afterwards, and it survives the Record never coming
back.

### Tiers are chosen by layout, never by version

Everything version-dependent resolves through field presence on the Entry layout — whether
`ts_start`/`ts_stop` exist, whether `duration` exists — using the existing layout-driven
view (ADR 0007) and the idiom already used by the snapshot collector. No comparison against
a version number appears in the monitor, the conversion or the statistics.

Consequently the two tiers are a property of the data, not a mode: a build with timing
fields produces spans and pause figures; one without produces counters and Coverage `0`.

### Loss reconstruction

**Shipped — see [ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md)**,
which records the accounted span, the tier-dependent count, Coverage, the `duration` gate and
the pause floor.

**Only the accounting lands here, not the span geometry.** Turning gaps into drawn intervals
needs each bounded into a window, windows merged across an interpreter's generations, the
result split around Collections that *were* observed, and counts and pause apportioned across
the pieces. That belongs with the Perfetto increment, where gcmon's ADR-0015 puts Loss spans on
a track of their own, "own track" being a Perfetto concept and not a Chrome one. This spec
delivers the numbers: how many Collections ran, how many were read, how much pause is
unaccounted, per generation per interpreter.

*Rejected:* estimating pre-3.15 pause time by sampling the `collecting` flag. That flag is a
single value, so it yields an aggregate GC duty cycle rather than per-generation figures, and
splitting an aggregate across generations by count would be fabrication. Worth revisiting as a
separately-named feature, never as a backfill for absent data.

### Statistics fold live and emit at end of run

A streaming accumulator folds Records as they are polled, producing per-generation collections,
collected, uncollectable, Coverage, and, where the build has timing, pause total and mean with
their scale factor. Absent fields are omitted rather than defaulted.

The table shipped behind `--summary` and the JSON document behind `--summary-json`, both
rendering one folded summary so they cannot disagree. Its schema is in
[`docs/summary-json.md`](../docs/summary-json.md), where a figure the build cannot supply has
no key (story 11).

**Percentiles have not shipped**, and neither has the replay-from-file path: reconstructing
this from a written trace is what the pyperf hook needs, and it arrives with the JSONL
exporter. The accumulator takes a stream of Records from either source, the only concession
made to that future.

## 5. Seams and testing decisions

- **Seam:** the **poll seam** — scripted batches of `GcStat` in, recorded `TraceEvent`s and
  the statistics aggregate out. It is the highest seam that observes dedup by counter,
  in-flight handling, multi-interpreter keying, Loss window opening/merging/splitting, tier
  selection and every record type, without a live interpreter. A second, lower seam exists
  where the intermediate meets each encoder: `TraceEvent`s in, bytes out.
- **New seam needed:** none at the poll level — `monitor::context::select_fresh` is already
  an extracted seam of exactly this shape (ADR 0005 names it as one of the two extracted
  for this purpose), and `EventsExporter` is already a trait, so a recording implementation
  is the test double. The encoder seam is *created* by the `TraceEvent` extraction rather
  than added on top of it.
- **What makes a good test here:** external behaviour only — what reaches the exporter and
  what the aggregate reports, never how the accumulator stores it. Tiers are expressed as
  two layout fixtures (a counters-only field set versus the full ring field set), so no
  test mentions a version. For the live layer, assert decoded **shape** against the
  target's own `sys.version_info`, never a hardcoded expectation.
- **Prior art:** `select_fresh`'s existing unit tests, which build `GcStat`s with
  `GcStat::from_fields` over a `seq_layout` fixture; `tests/live_smoke.rs` for the live
  layer; gcmon's `test_monitor_cursor.py` and `test_loss.py` for the case catalogue.
- **Cases:**
  1. A build whose layout has no timing fields produces counter events and no spans, with
     Coverage `0` and pause figures absent.
  2. A build with timing produces spans, and a gap in the counter sequence is accounted as
     Loss whose count equals the counter delta minus the Records observed.
  3. An in-flight Entry produces nothing, does not advance the cursor, and the Record is
     emitted when a later poll finds it complete.
  4. Records from a second interpreter are recorded and accounted against their own
     accumulator.
  5. **The regression guard:** for any input, the Chrome encoder's bytes are identical to
     those produced before the `TraceEvent` extraction.
- **Up the ladder:** Loss is total over a large input space with stated invariants, which
  is the policy's own trigger for randomized property tests — parts sum to the whole
  (`exact_count == sampled_count + lost_count`; apportionment preserving its total; a split
  span preserving counts across its pieces), Coverage within `[0, 1]`, lost pause never
  negative. Fixed seed, per policy. gcmon covers this ground with a 39 KB example-based
  file; properties pin more of it for less.

## 6. Out of scope

- **The Perfetto exporter.** Decided (hand-rolled in Rust, translating gcmon's, with its
  wire-level tests) but deliberately the next increment, so that the Loss arithmetic lands
  against an exporter that already works and a wrong number is the only variable.
- **Loss spans drawn in the trace** — window bounding, cross-generation merging, splitting
  around observed Collections, proportional apportionment. Deferred to the Perfetto
  increment for the reason in §4: they want a track of their own, which Chrome has no way
  to express. This spec delivers Loss as *numbers* in the statistics surface; the geometry
  that turns those numbers into drawn intervals comes with the exporter that can show them
  properly.
- **The JSONL and stdout exporters**, and therefore statistics replayed from a written
  trace.
- **The control plane and the pyperf hook**, and the Python client package they require —
  the last increment, and the least valuable.
- **Offline `convert`/`combine`.** Nearly free once the intermediate exists, which is why
  it can wait.
- **RSS sampling**, `--duration`, and the environment-variable option surface.
- **Duty-cycle pause estimation for pre-3.15 builds**, per §4.
- **The in-flight-entry fix itself**, which shipped first as its own change (`ce747cb`) so
  that a live correctness bug was not held hostage to this work.

## 7. Further notes

**Open questions to settle when this is picked up.**

- Whether the accumulator validates that a poll's Records are contiguous, or trusts it as
  gcmon does. gcmon trusts it because CPython's own `get_gc_stats()` hands it the ring;
  gcscope performs the raw read itself and is at least as exposed to a torn read. Cheap to
  check, and the failure mode without it is silent.
- Whether Chrome trace output must stay compatible with existing gcmon captures and the
  analysis notebooks that read them. Worth answering before Perfetto, not before this.

**Decisions that graduate to ADRs when this lands**, per the folder's lifecycle rule: the
two-tier version reach (why monitoring does less below 3.15, and why the line is drawn at
field presence rather than version) is durable and surprising enough to need one. The
`TraceEvent` extraction is well-precedented by gcmon's ADR-0007 and needs one only if the
Rust shape diverges from it.

**Vocabulary** for this work is in [`CONTEXT.md`](../CONTEXT.md): Observer, Observed,
Collection, Record, Entry, Loss, Coverage, Lifetime total. Note that gcmon's own naming
inverts the control relationship (it calls the listening side "parent" and the connecting
side "child", which is backwards under the pyperf hook and meaningless under attach); the
glossary's Observer/Observed replaces it, and ported code should not carry the old terms
across.
