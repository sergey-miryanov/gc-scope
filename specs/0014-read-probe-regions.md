# 0014 — Read Probe regions, and tell the operator when one is there

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** L
- **Origin:** Grilling session 2026-08-06 on productizing the Probe. The prototype reader is
  `X:/Work/gc-monitor/gcprobe/verify/src/main.rs`, which already does the whole read end to end
  against a PE target; this spec generalises it and moves it inside gcscope. Research note
  §12.4 identified the missing piece as "a third `GcStatsRegion` variant".
- **Respects:** [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md) (resolve once),
  [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md) (decode keyed by layout),
  [ADR 0004](../docs/adr/0004-per-platform-image-layout.md) (image facts are discovered and
  CI-verified, never assumed),
  [ADR 0006](../docs/adr/0006-layout-registration-integrity.md) (fails closed),
  [ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md) (`GcStat` is a layout-driven view),
  [ADR 0012](../docs/adr/0012-version-detection-fails-closed.md)
- **Blocked by:** [0013](0013-probe-portable-core.md) — there is nothing to read until a Probe
  publishes a version-4 header. [0012](0012-gen-offsets-serves-the-probe.md) supplies the digest
  this matches against.

## 1. Problem statement

An operator can install a Probe into their 3.13 or 3.14 application and gcscope will ignore it
completely. The region is there, correctly written, and nothing reads it.

Worse, the operator has no way to discover that the option exists. gcscope attaches to a 3.14
process, reports Collection counts, reports no pause times, and says nothing about why or about
what would change that. The capability gap is invisible from the outside, so the people who
would most benefit are the least likely to find out.

The reason nothing reads it is structural. `OffsetTable::gc_stats_region` produces
`GcStatsRegion::Direct` or `Deref`, and both start from `gc_addr` — an address gcscope derived
itself from offsets it owns. **Nothing in CPython points at a Probe region.** It is not
reachable from interpreter state at any offset, because CPython does not know it exists. The
chain has to start somewhere else entirely: at a symbol exported by a module that was not
written by CPython and is not trusted the way CPython's own memory is.

That last clause is the part that makes this more than plumbing. Every byte gcscope reads today
comes from a structure whose location it computed itself. A Probe region's address is *published
by the target*, and any process can export eight bytes reading `GCPRB015`.

## 2. Solution

When gcscope attaches to a process carrying a Probe, per-Collection timing appears — pause
durations, heap size, the whole Record — for an interpreter that previously offered only counts.
When it attaches to one without, it says so once, and says what would fix it.

Discovery needs nothing out of band. gcscope enumerates the target's mapped images as it already
does for version detection, finds the Probe module by name, reads its export table, and takes
the region address and full geometry from the published header. A PID is sufficient; nothing is
passed in.

A Probe that does not check out is **refused and reported**, never half-used. The counts from
CPython's own inline array remain, so the operator loses nothing they had — but they are told
plainly that a Probe was present and rejected, and why. A silent fallback would leave someone
who installed a wheel staring at unchanged output with nothing to debug.

Figures carry their provenance. Counters that a Probe seeded are **Lifetime totals** and are
reported as such; `duration` and `candidates` are **Install-relative** and are labelled
differently; fields the build cannot supply — `heap_size` on 3.13 — are reported **absent**
rather than as zero.

## 3. User stories

1. As an **operator attaching to a production 3.14 interpreter with a Probe installed**, I want
   pause durations in my trace, so that I can see what my collector costs rather than only how
   often it ran.
2. As an **operator attaching to a 3.13/3.14 interpreter without a Probe**, I want to be told
   once that per-Collection timing is available and how to get it, so that I can find a
   capability I had no way to know existed.
3. As an **operator who installed a Probe and sees no change**, I want gcscope to tell me it
   found one and rejected it, and which check failed, so that I have something to act on.
4. As an **operator piping gcscope's output into a script**, I want the hint to stay out of my
   data, so that adding it breaks nothing.
5. As an **operator on 3.15+**, I want never to see the hint, because my interpreter already
   publishes a Native ring.
6. As an **operator on 3.11 or 3.12**, I want never to see the hint, because no wheel exists for
   me and advertising one would be a false lead.
7. As an **operator reading a Probe's figures**, I want `duration` distinguished from
   `collections` in provenance, so that I do not average an Install-relative figure against a
   Lifetime total.
8. As a **security-conscious operator**, I want gcscope to refuse a malformed or hostile header
   rather than following it, so that attaching to an untrusted process cannot make gcscope read
   arbitrary addresses or allocate unbounded buffers.
