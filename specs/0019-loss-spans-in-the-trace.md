# 0019 — Draw Loss as intervals in the trace

- **Status:** Blocked (spec 0018 — Loss spans want a track of their own, which Chrome cannot
  express)
- **Kind:** feature — enhancement
- **Effort:** M
- **Origin:** Split out of spec 0011 (now deleted), which shipped Loss as *numbers* and
  deferred the geometry that turns them into drawn intervals. Carries that spec's user story
  13, the only one it left unfinished.
- **Respects:** [ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md) (the
  accounted span and what may be scaled),
  [ADR 0020](../docs/adr/0020-monitor-reads-every-interpreter.md) (the per-interpreter
  accumulators this reads, and their bound),
  [ADR 0017](../docs/adr/0017-monitoring-tiers-follow-the-entry-layout.md) (this tier exists
  only where the layout carries timestamps)

## 1. Problem statement

`--summary` reports that 117 of a generation's 205 Collections were never read and that they
consumed 17.6 ms of pause. The trace shows the 88 that were read and nothing else, so an
operator correlating a latency spike against GC activity sees a quiet stretch where in fact
gcscope was blind. The numbers say Loss happened; the picture says it did not, and the picture
is what anyone looks at.

Worse, the two disagree in a direction that reads as reassurance. A gap in the trace looks like
a gap in GC activity.

## 2. Solution

Each interpreter gains a Loss track beside its Collections. Where gcscope holds no Record for
Collections it knows ran, it draws the interval it was blind over, annotated with how many
Collections went unseen and how much pause they consumed. An operator looking at a gap can
tell "the GC was idle here" from "gcscope could not see here", which today they cannot.

A run with full Coverage draws no Loss slices at all.

## 3. User stories

1. As an operator, I want the intervals gcscope was blind over drawn in the trace, so that a
   quiet stretch is not mistaken for an idle collector.
2. As an operator, I want each Loss slice annotated with the Collections and pause it covers,
   so that the picture and the summary tell the same story.
3. As an operator, I want the slice's width to be the blind interval rather than the lost
   pause, so that I am not misled into reading a 4-second slice as a 4-second stall.
4. As an operator, I want a Loss slice never to overlap a Collection gcscope did read, so that
   the trace does not claim blindness over a moment it has a Record for.
5. As an operator whose Coverage is `1.0`, I want no Loss track at all, so that the addition
   costs nothing on a run that lost nothing.
6. As an operator, I want a Collection that was in flight at a poll to bound the interval,
   rather than being discarded, so that reconstruction uses the strongest evidence available
   about when gcscope last had certainty.
7. As an operator on a build with no per-Collection timing, I want no Loss track, so that a
   tier with no timestamps does not grow drawn intervals it cannot place.
8. As a gcscope maintainer, I want the counts and pause apportioned across a split interval to
   sum back to the unsplit totals, so that the drawn figures reconcile with the summary's.

## 4. Implementation decisions

### The certainty bound is already built, and this spec is its only consumer

`Cursor::last_certainty` holds, per `(pid, interpreter)`, the latest moment gcscope knows the
interpreter had reached. It is raised from two places: a finished Record's `ts_stop`, and an
in-flight Entry's `ts_start` — the Entry excluded from the trace for having no `ts_stop` yet.
That exclusion is what spec 0011 decided; keeping its start is what makes it evidence. A
Collection that had *started* is later proof than one that had *ended*, and it survives the
Record never coming back.

The bound exists only at the poll that catches the in-flight Entry. The ring overwrites that
Entry within a few polls, no trace file carries it, and no replay path can recover it — which
is why the code shipped ahead of its consumer rather than waiting for this spec.

gcmon does the same thing more narrowly: `_in_flight` carries the newest incomplete `ts_start`
to the next poll as a lower bound on the window (`monitor.py:36-49`, `:146-151`), and
`confirmed_by_interpreter` supplies "nothing was lost before this read". gcscope's single bound
covers both.

### Four steps, in order, all pure

Translating gcmon's `loss.py:189-298`:

1. **Bound each gap into a window.** A run of Collections whose counter skipped is blind
   between the previous Record's stop and this one's start, floored at the interpreter's
   certainty bound.
2. **Merge windows across an interpreter's generations.** Collections in one interpreter are
   serialized, so a gen-0 window and a gen-2 window overlapping describe one blind interval,
   not two. Merging keeps the track laminar.
3. **Split around Collections that were observed.** No lost Collection ran during one gcscope
   holds a Record for.
4. **Apportion counts and pause across the pieces** by width, largest-remainder so the parts
   sum back to the whole.

All four are arithmetic over the accumulator, so they belong beside `monitor/loss.rs` and are
reachable through the poll seam with no live interpreter.

### The slice is the blind interval, and says so

Width is the interval gcscope could not see, not the pause consumed inside it — those differ by
orders of magnitude, and reading one as the other turns a 4-second blind spot into a reported
4-second stall. The lost pause is an annotation. gcmon documents this trap in its
`docs/formats.md`, which is evidence it caught someone.

### The track, and why this is blocked

One track per interpreter, `GC Loss {iid}`, beside that interpreter's Collections. gcmon
reserves negative tids (`-2, -3, …`) to express it in Chrome and gives it a real track in
Perfetto (its ADR-0015). Chrome has no concept of a track belonging to anything but a process
or a thread, so the Chrome expression is a convention a reader has to be told about, while the
Perfetto one is the thing itself. That is why spec 0011 sent this here and why this spec is
blocked on 0018 rather than shipping into Chrome alone.

Whether Chrome also gets the negative-tid convention is a decision for when 0018 lands.

### Scaling

The apportioned pause partitions a total, so ADR 0019 decision 7 permits the scale factor
here — this is the case that rule was written to allow, as distinct from the percentile it was
written to forbid.

## 5. Seams and testing decisions

- **Seam:** the poll seam — scripted batches of `GcStat` in, recorded `TraceEvent`s out. The
  whole of this is observable there, including the in-flight bound, which needs two polls and
  no target.
- **New seam needed:** none.
- **What makes a good test here:** the drawn figures against the summary's, and the invariants
  a split must preserve. Never how a window is stored.
- **Prior art:** `monitor/loss.rs`'s existing tests and its property tests; gcmon's
  `test_loss.py` for the case catalogue.
- **Cases:**
  1. A gap in the counter sequence draws one slice, annotated with the counter delta.
  2. A slice never overlaps an observed Collection.
  3. Windows from two generations of one interpreter merge into one laminar interval.
  4. An in-flight Entry at poll N bounds the window opened at poll N+1, and the same holds
     when its Record never returns.
  5. A run with full Coverage draws no slice.
  6. A counter-only layout draws no slice.
  7. Properties: apportioned counts and pause sum to the unsplit totals; no slice has negative
     width; the drawn totals equal the summary's `lost` and `pause_total_ns - pause_measured_ns`.

## 6. Out of scope

- **Loss in the summary surfaces.** Already shipped; this spec adds no figure, only geometry
  for figures that exist.
- **Anything that would change a summary number.** If drawing Loss changes what `--summary`
  reports, the geometry is wrong.
- **The `Processes` liveness track**, which is a different track for a different fact.

## 7. Further notes

`Cursor::last_certainty` is dead code until this ships: computed, raised from both sources,
dropped with its interpreter, and covered by five tests, with no caller. It was kept
deliberately. Do not delete it without reading §4 first.
