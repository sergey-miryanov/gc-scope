# gcscope

gcscope observes CPython's garbage collector from outside the process — attaching to a
running interpreter, reading its GC statistics from memory, and turning them into traces
and summaries. This file is the glossary for that domain. It defines terms, not designs.

## Processes

**Observer**:
The gcscope process doing the reading. It runs the control plane's listening side.
_Avoid_: parent, monitor process, host

**Observed**:
The CPython process whose GC is being read. It may be attached to, spawned by, or the
parent of the Observer — the relationship is not fixed, so never name it by kinship.
_Avoid_: child, target, monitored process

## GC activity

**Collection**:
One run of the garbage collector over one generation of one interpreter.

**Record**:
The set of numbers CPython publishes about a single Collection — its generation,
interpreter, start and stop timestamps, heap size, and cumulative counters.
_Avoid_: event, sample, stat

**Ring**:
The fixed-size sequence of Entries through which Records are published. Once full it
overwrites its oldest Entry, so it holds a window rather than a history.
_Avoid_: buffer, log, queue

**Entry**:
One position in the fixed-size sequence through which the Observed publishes its
Records. Reserved for this meaning; CPython's own `__slots__` and type slots are the
only "slots".
_Avoid_: slot

**Generation**:
One of CPython's GC age cohorts (0, 1, 2). Not the same across builds — free-threaded
and incremental collectors differ in how many exist and what they mean.

## Probes

**Probe**:
A component loaded inside the Observed that publishes Records CPython itself does not.
Part of the Observed, never a third process and never something the Observer places there.
_Avoid_: agent, shim, injector

**Native ring**:
A Ring the Observed's own CPython publishes.

**Probe ring**:
A Ring a Probe publishes. Identical in shape to a Native ring and different in
provenance, so the two are told apart by what a Probe declares, never by their bytes.

**Seeding**:
Initialising a Probe ring's cumulative counters from the Observed's own, so that they
remain Lifetime totals rather than counting from the Probe's arrival.

## Fidelity

**Loss**:
Collections that ran in the Observed but that the Observer never read as Records —
whether overwritten before it polled, or never published as Records at all. Loss is
*reconstructed* by subtracting what was read from what the cumulative counters say ran,
so its totals are exact while the individual Collections are gone forever.
_Avoid_: drop, miss, gap

**Coverage**:
The share of a generation's Collections the Observer read as Records, in `[0, 1]`. It
qualifies every sampled figure beside it: at `1.0` a distribution is real, at `0.2` it is
the surviving tail of a biased sample, and at `0` the counts stand alone with no
distribution behind them.

**Lifetime total**:
A figure covering the Observed interpreter's whole history since it started, rather than
the window the Observer watched. Never comparable across runs of differing length.
_Avoid_: total, cumulative

**Install-relative**:
A figure counted from a Probe's arrival rather than from the Observed interpreter's start.
Never comparable with a Lifetime total, including one sitting beside it in the same Record.
_Avoid_: relative, since-install

## Control

**Control plane**:
The channel by which the Observed tells the Observer to start or stop recording it, or
marks a moment in the trace. The Observed holds the client; the Observer listens.

**Instant message**:
A named moment the Observed injects into the trace to correlate GC activity with its own
behaviour.
_Avoid_: marker, annotation, tag
