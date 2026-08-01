# CPython version support — what varies, and why it's hard

gcscope reads a running CPython process's garbage-collector state from outside it:
no debugger, no code injected into the target, nothing the interpreter cooperates
with beyond what it already exposes. Everything it reads is at a byte offset inside
a C struct, and those structs are internal — CPython guarantees nothing about them
across versions, builds, or platforms.

This document is about the variability itself: what changes across CPython versions,
why each change makes attaching harder, and what it costs. It decides nothing.

- **What gcscope decided in response** — the [ADRs](adr/). Every section below ends
  by naming the one that owns its subject.
- **What is supported today, and how to use or extend it** — [`README.md`](../README.md):
  the version list, the attach recipes, and the procedure for adding a version.

## Contents

1. [Why offsets vary, and why a wrong one fails open](#1-why-offsets-vary-and-why-a-wrong-one-fails-open)
2. [Knowing which version you are looking at](#2-knowing-which-version-you-are-looking-at)
3. [PEP 768: the interpreter starts describing itself](#3-pep-768-the-interpreter-starts-describing-itself)
4. [Before PEP 768: nothing published, and nothing to anchor on](#4-before-pep-768-nothing-published-and-nothing-to-anchor-on)
5. [What the ABI freeze does and does not promise](#5-what-the-abi-freeze-does-and-does-not-promise)
6. [Where GC state lives, and how it reshaped](#6-where-gc-state-lives-and-how-it-reshaped)
7. [Build configuration varies independently of version](#7-build-configuration-varies-independently-of-version)
8. [Platform image facts cannot be inferred](#8-platform-image-facts-cannot-be-inferred)
9. [What the forces cost together](#9-what-the-forces-cost-together)

---

## 1. Why offsets vary, and why a wrong one fails open

CPython's public C API is stable. Its *internal* structures — `_PyRuntime`,
`PyInterpreterState`, `_gc_runtime_state` — are not, and are free to change in any
release, because nothing outside the interpreter is supposed to read them. gcscope
reads them anyway, which means every field position it uses is a fact about one
specific build.

Getting such a fact wrong does not crash. The address is still inside a mapped page,
the read succeeds, and a plausible integer comes back — a collection count that is
really a timestamp, a pointer that is really a size. The same code runs, the same
branches are taken, and the output looks ordinary. This is the shape of nearly every
bug in this area, and it is why the rest of this document keeps returning to the same
question: what, concretely, would tell us the number is wrong?

Resolution proceeds in five steps, and each force below bends one of them:

```
find the interpreter → learn its version → decide which layout describes it
                    → walk to the GC state → decode the entries
```

The forces are not independent refinements of one problem. They are separate axes —
version, build configuration, platform — that intersect, and their boundaries do not
line up with each other.

---

## 2. Knowing which version you are looking at

Every later step keys on the version, so it has to come first. The interpreter cannot
always be asked for it.

`Py_Version` — a symbol holding `PY_VERSION_HEX`, which can be resolved in the image
and read from live memory — only exists from **3.11**. Before that there is nothing
to read, and the version has to be recovered from a version literal embedded in the
image's read-only data. That is a parse, not a fact: the same section holds strings
like `3.1` and `lib13.12.0`, so the match has to demand a micro component, reject a
digit before the `3`, and require a terminator after the version. A version arrived
at this way is a well-constrained guess.

From **3.13** the process publishes its own version word, and that one is
authoritative — it comes from the build itself rather than from a symbol or a string.
The two sources can disagree: an interpreter upgraded in place still runs the old
image, and a string scan can land on the wrong literal. The published word wins.

So there are three detection regimes with boundaries at 3.11 and 3.13 — and neither
boundary aligns with the others in this document. The cost is that the detected
version is a hypothesis rather than a result: on 3.13+ it is only good enough to
choose *how to look*, and is replaced by what the process says about itself.

→ gcscope's response: [ADR 0002](adr/0002-version-split-runtime-finding.md).

---

## 3. PEP 768: the interpreter starts describing itself

CPython 3.13 implemented [PEP 768](https://peps.python.org/pep-0768/), which made
external debugger attachment a supported thing rather than a trick. Three parts of
it matter here, and all three land on the same version boundary.

**A fixed marker.** The interpreter emits a dedicated `PyRuntime` section containing
the eight bytes `"xdebugpy"`. Those bytes are identical in every 3.13+ build, so a
candidate address can be confirmed *without knowing that build's layout* — which is
exactly the confirmation that was impossible before (§4).

**Self-published offsets.** The build bakes its own `offsetof()` values into a
`_Py_DebugOffsets` struct that a reader can retrieve from the running process. The
offsets stop being something we determine about CPython and become something CPython
states about itself. What remains is one open question — which *definition* those
bytes should be read through — and §5 is about why that question is not trivial.

**Consent to being attached to.** On macOS the entitlement
`com.apple.security.get-task-allow` is what admits a same-user caller to another
process's task port. Every macOS framework build runs a hardened runtime, but only
3.13+ ships that entitlement. So on one machine, with one user, gcscope attaches to
3.13 unprivileged and needs `sudo` for 3.12 — a permission difference that is
CPython's doing, not the platform's.

The cost of PEP 768 is that it is a boundary, not a migration. Two eras have to be
supported side by side for as long as 3.12 is in use: one where the interpreter
describes itself, one where it must be described from outside.

→ gcscope's response: [ADR 0002](adr/0002-version-split-runtime-finding.md); attach
recipes in [`README.md`](../README.md).

---

## 4. Before PEP 768: nothing published, and nothing to anchor on

For 3.8–3.12 both halves of the previous section are missing, and the second absence
is the more interesting one.

Offsets have to be extracted from CPython's headers by hand, per minor version. They
genuinely differ: across those five releases the interpreter's id, its thread-list
head, and its GC state each moved more than once — the id alone sits at four
different offsets between 3.8 and 3.12. There is no shortcut that covers the range.

The harder problem is confirmation. With no marker, no constant proves an address is
`_PyRuntime`. The symbol table gives a candidate, but a symbol can be stale,
differently decorated, or resolved against the wrong image, and the failure is silent
(§1). The only remaining evidence is the layout itself: read the memory as though the
description were right, follow the interpreter and thread pointers, and see whether
the graph closes back on itself. Structure that consistent does not arise by
accident.

That inverts the resolution order. On 3.13+ the runtime is found first and its
offsets are read afterwards. Before 3.13 the offsets are the test, so they must be in
hand *before* the search begins — which is possible only because they are derived
from the version alone, and the version was obtained in §2 without touching the
runtime.

The cost is five hand-maintained descriptions whose correctness cannot be established
by inspection, only by running against a real interpreter of that version.

→ gcscope's response: [ADR 0002](adr/0002-version-split-runtime-finding.md),
[ADR 0003](adr/0003-layout-driven-gc-stats-decode.md).

---

## 5. What the ABI freeze does and does not promise

Once a minor version is released, CPython freezes its internal layout for the rest of
that line: 3.15.0, 3.15.1 and 3.15.2 place the same fields at the same offsets. Any
one of them therefore describes the others correctly, and a description written for a
patch release that was never seen is not a guess.

Within a *pre-release* cycle nothing is frozen, and the 3.15 cycle demonstrates it
twice. `3.15.0b1` shrank the per-collection GC stats entry from 96 bytes to 64.
`3.15.0b4` inserted a field into the thread state, shifting every field after it by
8 bytes. Neither change is visible from outside except by reading through the wrong
description and getting the wrong answer.

The two halves are asymmetric in a way that is easy to get backwards: a released
build may safely borrow a sibling release's description, and a pre-release may borrow
nothing — not from a later release, and not from the beta immediately before it. A
final release cannot borrow from its own rc either, since the freeze begins at
release, not at the rc.

The cost is that every alpha, beta and rc has to be described explicitly or refused
outright. Approximating one is worse than failing: refusal is loud, and a wrong
description is silent. It also means a CI leg tracking the newest pre-release goes
red whenever CPython ships one — which is a working alarm, not a breakage.

→ gcscope's response: [ADR 0006](adr/0006-layout-registration-integrity.md).

---

## 6. Where GC state lives, and how it reshaped

The GC state has moved once, and its statistics have changed shape once.

**Where it lives.** In 3.8 the GC state is global: one `_gc_runtime_state` inside
`_PyRuntime`, shared by every interpreter in the process. From 3.9 it is per
interpreter, reached by walking the interpreter chain. A reader that assumes one of
these silently reports the other's memory — 3.8 has interpreters to walk, and their
memory at the expected offset is *something*, just not GC state.

**What the statistics are.** From 3.8 through 3.14 each generation has exactly one
record of running totals — collections, collected, uncollectable — stored inline in
the GC state at a fixed position. That record's shape held for seven releases, but
its position did not: it sits at `0x80` through 3.13 and at `0x78` in 3.14. A shape
being stable says nothing about where it is.

3.15 replaced the totals with a **ring of recent collections** behind a pointer. Each
generation keeps the last N collection records plus a write cursor, and a record
carries timestamps, a duration, and heap size — a history of individual collections
rather than a running count.

```
inline (3.8 – 3.14)                    ring (3.15+)
fixed position in the GC state         behind a pointer in the GC state

┌──────────────────────────┐           ┌──────────────────────────┐
│ gen 0: totals            │           │ gen 0: record 0 … N-1    │
├──────────────────────────┤           │        write cursor      │
│ gen 1: totals            │           ├──────────────────────────┤
├──────────────────────────┤           │ gen 1: record 0 … M-1    │
│ gen 2: totals            │           │        write cursor      │
└──────────────────────────┘           ├──────────────────────────┤
                                       │ gen 2: …                 │
one record per generation,             └──────────────────────────┘
three fields, fixed count              N and M depend on the build (§7)
```

The two shapes need different reads — one is a fixed-size read at a known offset, the
other requires following a pointer first — and the field set differs not just between
them but between 3.15 pre-releases (§5). Which fields exist is therefore a property
of the build, discoverable only from that build's own description; it cannot be a
fixed structure with optional members, because "absent" and "present but zero" are
different answers and both occur.

→ gcscope's response: [ADR 0003](adr/0003-layout-driven-gc-stats-decode.md),
[ADR 0007](adr/0007-gcstat-layout-driven-view.md).

---

## 7. Build configuration varies independently of version

The version does not determine the layout on its own, because two builds of the same
version can differ.

**Free-threaded builds.** A free-threaded (no-GIL) build sizes its GC stats rings
differently from the GIL build of the same version — where the GIL build keeps eleven
records for the young generation and three for each older one, the free-threaded
build keeps one apiece. Same version, same hex, different geometry.

**Forks that add instrumentation.** A fork that extends the per-collection record —
for example one adding per-phase timings, growing an entry from 64 bytes to 208 —
does not change `patchlevel.h`. It reports the same `PY_VERSION_HEX` as the release
it derives from, and its `_Py_DebugOffsets` can be byte-identical, since only the
stats record changed. Nothing in the version identifies it.

So the version hex is not a complete key, and the discriminator has to be something
observable from outside the process. The one such thing is the size the process
publishes for its own stats region: with 64-byte records that region is 1112 bytes,
with 208-byte records 3560. The difference is what makes the two builds separable at
all.

The cost is an invariant rather than a mechanism: two builds sharing a version whose
published sizes *coincide* cannot be distinguished from outside, at all. There is no
later fix for that case — it has to be prevented, by refusing to support such a pair
rather than by choosing between them.

→ gcscope's response: [ADR 0006](adr/0006-layout-registration-integrity.md).

---

## 8. Platform image facts cannot be inferred

The third axis is the executable format, and it is the one where reasoning from
documentation is most tempting and least reliable.

The same interpreter, built for three platforms, differs in ways that all matter:
CPython's section-emitting macro produces a dotted `.PyRuntime` on Linux but not
elsewhere, and PE truncates section names to eight characters, so the section is
named differently in all three images. Mach-O prefixes C symbols with an underscore.
macOS ships universal binaries, so offset 0 is a fat header rather than an image, and
each slice's internal file offsets are relative to that slice. The image base is the
first mapping on ELF and PE, but on macOS the kernel attributes unrelated low-address
reservations to the image path, so the base is the first *executable* mapping.

The first live run across three operating systems failed every non-Windows leg on
five assumptions of this kind, each of which had looked like a portable fact. Two are
worth keeping in mind. One failed silently: a wrong image base still pointed into a
mapped region, so reads succeeded and returned garbage, and only the `"xdebugpy"`
marker caught it. Another presented as the wrong kind of bug entirely: parsing a fat
image in place returned no symbols, which broke only symbol-driven paths — and since
those are exactly the pre-3.13 paths (§4), a format defect looked like a version
defect.

The cost is a rule about evidence: a per-platform image fact is established by
running on that platform, never by deriving it from a format specification. That
makes the operating system a real axis of the test matrix rather than a
nice-to-have.

→ gcscope's response: [ADR 0004](adr/0004-per-platform-image-layout.md).

---

## 9. What the forces cost together

The axes are independent, so they multiply. Version × build configuration × platform
is not a list of variations on one problem; a correct read on Linux 3.12 predicts
nothing about macOS 3.15 free-threaded, because the code paths that differ between
them are the ones §2, §4, §6 and §8 each fork on separately.

Combine that with §1 — a wrong offset produces plausible output rather than an error
— and the consequence is uncomfortable but simple: reasoning cannot establish that a
combination works, and neither can a test that only checks something was returned.
The only evidence is having run that combination against a real interpreter and
checked the *shape* of what came back.

That defines what "supported" honestly means here. It is the grid that has actually
been run: three operating systems across 3.8 through 3.15, plus a free-threaded
build, plus source-built legs for the two builds no installer provides — a
GC-instrumented fork and an in-development 3.16. A combination outside that grid may
well work. It has not been shown to.

Two consequences of the grid are deliberate. The newest 3.15 pre-release is left
floating rather than pinned, so CI goes red when CPython ships one that has no exact
description (§5) — the alarm is the point. And a version with no installer is built
from source at a recorded commit, because an in-development branch drifts, and
"3.16" alone does not identify anything.

→ gcscope's response: [ADR 0005](adr/0005-testing-strategy.md).
