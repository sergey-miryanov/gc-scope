# CPython version support: what varies, and why it's hard

gcscope reads a running CPython process's garbage-collector state from outside it:
no debugger, no code injected into the target. Everything it reads sits at a byte
offset inside an internal C struct, and CPython guarantees nothing about those
structs across versions, builds, or platforms.

This document covers the variability: what changes, why each change makes attaching
harder, and what it costs. It decides nothing.

- **What gcscope decided in response**: the [ADRs](adr/). Each section ends by naming
  the one that owns its subject.
- **What is supported today, and how to use or extend it**: [`README.md`](../README.md).

## Contents

- [CPython version support: what varies, and why it's hard](#cpython-version-support-what-varies-and-why-its-hard)
  - [Contents](#contents)
  - [Why offsets vary, and why a wrong one fails open](#why-offsets-vary-and-why-a-wrong-one-fails-open)
  - [1. Finding the interpreter in the process](#1-finding-the-interpreter-in-the-process)
  - [2. Knowing which version you are looking at](#2-knowing-which-version-you-are-looking-at)
  - [3. Locating the runtime](#3-locating-the-runtime)
    - [PEP 768: the interpreter starts describing
      itself](#pep-768-the-interpreter-starts-describing-itself)
    - [Before PEP 768: nothing published, and nothing to anchor
      on](#before-pep-768-nothing-published-and-nothing-to-anchor-on)
  - [4. Deciding which layout describes the build](#4-deciding-which-layout-describes-the-build)
    - [What the ABI freeze does and does not
      promise](#what-the-abi-freeze-does-and-does-not-promise)
    - [Build configuration varies independently of
      version](#build-configuration-varies-independently-of-version)
  - [5. Reaching the GC statistics](#5-reaching-the-gc-statistics)
  - [6. Decoding the entries](#6-decoding-the-entries)
  - [7. Platform image facts cannot be inferred](#7-platform-image-facts-cannot-be-inferred)
  - [8. What the forces cost together](#8-what-the-forces-cost-together)

---

## Why offsets vary, and why a wrong one fails open

- CPython's public C API is stable. Its internal structures (`_PyRuntime`,
  `PyInterpreterState`, `_gc_runtime_state`) are not, and can change in any release.
- gcscope reads them anyway, so every field position it uses is a fact about one
  build.
- A wrong offset does not crash. The address still lands in a mapped page, the read
  succeeds, and a plausible integer comes back: a timestamp reported as a collection
  count, a size reported as a pointer. The same branches execute, and the output is
  indistinguishable from a correct read.

Resolution runs in six steps, and sections 1 to 6 take them in order:

1. find the Python image mapped into the process
2. learn its version
3. locate the runtime (`_PyRuntime`)
4. decide which layout describes this build
5. reach the GC statistics
6. decode the entries

Steps 3 and 4 swap before 3.13 (see [Before PEP 768](#before-pep-768-nothing-published-and-nothing-to-anchor-on)).
Sections 7 and 8 are not steps: they cut across all six.

Three axes drive the variation: version, build configuration, platform. Their
boundaries do not line up.

---

## 1. Finding the interpreter in the process

The interpreter is not a fixed file, and the process name does not identify it.

- **It may be the executable.** `/usr/bin/python3.12` can carry the interpreter
  outright.
- **It may be a shared library.** `libpython3.12.so` or `python313.dll`, with the
  executable acting as a launcher.
- **It may be both.** Many distributions ship an executable linked against the shared
  library, so the process maps two files that both look like Python.
- **It may be neither, by name.** An application that embeds CPython links libpython
  under its own name, and its command line says nothing about Python.

Later steps read different things out of whichever file they get:

| Step | Needs from the image |
|---|---|
| 2, version | the `Py_Version` symbol, or the read-only data section ([section 2](#2-knowing-which-version-you-are-looking-at)) |
| 3, runtime | the `PyRuntime` section, or the `_PyRuntime` symbol ([section 3](#3-locating-the-runtime)) |

Those are independent properties of a file. A stripped binary has sections but no
symbols; a launcher has neither while the library it loads has both. In a process
carrying an executable and a shared library, one file can answer step 2 and the other
step 3, so each candidate has to be tried for each purpose rather than chosen once.

An address inside the file is also not an address inside the process. Converting one
to the other needs the image's load base, and where that base sits depends on the
platform ([section 7](#7-platform-image-facts-cannot-be-inferred)).

Cost: identifying the interpreter is a search over the process's mapped files, and
choosing the wrong one yields a wrong base. A wrong base still reads mapped memory
and returns numbers, so the search cannot detect its own failure. What catches it is
the confirmation in step 3, two steps later and on evidence of a different kind.

→ gcscope's response: [ADR 0004](adr/0004-per-platform-image-layout.md) covers the
load base; the search over mapped files has no ADR of its own.

---

## 2. Knowing which version you are looking at

Steps 3 and 4 dispatch on the version, and steps 5 and 6 on what step 4 produces, so
the version resolves before any of them. Three sources exist, and which of them a
build provides depends on its version.

- **3.8 – 3.10: a string in the binary.** Nothing exports the version as a readable
  variable, so the only record of it is the `PY_VERSION` literal the compiler put in
  the image (`3.14.0`, NUL-terminated). You scan the read-only data section for it
  and parse what matches:

  ```
  3.<minor>.<micro>[ a<serial> | b<serial> | rc<serial> ]<terminator>
  ```

  The major is fixed at `3`, and the optional suffix supplies the release level for a
  pre-release build. Three constraints keep the match honest, each of them answering
  a false positive that occurs in that same section:

  - the `3` must not follow a digit, or `lib13.12.0` reads as a version
  - the micro component is mandatory, so a stray `3.1` cannot shadow the real `3.10.4`
    later in the section
  - a terminator must follow: NUL, space, `(`, `"`, or whitespace

  The section holds other strings that embed the version. A 3.14 image carries the
  build tag `v3.14.0-dirty` a few bytes from the literal itself; the terminator rule
  is what rejects it, since `-` is not a terminator.

  When no literal matches, detection fails rather than returning a value. A
  non-version literal that satisfies all three constraints is accepted.

- **3.11+: an exported variable.** CPython 3.11 added `Py_Version`, a global holding
  `PY_VERSION_HEX`. The binary's symbol table gives the variable's address, and the
  live process gives its value. `patchlevel.h` packs that value as:

  ```
  (major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial

  level:
    0xA alpha
    0xB beta
    0xC release candidate
    0xF final

  0x030f00b4  =  3.15.0b4
  0x030d01f0  =  3.13.1 final
  ```

  The level nibble is the field that separates a pre-release from a shipped build,
  which is what the [ABI freeze rules](#what-the-abi-freeze-does-and-does-not-promise)
  key on.

- **3.13+: a field the running build publishes about itself.**
  [PEP 768](#pep-768-the-interpreter-starts-describing-itself) has the interpreter
  emit a `_Py_DebugOffsets` block, which starts with the eight marker bytes
  `"xdebugpy"` followed by that build's `PY_VERSION_HEX`. Finding the marker in the
  process both locates the block and proves it is one, so the version comes out
  without consulting a symbol table.

When two sources disagree, the published field takes precedence. The sources answer
different questions: `Py_Version` reports the version of the image on disk, read at an
address that image supplies, while the published field reports the version of the
block whose offsets step 4 must match. They diverge when an interpreter is upgraded in
place and keeps running the old image, and when a string scan matches the wrong
literal.

Cost: on 3.13+ the version from step 2 selects the finder only, and the process
replaces it before step 4.

Note also that the boundaries here are detection boundaries. 3.11 is one of several
places where the pre-3.13 struct layouts shift as well, but the two need not
coincide, and nothing about how a build reports its version predicts how its
structures are arranged.

→ gcscope's response: [ADR 0002](adr/0002-version-split-runtime-finding.md).

---

## 3. Locating the runtime

`_PyRuntime` is a single global. Every step after this one starts from its address,
and the process exposes no direct way to obtain it.

- **Input:** the image and its load base (step 1), the version (step 2).
- **Output:** a confirmed `_PyRuntime` address.

Both eras derive a candidate address and then confirm it. They differ only in the
available evidence:

| Build | Candidate from | Confirmed by |
|---|---|---|
| 3.13+ | the `PyRuntime` section address | the `"xdebugpy"` marker at that address |
| 3.8 – 3.12 | the `_PyRuntime` symbol | a pointer round-trip through the interpreter and thread structures |

### PEP 768: the interpreter starts describing itself

CPython 3.13 implemented [PEP 768](https://peps.python.org/pep-0768/), which turned
external debugger attachment into a supported operation. Three parts matter here, and
all three land on the same version boundary.

- **A fixed marker.** The interpreter emits a dedicated `PyRuntime` section holding
  the eight bytes `"xdebugpy"`, identical in every 3.13+ build. You confirm a
  candidate address without knowing that build's layout, which settles step 3 on its
  own.
- **Self-published offsets.** The build bakes its own `offsetof()` values into a
  `_Py_DebugOffsets` struct, which a reader retrieves from the running process. That
  hands step 4 its raw material and leaves one question open, which
  [section 4](#4-deciding-which-layout-describes-the-build) takes up: which definition
  to read those bytes through.
- **Consent to being attached to.** On macOS the entitlement
  `com.apple.security.get-task-allow` admits a same-user caller to another process's
  task port. Every macOS framework build runs a hardened runtime, but only 3.13+
  ships that entitlement, so you attach to 3.13 unprivileged and need `sudo` for
  3.12 on the same machine as the same user.

Cost: two eras to support side by side for as long as 3.12 stays in use, one where
the interpreter describes itself and one where it is described from outside. Attach
recipes are in [`README.md`](../README.md).

### Before PEP 768: nothing published, and nothing to anchor on

For 3.8–3.12 neither part exists. The offsets can be transcribed from CPython's
headers; the absent marker has no equivalent substitute.

- **Offsets** come out of CPython's headers by hand, one set per minor version. The
  interpreter's id alone sits at four offsets between 3.8 and 3.12; the thread-list
  head and the GC state each moved more than once.
- **Confirmation** has no constant to check. The symbol table hands you a candidate,
  but a symbol can be stale, carry a different decoration, or resolve against the
  wrong image, and that failure stays silent.

The layout itself is the only test left. Read a window at the front of the candidate
runtime and treat every 8-byte word in it as a possible `PyInterpreterState *`:

```
for each 8-byte word in the window, in order:
    candidate = the word
    tstate    = *(candidate + thread-list head)     the interpreter's first thread
    back      = *(tstate + owning interpreter)      that thread's interpreter
    stop at the first candidate where back == candidate

accept the runtime when   that candidate == *(runtime + interpreters head)
```

The round-trip establishes that a word points at something shaped like an
interpreter. Comparing it against `interpreters.head` establishes that it is the
interpreter this runtime owns. The order matters: the scan stops at the first word
whose round-trip closes and compares only that one, so a self-consistent pointer
lying earlier in the window than `interpreters.head` shadows the real head and the
runtime is rejected. The check errs in that direction. For it to accept a wrong
runtime, unrelated memory would have to close the round-trip and match the head
pointer, at the offsets this version's description specifies.

A NULL thread pointer disqualifies a candidate, and each pointer the walk
dereferences is checked against the process's mapped regions first. That check covers
the pointer, not the field offset added to it, so a read near the end of a mapping
still relies on the read itself failing.

Needing the description before the search is what inverts steps 3 and 4. On 3.13+ you
locate the runtime and then read its offsets out of it; before 3.13 the offsets are
the test, so you need them before the search starts. They follow from the version alone, which step 2 obtained without touching
the runtime.

Cost: five hand-maintained descriptions. No inspection validates them; only a run
against a real interpreter of that version does.

→ gcscope's response: [ADR 0002](adr/0002-version-split-runtime-finding.md).

---

## 4. Deciding which layout describes the build

- **Input:** the version (step 2), the runtime address and, on 3.13+, its block of
  self-published offsets (step 3).
- **Output:** the byte offset of every field the remaining steps read.

Steps 5 and 6 read fields at byte offsets, and the offsets are version-specific, so a
description of every supported build has to exist before any reading happens. What is
compiled in, and what the process supplies, differs by era:

| Build | Compiled in | Supplied by the process |
|---|---|---|
| 3.8 – 3.12 | the offset values, transcribed from CPython's headers | nothing |
| 3.13+ | the layout of the `_Py_DebugOffsets` block: which of its fields sits where | the offset values stored in those fields |

The 3.13+ arrangement has two levels, and both are version-specific:

- **The values.** The block holds the offsets of CPython's own structs, and the running
  build fills it in with its own `offsetof()` results. These need no maintenance here,
  which is what [PEP 768](#pep-768-the-interpreter-starts-describing-itself) bought.
- **The block.** `_Py_DebugOffsets` is itself a struct, and its layout changed across
  releases: 3.13 publishes fifteen named sub-structs, 3.15 publishes twenty-one. To
  take a value out of the block you have to know where in the block it sits, so a
  definition of the block is generated from each version's source tree and compiled
  in.

One thing stays compiled in for every version: CPython publishes no description of
the GC statistics region's geometry, so [section 5](#5-reaching-the-gc-statistics)
and [section 6](#6-decoding-the-entries) carry it. From 3.15 the build does publish
that region's total size, and that one number is usable as a cross-check. 3.13 and
3.14 publish only the GC state's size and its `collecting` offset, so their geometry
has no external corroboration at all.

Pre-3.13 the compiled description is the answer. From 3.13 it is the key for reading
the answer out of the process. Either way the version selects it, and two things
break that selection, one per subsection below:

1. The running build may be one no description was compiled for. Whether a
   neighbouring build's description may stand in is a question about CPython's
   release process.
2. The version may not identify the build at all.

### What the ABI freeze does and does not promise

A description compiled for one build is usable for another exactly as far as CPython
freezes its internal layout, and that boundary falls inside a minor version:

- **Across patch releases of a shipped minor: frozen.** 3.15.0, 3.15.1 and 3.15.2
  place the same fields at the same offsets, so any one of them describes the others,
  including a patch release you have never seen. CPython treats this as an
  obligation, not an accident: 3.14 shipped an incremental collector through 3.14.4
  and reverted to a generational one in 3.14.5, and the struct kept its shape across
  that change of algorithm.

  ```c
  /* 3.14.4, incremental */          /* 3.14.5, generational */
  struct gc_generation young;        struct gc_generation generations[NUM_GENERATIONS];
  struct gc_generation old[2];

  Py_ssize_t work_to_do;             /* dummy members to preserve other offsets */
  int visited_space;                 Py_ssize_t dummy1;  /* was work_to_do */
  int phase;                         int dummy2;         /* was visited_space */
                                     int dummy3;         /* was phase */
  ```

  Three generations either way, the same size in the same place, and placeholders
  standing in for the fields the incremental collector no longer needs. The revert
  also restored a `generation0` pointer that the incremental collector had dropped,
  and appended it at the end of the struct rather than returning it to its old
  position ahead of the statistics. `generation_stats` therefore sits at `0x78` in
  both, the per-collection entry is unchanged, and a description generated from
  3.14.4 still reads 3.14.5 correctly.
- **Inside a pre-release cycle: not frozen.** `3.15.0b1` shrank the per-collection GC
  stats entry from 96 bytes to 64. `3.15.0b4` inserted a field into the thread state,
  shifting every field after it by 8 bytes. From outside the process you see neither
  change, except by reading through the wrong description and getting the wrong
  answer.

What may borrow a description from what:

- a released build, from another release of the same minor
- a pre-release, from nothing: not a later release, and not the beta before it
- a final release, not from its own rc, because the freeze starts at release

Cost:

- Every alpha, beta and rc needs its own description or a refusal. A refusal reports
  an error; an approximation returns wrong numbers and reports nothing.
- A CI leg that tracks the newest pre-release turns red whenever CPython ships one,
  by design.

### Build configuration varies independently of version

Even with a description compiled for the exact version reported, that version can name
more than one build:

- **Free-threaded (no-GIL).** `Py_GIL_DISABLED` changes the GC statistics geometry:
  the GIL build keeps eleven entries for the young generation and three for each
  older one, the free-threaded build one apiece. It does not change the shape of
  `_Py_DebugOffsets`. That header has no conditionals inside the struct, only around
  the values assigned to its fields, so free-threading-only fields are present and
  zero in a GIL build and the same compiled description reads both.
- **A fork that adds instrumentation.** Extending the per-collection entry, say with
  per-phase timings that grow it from 64 bytes to 208, leaves `patchlevel.h` alone.
  Same `PY_VERSION_HEX`, and a byte-identical `_Py_DebugOffsets`, since only the
  stats entry changed.

The two cases are not equally tractable, and the difference is whether the build says
anything about itself:

- Free-threading is announced. The published block carries a `free_threaded` flag, so
  the geometry follows from a value read out of the process.
- A fork announces nothing. The only signal left is the stats-region size the build
  publishes, and only from 3.15, where that size exists:
  [section 6](#6-decoding-the-entries) derives 1112 bytes for a GIL build with 64-byte
  entries against 3560 for one with 208-byte entries. The size depends on the entry
  count as well as the entry size, so it separates builds only within one threading
  model. The same 64-byte entries give 216 bytes in a free-threaded build.

Cost: two builds that share a version, announce nothing about themselves, and publish
the same region size cannot be told apart from outside the process. Before 3.15 there
is no published size at all, so an instrumented fork of a 3.13 or 3.14 build has
nothing to separate it from the clean release. No later check resolves either case,
so such a pair has to be refused rather than chosen between.

→ gcscope's response: [ADR 0006](adr/0006-layout-registration-integrity.md) for the 3.13+
registry, and [ADR 0010](adr/0010-pre-3-13-offsets-stay-hand-maintained.md) for why the
pre-3.13 half of the table above is not generated from source to match it.

---

## 5. Reaching the GC statistics

Reaching the statistics region takes two hops from the runtime address: runtime to GC
state, then GC state to region. CPython changed each hop once.

| Build | GC state | Statistics region |
|---|---|---|
| 3.8 | global, in `_PyRuntime` | inline, at `0x80` in the GC state |
| 3.9 – 3.13 | per interpreter | inline, at `0x80` in the GC state |
| 3.14 | per interpreter | inline, at `0x78` in the GC state |
| 3.15+ | per interpreter | behind a pointer stored in the GC state |

- **The root moved at 3.9.** 3.8 keeps one `_gc_runtime_state` inside `_PyRuntime`,
  shared by every interpreter; from 3.9 each interpreter carries its own, reached by
  walking the interpreter chain. Assume the wrong one and you read the other's memory
  without noticing, because 3.8 has interpreters to walk and their memory at that
  offset holds something.
- **The region became indirect at 3.15.** Through 3.14 the statistics sit at a fixed
  offset inside the GC state, so the region address is arithmetic. From 3.15 the GC
  state holds a pointer to them, so reaching the region costs a read, and a NULL
  there is the ordinary "not allocated yet" state rather than an error.
- **The fixed offset itself moved at 3.14**, from `0x80` to `0x78`, when the
  incremental collector dropped a `generation0` pointer that had sat ahead of the
  statistics. The entry struct was untouched; only its position changed.

Cost: the two hops vary independently, so the route to the region is a per-build
description rather than a fixed path.

→ gcscope's response: [ADR 0003](adr/0003-layout-driven-gc-stats-decode.md).

---

## 6. Decoding the entries

What sits in the region changed shape once, at 3.15.

```
inline (3.8 – 3.14)                    ring (3.15+)

┌──────────────────────────┐           ┌──────────────────────────┐
│ gen 0: totals            │           │ gen 0: entry 0 … N-1     │
├──────────────────────────┤           │        write index       │
│ gen 1: totals            │           ├──────────────────────────┤
├──────────────────────────┤           │ gen 1: entry 0 … M-1     │
│ gen 2: totals            │           │        write index       │
└──────────────────────────┘           ├──────────────────────────┤
                                       │ gen 2: …                 │
one entry per generation,              └──────────────────────────┘
three fields, fixed count              N and M depend on the build (section 4)
```

An inline entry holds running totals: collections, collected, uncollectable. A ring
entry keeps those counters and adds per-collection detail — timestamps, a duration, a
heap size, a candidate count — so the region records recent collections individually
instead of only summing them.

The ring's geometry follows from two numbers, the entry size and the entry count per
generation:

```
base[0] = 0
base[1] = base[0] + entries[0] × entry_size + 8
base[2] = base[1] + entries[1] × entry_size + 8

a reader reads   base[2] + entries[2] × entry_size
the build says   base[2] + entries[2] × entry_size + 8
```

Each generation's buffer is its entries followed by a one-byte write index. The entry
struct contains a `double`, so it is 8-byte aligned, and the index plus its tail padding
costs 8 bytes rather than 1. The two totals differ by that same 8: the size the build
publishes counts generation 2's trailing index, and a reader has no reason to read it.
That published size is what
[distinguishing two builds of one version](#build-configuration-varies-independently-of-version) relies on:

| Build | entry | entries per gen | reader reads | build says |
|---|---|---|---|---|
| 3.15, GIL | 64 | 11, 3, 3 | 1104 | 1112 |
| 3.15, free-threaded | 64 | 1, 1, 1 | 208 | 216 |
| 3.15.0a8, GIL | 96 | 11, 3, 3 | 1648 | 1656 |
| instrumented fork, GIL | 208 | 11, 3, 3 | 3552 | 3560 |

Cost: the field set differs between 3.15 pre-releases
(see [the ABI freeze](#what-the-abi-freeze-does-and-does-not-promise)), so which fields exist is
a property of the build. A fixed struct with optional members cannot express that,
because "absent" and "present but zero" are different answers and both occur.

→ gcscope's response: [ADR 0003](adr/0003-layout-driven-gc-stats-decode.md),
[ADR 0007](adr/0007-gcstat-layout-driven-view.md).

---

## 7. Platform image facts cannot be inferred

One interpreter built for three platforms differs in ways that all matter:

| Fact | ELF | PE | Mach-O |
|---|---|---|---|
| `PyRuntime` section name | dotted, `.PyRuntime` | truncated to 8 characters | undotted |
| C symbols | undecorated | undecorated | underscore-prefixed |
| offset 0 of the file | the image | the image | a fat header; each slice's file offsets are slice-relative |
| image base | first mapping | first mapping | first *executable* mapping |

The first live run across three operating systems failed every non-Windows leg on
five assumptions of this kind. Two of the five failed in ways worth recording:

- A wrong image base still pointed into a mapped region. Reads succeeded, garbage came
  back, and only the `"xdebugpy"` marker caught it.
- Parsing a fat image in place returned no symbols, which broke only symbol-driven
  paths. Those are the pre-3.13 paths
  ([Before PEP 768](#before-pep-768-nothing-published-and-nothing-to-anchor-on)), so a format defect
  presented as a version defect.

Cost: you establish a per-platform image fact by running on that platform, never by
deriving it from a format specification. That makes the operating system a real axis
of the test matrix.

→ gcscope's response: [ADR 0004](adr/0004-per-platform-image-layout.md).

---

## 8. What the forces cost together

- The axes multiply: version × build configuration × platform. A correct read on Linux
  3.12 tells you nothing about macOS 3.15 free-threaded, because
  [section 2](#2-knowing-which-version-you-are-looking-at),
  [section 3](#3-locating-the-runtime),
  [section 4](#4-deciding-which-layout-describes-the-build),
  [section 5](#5-reaching-the-gc-statistics), [section 6](#6-decoding-the-entries) and
  [section 7](#7-platform-image-facts-cannot-be-inferred) each fork somewhere
  different, and the free-threaded half of that example forks in 4 and 6 alone.
- A wrong offset produces plausible output, so a test that checks only that something
  came back passes on garbage.
- What remains is a run against a real interpreter with the *shape* of the result
  asserted.

"Supported" therefore means the grid that has been run:

- three operating systems × 3.8 through 3.15
- a free-threaded build
- source-built legs for the two builds no installer provides: a GC-instrumented fork,
  and an in-development 3.16

Combinations outside that grid are untested.

Two properties of the grid are deliberate:

- CI tracks the newest 3.15 pre-release in place of pinning it, so a release with no
  exact description turns the leg red
  (see [the ABI freeze](#what-the-abi-freeze-does-and-does-not-promise)).
- A version with no installer is built from source at a recorded commit, because an
  in-development branch drifts and "3.16" on its own identifies nothing.

→ gcscope's response: [ADR 0005](adr/0005-testing-strategy.md).
