# 0022 — Every timestamp gcscope writes names the clock it is on

**Status:** Accepted — implemented 2026-08-09, with the first timestamp gcscope writes into a
Perfetto trace. Extends [ADR 0017](0017-monitoring-tiers-follow-the-entry-layout.md)'s
"absent rather than zero" from pause figures to timestamps.

**Superseded in part by [ADR 0023](0023-one-clock-read-per-platform.md) (2026-08-12).** Decision 4
below is discharged: the machine-readable declaration exists, so a trace may now carry Collections
and gcscope's own markers together. Decision 1's `clock` debug annotation is **gone**, replaced by
the domain on the packet. **Decision 3 is reversed** — the Observer's clock has no origin of its
own any more, because it turned out not to be a clock of its own; see the note under it. Decision 2
stands unchanged. The last consequence below — which predicted that the target's readings would
have to become offsets into a domain gcscope invents — turned out to be wrong, and ADR 0023 says
why it is left standing here rather than edited away.

## Context

A gcscope trace carries readings from two clocks, and nothing in it says so.

- **The target's clock.** `ts_start`/`ts_stop` on a Record are what CPython published, from its
  own monotonic counter. gcscope copies them through untouched — the raw numbers are what the
  target said.
- **The Observer's clock.** Nanoseconds since the run began, read from `Instant::now()` at each
  poll. A build that publishes no timestamps has nothing else to place a counter sample on
  ([ADR 0017](0017-monitoring-tiers-follow-the-entry-layout.md)), and so does anything gcscope
  says about itself, such as when it attached to a process.

The two share no origin. The Observer's starts near zero; the target's is a counter that has
been running as long as the interpreter has. Put side by side with nothing distinguishing them,
they read as one timeline and are decades apart on it. gcmon writes both into one trace and
says nothing, which is the state this decision is against.

Underneath it sits the older habit of writing `0` for a reading gcscope does not have.
`mark_process_lifecycle` took a `ts_ns` that all three of its call sites passed `0` for. A zero
is not "no value" — it is the origin, a point on the timeline like any other, and the only thing
that distinguishes it from a measurement is knowing it was a placeholder.

Perfetto has a mechanism for exactly this: `TracePacket.timestamp_clock_id`, with a
`ClockSnapshot` correlating a producer's clock to a builtin one. **We tested it.** A custom
clock domain works — trace-processor converts the events into trace time correctly — but only
when the snapshot anchors it to **BOOTTIME**. Anchored to REALTIME or MONOTONIC instead,
trace-processor reports `clock_sync_failure_no_path` and **drops the events entirely**.

That is the mechanism gcscope wants and cannot reach yet, for two separate reasons:

- Anchoring the Observer's clock needs a BOOTTIME reading. Rust's `Instant` deliberately exposes
  no absolute value, so this means per-platform code — `CLOCK_BOOTTIME`, `GetTickCount64`,
  `mach_continuous_time` — and a dependency to call it through.
- Anchoring the **target's** clock is not a matter of effort. A snapshot pairs two clocks read
  at one instant, and gcscope cannot read CPython's counter on demand; it sees only the values
  already written into Records. There is no instant at which to take the pair.

  *(This second point is where the reasoning went wrong, and
  [ADR 0023](0023-one-clock-read-per-platform.md) is the correction: gcscope cannot read CPython's
  counter, but it does not need to — the counter is a system clock, and gcscope can read that.)*

## Decision

1. **Every timestamp gcscope writes says which clock it came from.** Today that statement is a
   `clock` debug annotation on the event carrying the timestamp, so it travels with the number
   and is visible both in the UI and in trace-processor's `args`.
2. **A reading gcscope does not have is absent, never zero.** `mark_process_lifecycle` takes an
   `Option<i64>`; an encoder handed `None` writes no timestamp, and where the format needs one
   to place an event, it writes no event. `ProcessDescriptor.start_timestamp_ns` is left off for
   the same reason: it wants the moment the process began, on the target's clock, and gcscope
   attached partway through a program already running.
3. ~~**The Observer's clock keeps its origin at the attach.** It is what a run's counter samples
   are already on, and a trace starting near zero reads more easily than one starting at
   1.7×10¹⁸. Moving it to wall time to make the correlation easier would change every existing
   counter-only trace for a benefit the correlation does not yet deliver.~~

   **Reversed (2026-08-12), by the step that first drew counter tracks in a Perfetto trace.**
   This reads as a choice about presentation and is not one. A build with no timestamps places its
   samples on this reading and a build with timestamps places everything on the target's, so an
   origin at the attach makes those two origins in one event stream, and nothing downstream can
   tell which of them a `TraceEvent` carries. A tree holding a 3.13 target and a 3.15 one
   described them decades apart in either format. Since
   [ADR 0023](0023-one-clock-read-per-platform.md) the Observer's clock is *the machine's
   monotonic clock*, so there is no origin left to choose: `MonitorContext::observed_at_ns` hands
   over the poll's reading as it is, and every timestamp crossing the exporter boundary is in the
   one domain. Traces do now start at 1.7×10¹⁸, which was the cost this decision declined to pay
   for a benefit it could not yet see.
4. **`timestamp_clock_id` + a BOOTTIME-anchored `ClockSnapshot` is the target state**, and it is
   **required before a trace carries both clocks at once**. Perfetto traces at the time of writing
   carried only Observer-clock events; the step that adds Collections adds target-clock ones, and
   it inherits this. *(Discharged by ADR 0023, which also found that the two are one clock — so
   what the declaration ended up carrying is the one domain both are in.)*

## Consequences

- A process gcscope watched appears in the trace with a marker saying when gcscope started
  watching, and a reader who wonders what that number is relative to can find out from the
  event itself.
- The annotation is prose, not a clock domain. Trace-processor cannot use it to convert
  anything, so a trace mixing both clocks would still render wrong — it would merely say so. The
  rule in decision 4 exists because that is not good enough.
- Whoever implements the machine-readable form pays for a portable boot-clock reading, and finds
  that the target's clock still cannot be snapshotted. The likely shape is to declare the
  Observer's clock properly and treat the target's readings as offsets into a domain gcscope
  defines from the first Record it reads — which changes the numbers in the trace, so it is a
  decision of its own rather than an implementation detail of this one.
- gcscope now announces a process the moment it attaches rather than when the process first
  collects. A worker that collected nothing reaches both exporters, so the Chrome trace gains a
  `process_name` line it did not have.
- **That does not finish the job on the Chrome side.** A Chrome trace holding only that
  metadata line yields a `process` row and no track and no slice — checked, not assumed — for
  the same reason the Perfetto UI hides an empty track. Perfetto's answer is the marker this
  ADR times; Chrome's would be a `TraceEvent::Instant`, and the encoder can already write one
  (`"ph":"I"`, process-scoped). It is not written, because it would put an event into every
  Chrome trace an operator has ever captured and this decision is about clocks. So the silent
  worker is visible in a Perfetto trace and still only in the process list of a Chrome one.