9. As a **gcscope maintainer**, I want a change to the decoder that breaks Probe reading to go
   red on my PR, not on a workflow my PR did not run.

## 4. Implementation decisions

### The third variant

`GcStatsRegion` gains a variant for a region reached through a module export rather than through
interpreter state. It is not a third flavour of "an address plus an offset" — resolving it
requires finding an image, parsing its export table, reading and validating a header, then
walking a slot table to match the interpreter id. That work does not belong in
`OffsetTable::gc_stats_region`, whose inputs are `gc_addr` and an offset; it belongs beside the
image handling that already exists for version detection, with the variant naming its result.

Precedence: a **validated** Probe ring supersedes the inline array on 3.13/3.14 — it is a strict
superset, and after [0013](0013-probe-portable-core.md)'s seeding the counts agree. Any
validation failure falls back to the inline array. On 3.15+ the Native ring always wins and no
Probe lookup is attempted.

### Discovery

Name-filtered symbol lookup. Mapped images whose basename starts with the Probe module name are
parsed with **goblin** — already a dependency, and already handling PE, ELF and Mach-O uniformly
— and the header symbol is looked up in the export table.

The prototype matches `gcprobe.pyd` exactly. That does not survive the port: the file is
`gcscope_probe.cpython-314-x86_64-linux-gnu.so` on Linux and similarly tagged on macOS, so the
match becomes a prefix. Per [ADR 0004](../docs/adr/0004-per-platform-image-layout.md) the
per-platform export-table facts are discovered and CI-verified rather than assumed — including
the one that bites silently: on ELF and Mach-O the symbol must actually reach the dynamic symbol
table, and a build that picks up `-fvisibility=hidden` produces a module where discovery finds
nothing and there is no error to report.

Rejected: **parsing every mapped image** (pays export-table parsing on libc and friends at every
attach, for a name-independence nobody asked for), and **routing discovery through interpreter
state** (removes the module-name coupling but forces gcscope to read Python objects, which its
design deliberately avoids).

### Validation

The chain starts at data the target publishes, so it fails closed in the shape of
[ADR 0012](../docs/adr/0012-version-detection-fails-closed.md) — refuse, never clamp, never
"read what we can":

| Check | Why it is not optional |
|---|---|
| `magic` exact, `version` known, `header_size` in range | The cheapest possible rejection of anything that is not a Probe |
| `region_size` consistent with `young_entries`/`old_entries`/`item_size` | The geometry must prove itself; an inconsistent one is either corrupt or hostile |
| `item_size` equals the generated layout's item size | The compile-time assertion from [0012](0012-gen-offsets-serves-the-probe.md), enforced again at read time |
| `layout_digest` resolves to a registered layout | The **only** check that catches a field moving *within* the entry — every size stays identical |
| `max_interp` bounded; `slots_addr` inside a mapped region | An unbounded `region_size` is a memory-exhaustion vector in gcscope, not merely bad data |
| `py_version` equals the version gcscope detected independently | Free, and strong: two independent derivations of the same fact |

The digest check is the one that earns its place least obviously and matters most. Sizes cannot
distinguish a reordered 64-byte entry from a correct one, so without it a Probe built against a
superseded 3.15 pre-release decodes cleanly into the wrong fields.

### Provenance in the output

The Probe's `capabilities` word is not an implementation detail to be consumed and discarded — it
determines how every figure derived from the region may be described. Seeded counters are
Lifetime totals; `duration` and `candidates` are Install-relative; a field the build cannot supply
is absent. `GcStat` is already a layout-driven view over raw bytes
([ADR 0007](../docs/adr/0007-gcstat-layout-driven-view.md)), so the natural home for provenance
is beside the layout that view is keyed on, not threaded through every call site.

The specific error to design against: a Lifetime-total `collections` and an Install-relative
`duration` occupy the same 64-byte entry, and a mean pause computed from the pair is wrong
without anything looking wrong.

### Operator messaging

Two messages, deliberately different in kind:

- **Capability hint** — printed at most once per invocation, to stderr, suppressed when stderr
  is not a TTY or when quiet is requested. The TUI carries it as a status field rather than a
  nag. Gated to exactly the interpreter versions a wheel exists for: not 3.11/3.12 (no wheel),
  not 3.15+ (Native ring). That gate tracks the published wheel matrix, so it lives next to the
  version registry rather than at the print site.
- **Rejection diagnostic** — always shown, naming the check that failed. Not suppressible. The
  silent-fallback decision above is only defensible because this exists.

Rejected: printing the hint unconditionally (pollutes piped output), and hiding it behind a
`doctor` subcommand (the operators who need it are precisely those who do not know to look).

