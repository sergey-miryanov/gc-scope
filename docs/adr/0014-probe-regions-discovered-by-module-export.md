# 0014 — Probe regions are discovered through the module export table

**Status:** Accepted — decided 2026-08-06. (Extends
[ADR 0003](0003-layout-driven-gc-stats-decode.md) with a third region kind, and extends
[ADR 0012](0012-version-detection-fails-closed.md)'s fail-closed posture to the first input
class that is not CPython's own memory. Depends on
[ADR 0004](0004-per-platform-image-layout.md).)

## Context

`GcStatsRegion` has two variants, and both start from `gc_addr` — an address gcscope derived
itself from offsets it owns. `Direct` adds an inline offset; `Deref` reads a pointer. The
difference between them is one indirection.

A Probe region is not reachable that way at all. **Nothing in CPython points at it**, at any
offset, because CPython does not know it exists. The chain has to begin somewhere outside
interpreter state entirely.

The prototype established that it can begin at the Probe module's own export table: find the
module among the target's mapped images, look up the header symbol, validate a magic, and read
the region address and full geometry from the published header. A PID is sufficient — nothing
is passed out of band, and the prototype's `verify-gcprobe <pid>` demonstrated it end to end.

Three discovery mechanisms were weighed:

- **Name-filtered symbol lookup** — enumerate mapped images (already done for version
  detection), keep those whose basename matches the Probe module, parse the export table with
  goblin (already a dependency, already covering PE, ELF and Mach-O), look up the symbol.
- **Unfiltered symbol scan** — the same without the name filter. Name-independent, at the cost
  of parsing export tables on libc and every other mapped image at every attach.
- **Interpreter-mediated** — the Probe stashes its header address somewhere gcscope already
  reads. Removes the module-name coupling, but forces gcscope to read Python objects, which
  its design avoids.

The name filter was chosen. That decision has a consequence worth stating plainly rather than
discovering later: **the module's filename becomes part of the wire contract.** A rename after
wheels ship silently breaks discovery in every released gcscope.

The trust question is what makes this more than plumbing. Every byte gcscope reads today comes
from a structure whose location it computed itself. Here the address is *published by the
target*, and any process can export eight bytes reading the magic. The realistic case is a
malformed or stale header rather than a hostile one, but the handling is the same, and it is
the handling [ADR 0012](0012-version-detection-fails-closed.md) already prescribes.

One check in particular cannot be dropped. If a field moves *within* the 64-byte entry between
two registered 3.15 layouts, `item_size`, `region_size` and both entry counts are unchanged —
every geometry check passes and every number is attributed to the wrong field. Sizes cannot
detect it. The registry already computes a `stats` digest per layout for exactly this class of
question, and the Probe can declare which one it implements.

## Decision

1. **A third `GcStatsRegion` variant**, reached through a module export rather than through
   interpreter state. Resolving it does not belong in `OffsetTable::gc_stats_region`, whose
   inputs are an address and an offset; it belongs beside the image handling that already
   exists, with the variant naming the result.
2. **Discovery is name-filtered symbol lookup via goblin.** Per
   [ADR 0004](0004-per-platform-image-layout.md), the per-platform export-table facts are
   discovered and CI-verified rather than assumed — including that on ELF and Mach-O the
   symbol must actually reach the dynamic symbol table. A build picking up
   `-fvisibility=hidden` yields a module that works perfectly and is invisible to discovery,
   with no error anywhere, so this is asserted against the built artifact.
3. **Validation fails closed, and refuses rather than clamps:** exact magic, known version,
   `header_size` in range; `region_size` self-consistent with the entry counts and item size;
   `item_size` equal to the generated layout's; `layout_digest` resolving to a registered
   layout; `max_interp` bounded and `slots_addr` inside a mapped region; and `py_version`
   equal to the version gcscope detected independently. The digest check is the only one that
   catches a within-entry field move.
4. **Precedence: a validated Probe ring supersedes the inline array on 3.13/3.14**, being a
   strict superset whose counts agree once seeded
   ([ADR 0015](0015-probe-counters-are-seeded.md)). On 3.15+ the Native ring always wins and
   no lookup is attempted.
5. **Fallback is announced, never silent.** A validation failure falls back to the inline
   array — the operator keeps the counts they had — but the rejection and the failing check
   are always reported. Silent fallback plus silence would leave someone who installed a wheel
   with unchanged output and nothing to debug, which is a worse outcome than refusing outright.
6. **gcscope does not install, inject or launch a Probe.** It reads one if present and says so
   if absent. Injection would convert an opt-in import into remote code execution in the
   target and is a different product.

## Consequences

- The Probe module's filename joins the set of things that cannot change silently, alongside
  the magic and the header layout. It warrants a comment at the declaration.
- The header-driven geometry path stops being an alternative and becomes the **only** way a
  Probe region decodes — the native path hardcodes 11/3 and Probe rings do not use those
  depths. It must be tested directly rather than incidentally.
- gcscope acquires an input class it did not have: data published by the observed process for
  gcscope's benefit. The validation table is the boundary, and it is *misdirection* protection,
  not a sandbox — gcscope already holds read access to the target before any of it runs. Worth
  stating so a later reader does not mistake it for a security boundary.
- The `stats` digest gains a second consumer and therefore a second reason not to change its
  definition casually.
- An operator on 3.13/3.14 without a Probe can now be told that per-Collection timing exists.
  That message is version-gated to the published wheel matrix, which couples an operator-facing
  string to a distribution fact — the gate belongs next to the version registry, not at the
  print site.
