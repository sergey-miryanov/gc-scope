# 0023 — The Observer's clock is the target's clock, read per platform

**Status:** Accepted — implemented 2026-08-12, with the step that puts Collections in a Perfetto
trace. Discharges decision 4 of [ADR 0022](0022-every-timestamp-names-its-clock.md), and replaces
the shape that ADR predicted.

## Context

[ADR 0022](0022-every-timestamp-names-its-clock.md) left one thing undone and one thing
forbidden. Undone: the machine-readable clock declaration, `timestamp_clock_id` with a
`ClockSnapshot`. Forbidden: a trace holding both of gcscope's clocks until that existed. Adding
Collections to a Perfetto trace runs straight into the second, because a Collection's timestamps
are the target's and the marker beside it is gcscope's own.

ADR 0022 also predicted what the fix would cost:

> Whoever implements the machine-readable form pays for a portable boot-clock reading, and finds
> that the target's clock still cannot be snapshotted. The likely shape is to declare the
> Observer's clock properly and treat the target's readings as offsets into a domain gcscope
> defines from the first Record it reads — which changes the numbers in the trace.

The first half held. The second was wrong, and it was wrong in a way worth writing down, because
the predicted shape would have been a **fabrication**: anchoring the target's clock to "the poll
that first read a Record" claims a Collection happened when gcscope noticed it, and a ring at
attach can hold Collections from an hour before. Every number in the trace would have moved by an
unknown amount.

**The two clocks were never two.** The Probe times a Collection with `PyTime_PerfCounterRaw`, and
CPython's perf counter is the machine's monotonic counter — `CLOCK_MONOTONIC` on Linux,
`mach_absolute_time` on macOS, `QueryPerformanceCounter` on Windows. Every one of those is
system-wide. gcscope runs on the same machine, so it can read the very same counter in its own
process and get a value in the same domain, exactly, with nothing to estimate. Rust's `Instant`
hides its absolute value, which is what made this look impossible; a `clock_gettime` call does
not.

Measured on Windows before any of this was built: CPython reported `perf_counter_ns` =
721728960985600 and gcscope read 721728960939900 a moment earlier — 46 ms apart, which is what it
costs to spawn an interpreter.

## Decision

1. **gcscope reads the machine's monotonic clock itself** (`src/monitor/clock.rs`), per platform,
   and that reading is the origin the Observer's clock counts from. There is one clock in a
   gcscope trace, not two that have to be reconciled.
2. **The numbers CPython published reach the trace unchanged.** No offsetting, no rebasing. A
   Collection's `ts_start` in a Perfetto trace is the integer the target wrote, which is also what
   the Chrome trace of the same run carries — so the two formats cannot come to disagree about
   when something happened.
3. **The trace declares that domain** rather than converting into one Perfetto already knows:
   `timestamp_clock_id` on every packet carrying a timestamp, and one `ClockSnapshot` pairing the
   domain with BOOTTIME. The conversion is the reader's, and the claim gcscope is making is
   visible in the file.
4. **Which system clock a platform's CPython uses is verified, not inferred.** It fails open in
   exactly the way [ADR 0004](0004-per-platform-image-layout.md)'s image facts do — a wrong clock
   returns a plausible number and the trace is merely wrong — so `live_smoke.rs` asks a real
   interpreter for `time.perf_counter_ns()` and compares it against gcscope's own reading, on
   every leg of the matrix.
5. **The BOOTTIME anchor is read once, at the start of the run.** Re-anchoring mid-run would put
   two disagreeing claims in one file.

## Consequences

- A Perfetto trace holds Collections and the markers gcscope puts beside them on one timeline,
  and the operator who zooms into a pause sees the attach where it belongs rather than days away.
- The `clock` debug annotation ADR 0022 introduced is **gone**. It said in prose what the domain
  now says in a form trace-processor can act on, and a second statement that goes stale
  independently of the first is worse than none.
- **BOOTTIME is load-bearing and was tested twice.** ADR 0022 recorded that anchoring to REALTIME
  or MONOTONIC makes trace-processor report `clock_sync_failure_no_path` and drop the events;
  `tests/perfetto_encoder.rs` now fails that way on purpose if the constant is changed, so the
  finding is a test rather than a paragraph.
- **A machine that suspends mid-capture shifts the trace by the length of its nap.** BOOTTIME
  counts through suspend and the monotonic clock does not, and the anchor between them is read
  once. Nothing inside the trace moves relative to anything else, which is the property that
  matters; what moves is where the whole trace sits against another producer's.
- gcscope takes `libc` and `windows-sys` as direct dependencies. Both were already in the tree
  through `remoteprocess` and friends, so this costs a manifest line rather than a build.
- The Observer's clock keeps its origin at the attach, so a counter-only trace on a build with no
  timestamps is unchanged, and the `--summary` figures are untouched. What changed is that the
  origin is now a stated point on a clock rather than an opaque `Instant`.
- A fourth Unix would compile with the anchor degraded to a single clock — internally exact,
  absolutely unplaced. gcscope is built and tested on three.