### The verifier

`gcprobe/verify/` is promoted to a gcscope integration test rather than rewritten. Its invariants
are already the right ones and were exercised against a real target: every written entry decodes,
`is_complete()` holds, durations are positive, cumulative counters never regress between samples.
What changes is that it reads through gcscope's own path instead of alongside it, so it tests the
shipped decoder rather than a parallel one.

## 5. Seams and testing decisions

- **Seam:** `PySession` — the existing resolve-once facade
  ([ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md)). A Probe region is another way
  a session reaches GC statistics, so the highest seam that observes it is the one already
  exposing `gc_stats` and `gc_stats_region_addr`. No new seam.
- **New seam needed:** none. Notably **not** a `test-hooks` gate: whether a Probe was accepted or
  rejected is something an operator is told anyway (§4), so it is an honest public signal, which
  [ADR 0005](../docs/adr/0005-testing-strategy.md) prefers.
- **What makes a good test here:** assert the decoded **shape** and the provenance labelling. A
  Probe region read with a wrong layout produces a complete, plausible table — the characteristic
  failure this repo is built to catch ([README §6](README.md#conventions)).
- **Prior art:** `tests/live_smoke.rs` for the attach-and-assert-shape pattern, and the prototype
  verifier for the invariants.
- **Cases:**
  1. Probe present and valid on 3.14 → timing appears; counters equal `gc.get_stats()` at
     install; `heap_size` populated.
  2. Probe present and valid on 3.13 → timing appears; `heap_size` reported **absent**, not `0`.
  3. Probe absent on 3.14 → inline counts unchanged, hint emitted once to stderr, nothing on
     stdout.
  4. Probe present with a **digest the registry does not know** → refused, diagnostic names the
     digest, inline counts still reported. This is the case sizes cannot catch.
  5. Probe present with inconsistent geometry, an out-of-range `slots_addr`, or a `py_version`
     disagreeing with independent detection → refused, each naming its check.
  6. Non-default Ring depth (512/128) decodes correctly — the header-driven geometry path is now
     the *only* path a Probe region takes, so it is tested directly rather than incidentally.
  7. Hint suppression: not a TTY, quiet requested, 3.12 target, 3.15 target — silent in all four.
  8. Regression guard: on every interpreter without a Probe, output is byte-identical to before
     this change.

## 6. Out of scope

- **The Probe itself** — [0013](0013-probe-portable-core.md).
- **Wheel building, publishing, CI matrix** — [0015](0015-publish-probe-wheels.md), except the
  single always-on contract leg, which this spec's §5 cases define and that spec wires up.
- **Loss and Coverage reporting.** [0011](0011-loss-reconstruction-and-gc-statistics.md) owns
  those surfaces. This spec makes the inputs correct and labelled on 3.13/3.14; it does not build
  the reporting on top. See §7.
- **gcscope installing, injecting or launching a Probe.** Explicitly rejected during the design
  session: it would turn an opt-in import into remote code injection and change the product.
- **Reading a Probe on 3.15+.** The Native ring wins; no lookup is attempted.
- **Multiple Probes, or a Probe in a process gcscope did not verify the version of.** Refused by
  the `py_version` cross-check rather than reconciled.

## 7. Further notes

**Interaction with [0011](0011-loss-reconstruction-and-gc-statistics.md), which is the important
one.** 0011 states that below 3.15 "there are no spans", Coverage is `0`, and pause figures are
reported as **absent**. A Probe falsifies all three on 3.13 and 3.14. The specs are compatible in
substance — 0011's reconstruction math works on a Probe ring precisely because
[0013](0013-probe-portable-core.md) seeds the counters, so the difference between what the
counters say ran and what was read is still exact — but whichever lands second must amend the
other's sub-3.15 branch rather than leaving two accounts of the same behaviour. If 0011 lands
first, its "Coverage is 0 below 3.15" becomes "Coverage is 0 below 3.15 **without a Probe**".

**Open question for when this is picked up.** Whether a rejected Probe should suppress the
capability hint. Both firing at once ("a Probe was rejected" + "install a Probe") is clearly
wrong; suppressing the hint whenever any Probe-shaped image is mapped is the obvious rule but
makes the hint depend on a failed parse. Not settled in the design session.

**Threat-model note.** The validation table above is written against a *malformed* header, which
is the realistic case. It is not a security boundary: gcscope already holds read access to the
target's memory before any of this runs, so a hostile header buys an attacker misdirection of
gcscope's own reads, not new access. Worth stating explicitly so a later reader does not mistake
the table for a sandbox.
