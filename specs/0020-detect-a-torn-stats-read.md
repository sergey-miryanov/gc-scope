# 0020 — Detect a stats region that moved under the read

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** S
- **Origin:** Open question §7 of spec 0011 (now deleted), which asked whether the accumulator
  should validate that a poll's Records are contiguous or trust it as gcmon does. That spec
  decided to **trust it**, on the grounds that no figure it reports is wrong without the check.
  This spec revisits that with the cost of being wrong stated.
- **Respects:** [ADR 0019](../docs/adr/0019-loss-is-accounted-over-the-observed-span.md) (the
  free-threaded write window it documents is *not* what this detects),
  [ADR 0020](../docs/adr/0020-monitor-reads-every-interpreter.md) (the once-per-process warning
  precedent)

## 1. Problem statement

gcscope reads a target's GC stats region with one `read_memory_h` call and decodes the bytes
pure, while CPython writes into that region without holding anything gcscope can wait on. The
region can move under the read, and nothing checks whether it did.

The failure that costs an operator is a single Entry caught half-written: `ts_start` belonging
to Collection k+11 sitting beside `ts_stop` belonging to k. `GcStat::is_complete` rejects the
case where that reads backwards and accepts the rest, so a pause of nonsense width flows into
`pause_measured_ns` and out through the table, the JSON and the trace, indistinguishable from a
real long Collection. An operator investigating a latency spike is handed one that never
happened.

There is no detector for this, and no reason to expect one to be needed until it is.

## 2. Solution

When a poll's Records show that the region moved during the read, gcscope says so once per
interpreter on stderr, naming what it saw. The operator learns that a figure in the run may be
an artefact of the read rather than of the target, which is the difference between chasing a
GC problem and chasing a gcscope problem.

Nothing else changes. No figure moves, no key appears, and a run over a stable region is
byte-identical to today's.

## 3. User stories

1. As an operator, I want to be told when gcscope's own read caught the target mid-write, so
   that I know whether to trust an outlier pause before I investigate it.
2. As an operator on a stable target, I want to hear nothing, so that the check costs me no
   attention.
3. As an operator, I want to hear it once per interpreter rather than per poll, so that a
   persistently unstable target does not bury its own output.
4. As a gcscope maintainer, I want the detector not to fire on CPython's documented
   free-threaded write ordering, so that a known-benign window does not train everyone to
   ignore the warning.
5. As a gcscope maintainer, I want the check to cost one pass over a poll's Records, so that it
   is payable on every poll of every interpreter.

## 4. Implementation decisions

### What contiguity means here

Within one poll, for one `(interpreter, generation)` ring, the complete Records should carry
`collections` counters forming a consecutive run. A ring holds the last N Collections, so their
counters are `k-N+1 … k`.

Three departures are legitimate and `Cursor::admit` already handles all three: Entries on a
ring not yet full read zero and are skipped; an Entry still running is held for a later poll;
and CPython copies a Record into the next Entry ahead of overwriting it, so the same counter
appears twice with no timestamp telling the two apart. A gap that survives those means the
region moved.

### What it does and does not buy

**It fixes no figure.** A Record lost to a torn read is genuinely lost, and ADR 0019's
arithmetic counts it correctly either way, because the count comes from CPython's cumulative
counter rather than from what arrived. Anyone picking this up expecting the numbers to improve
has misread it.

What it buys is a proxy for "the region held still" — correlated with the half-written Entry in
§1, which contiguity cannot see directly. That is the whole case for the feature, and it should
be weighed as such rather than as a correctness fix.

### Where it lives, and what it does

One pass in `monitor/cursor.rs` over a poll's sorted candidates, which are already sorted by
`(interpreter, generation, collections)` for folding. Report through `MonitorContext`, once per
`(pid, interpreter)`, in the shape `warn_if_interpreters_dropped` already established.

*Rejected for now:* a `torn_reads` key in the JSON summary. `docs/summary-json.md` is pinned
byte-for-byte and its consumers read it; adding a key for a condition nobody has observed in
the field is a schema commitment made on speculation. If the warning fires in practice,
promoting it is a small follow-up with evidence behind it.

### The free-threaded window is not this

ADR 0019 records a benign race: the free-threaded writer stores both timestamps, then
`collections++`, then `duration +=`, so a poll can admit a Record whose counter has advanced
and whose `duration` has not. The counters stay contiguous through it, so this check must not
fire there — worth a test, because a detector that cries wolf on a documented CPython ordering
is worse than no detector.

## 5. Seams and testing decisions

- **Seam:** the poll seam. Scripted batches of `GcStat` reach `Cursor::admit` directly, and a
  torn read is a batch with a hole in it — trivial to stage and impossible to stage live.
- **New seam needed:** none.
- **What makes a good test here:** that each of the three legitimate departures stays silent,
  and that a genuine gap does not.
- **Prior art:** `cursor.rs`'s admit tests; `context.rs`'s
  `a_process_past_the_interpreter_bound_is_reported_once`.
- **Cases:**
  1. A batch with a counter hole warns once.
  2. A second such batch for the same interpreter warns no further.
  3. A ring not yet full, an in-flight Entry, and a duplicated pre-overwrite counter each stay
     silent.
  4. The free-threaded ordering — counter advanced, `duration` not — stays silent.
  5. A dead PID's warning flag is dropped, so a recycled PID is warned about again.

## 6. Out of scope

- **Rejecting or repairing a torn batch.** The Records that did arrive are individually as good
  as any; discarding them would turn a diagnostic into data loss.
- **Detecting the half-written Entry directly.** No field pair proves it. If a way is found,
  that is a better feature than this one and supersedes it.
- **Reporting the count as a figure** in either summary surface, per §4.
