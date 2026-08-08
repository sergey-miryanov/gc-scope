# 0020 — The monitor reads every interpreter of an unlocked chain

**Status:** Accepted — implemented 2026-08-08. Completes the multi-interpreter half of
spec 0011 (now deleted), whose accumulator
[ADR 0019](0019-loss-is-accounted-over-the-observed-span.md) keys on
`(pid, interpreter, generation)`.

## Context

The monitor polled `gc_stats(false)`, which stops at the head of `_PyRuntime.interpreters`.
A process running sub-interpreters had everything outside one of them silently absent: no
spans, no counters, and no line in the summary saying an interpreter had been skipped. The
key the accumulator already carried named an interpreter the read never produced.

Two facts about CPython's chain shape the fix, and both cut against the obvious reading.

**A new interpreter is linked at the head.** So the head *changes*, and a head-only capture
still yields several interpreters over a long run, each in a window of its own. The first
live check asserted that two interpreters appear in the summary and passed against the
defect it was written for: 86 gen-0 Collections against 255, where reading both gives 280
against 280. Presence is not the property; being read *throughout* the run is.

**An interpreter id is never reused.** The accumulator is the dedup cursor while an
interpreter is in the chain and the figures the summary reports once it is gone, so a
workload creating and destroying sub-interpreters holds one per `(interpreter, generation)`
until the process exits. The map this replaced was keyed on `(generation, Entry)` and
bounded by the ring geometry; nothing bounds this one.

**The chain is walked without CPython's lock,** every tick, against the churn the same
workload produces. Reading N interpreters makes a torn read reachable where reading only the
head made it nearly impossible.

## Decision

1. **Every poll reads every interpreter.** Cost is linear in their number and small: the
   layout, the version and the runtime address resolve once at attach, so an interpreter
   costs its id, its `next` link and one read of its stats region.
2. **The live gate asserts every interpreter is read throughout the run.**
   `live_monitor_records_every_interpreter_not_only_the_first` requires each interpreter's
   Record count to be within a quarter of the busiest, against a fixture holding a
   sub-interpreter alive for the whole capture. Restoring the head-only read must fail it.
   No unit test replaces this one, because the property is about which interpreters CPython
   staged, not about what the code did with them.
3. **The bound is on retained history, not on existence.** Each poll names the interpreters
   that exist and nothing caches that. `MAX_RETAINED_INTERPRETERS` caps how many
   accumulators one process keeps, through the single gate every per-interpreter map passes,
   so the accumulators and the certainty bounds are bounded together.
4. **Room is made by dropping the interpreter seen least recently,** a lazy proxy for
   "finished". *Rejected:* refusing the newcomer, which shipped first and inverted the
   bound's purpose — ids are monotonic, so the set filled with the oldest and went silent on
   everything running while retaining a full set of dead accumulators, for exactly the
   churning workload the bound exists for.
5. **Absence from one poll is not death.** Evicting on first absence would be exact about
   existence and wrong in practice: the walk skips an interpreter whose read failed, and
   dropping a live cursor re-admits its whole ring, emitting every Record in it twice.
   Eviction under pressure is that hysteresis, and it cannot reach an interpreter still
   collecting, which appears in every poll.
6. **A torn chain read costs one interpreter past the head and the whole poll at it.** A
   revisited address ends the walk, since a `next` into freed-and-reused memory can point
   back into the chain. A negative id skips its link. A read failing past the head skips that
   interpreter: propagating it discarded every interpreter already read that tick and routed
   a live process through `revalidate` → `Died` → the give-up ladder, ending a run over one
   sub-interpreter's teardown. The head still propagates, which is the signal from when this
   read one interpreter.
7. **That control flow is a free function over its two reads.** `walk_interpreter_chain`
   takes them as closures. The reads need a target; the answers to a bad read do not, and a
   chain that loops back on itself is not something a live target stages on demand.

## Consequences

- A capture of a process running sub-interpreters reports each one in its own summary block
  and on its own trace track, and no interpreter's Loss is charged to another.
- A process past the bound is announced once on stderr, naming the count and saying that
  every interpreter still running is kept. `mark_died` drops the flag with the rest of the
  PID's state, so a recycled PID is announced again.
- A run that outlives 1024 interpreters loses the *figures* for the oldest of them, silently
  in the summary and audibly on stderr. Reporting them all would need the summary written
  incrementally rather than at end of run, which is a different feature.
- The Chrome encoder had to key `thread_name` dedup on `(pid, tid)` first. Every process's
  main interpreter is id 0, so the collision already cost multiprocessing workers their track
  names; reading a second interpreter per process would have made it bite within one process
  too.
- `walk_interpreter_chain`'s seven tests stage what no live target stages on demand: a chain
  looping back on itself, on the head, and on one link; a read failing at the head against
  one failing past it; an unreadable link; a NULL region; a NULL head. Written after codecov
  found the gap, against a claim in the ticket that none of it was unit-testable.
