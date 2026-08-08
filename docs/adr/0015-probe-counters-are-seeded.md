# 0015 — Probe counters are seeded from CPython's own

**Status:** Accepted — decided 2026-08-06. (Preserves the meaning of **Lifetime total** and
**Coverage** as `CONTEXT.md` defines them, and keeps
[ADR 0019](0019-loss-is-accounted-over-the-observed-span.md)'s reconstruction valid on
Probe rings. Complements [ADR 0014](0014-probe-regions-discovered-by-module-export.md).)

## Context

On 3.13 and 3.14 there are **two** sources for the same quantities at once:

- CPython's own inline `generation_stats` — `collections`, `collected`, `uncollectable`,
  counted since **interpreter start**, already read by gcscope as `GcStatsKind::InlineArray`.
- A Probe ring — the same three fields plus timing, counted since **Probe install**.

The prototype starts every counter at zero. So its `collections` is not the interpreter's
`collections` — while being byte-identical to a Native ring, where it *is*. The glossary is
explicit about what the reader will assume:

> **Lifetime total**: A figure covering the Observed interpreter's whole history since it
> started, rather than the window the Observer watched.

Feed an unseeded Probe ring to a decoder written for a Native one and every Lifetime total is
wrong by however many Collections ran before install, with nothing looking wrong.

**Coverage** breaks in the same motion and in the flattering direction. It is reconstructed by
differencing what was read against what the cumulative counters say ran. If both sides come
from the Probe, the ratio is computed over the window since install and reported as though it
covered the run — so an operator is told coverage is high precisely when the Probe was
installed late.

Three options:

- **(a) Ship as-is** and let the reader present Probe counters as Lifetime totals. Silently
  wrong, and wrong in the direction nobody checks.
- **(b) Seed at install** from CPython's own counters, so they become Lifetime totals in fact.
- **(c) Keep them install-relative** and introduce a distinct concept for pre-install activity.

(b) is the only one where the reader needs no new vocabulary and
[ADR 0019](0019-loss-is-accounted-over-the-observed-span.md)'s arithmetic keeps
working unchanged. The offset needed to read CPython's inline array is already computed by
`compute_inline_stats_off`, so the ingredient exists.

It is not complete, though, and the incompleteness is the interesting part. `duration` was
never recorded by CPython before 3.15, and `candidates` is unobtainable because
`deduce_unreachable` is `static inline`. Neither can be seeded from anything. They remain
counted from install no matter what is decided — and they sit in the same 64-byte entry as the
seeded counters. A mean pause computed from a Lifetime-total `collections` and an
Install-relative `duration` is wrong, and looks entirely reasonable.

## Decision

1. **Seed `collections`, `collected` and `uncollectable`** from CPython's own inline
   `generation_stats` when the Probe installs. Those counters are then Lifetime totals in the
   glossary's sense, and Coverage, Loss and every figure derived from them keep their existing
   meanings with no new concept in the reader.
2. **`duration` and `candidates` are Install-relative**, and the term enters the glossary
   beside **Lifetime total** precisely so the distinction has a name that can appear in output.
   Not seeded, not synthesised, not presented as though they were Lifetime totals.
3. **The region header declares which is which.** A capability word carries the seeding state
   alongside per-field validity, so a reader labels provenance from published data rather than
   inferring it from the interpreter version.
4. **A field the build cannot supply is reported absent, never as zero.** `heap_size` does not
   exist in 3.13's `_gc_runtime_state`; `candidates` is unobtainable everywhere. Zero already
   means "the self-check failed" and "genuinely empty", and a third meaning makes all three
   unreadable.
5. **Rejected: substituting a proxy.** 3.13's `long_lived_total` is a plausible stand-in for
   `heap_size` and is a different quantity. Publishing it under that name would repeat the
   error the prototype already documents for ring index 1, where increments and generational
   passes share a field across the 3.14 line and averaging them compares unlike things.

## Consequences

- The Probe reads the very statistics it supersedes, once, at install. A reader who finds that
  surprising gets an answer here.
- Seeding is a **point-in-time** operation with a race that must be acknowledged: a Collection
  running between the read and the first callback is counted by CPython and not by the Probe,
  or the reverse. The error is bounded by one Collection per generation and is not worth
  eliminating, but it should not be discovered as a mystery later.
- [ADR 0017](0017-monitoring-tiers-follow-the-entry-layout.md) gives a build with no timing
  fields Coverage `0` and absent pause figures, which below 3.15 is every build. With a Probe
  on 3.13/3.14 that is false, and
  its reconstruction arithmetic works there *because* of this decision. Whichever of the two
  lands second amends the other.
- `Install-relative` becomes a term the output surface has to be able to express, not merely a
  note in a design document. That is a labelling requirement on `GcStat`'s consumers, not on
  its decode.
- If CPython ever backports per-Collection timing below 3.15, `duration` becomes seedable and
  point 2 narrows to `candidates` alone.
