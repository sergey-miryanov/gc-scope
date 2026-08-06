# 0015 — Build, test and publish `gcscope-probe` wheels

- **Status:** Not started
- **Kind:** feature — ergonomics
- **Effort:** M
- **Origin:** Grilling session 2026-08-06 on productizing the Probe. The prototype builds with
  three `.bat` files that hardcode `C:\Python\Python314.5` and a Visual Studio 2022 Community
  path; there is no packaging of any kind.
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md) (platform image facts
  are CI-verified, not assumed), [ADR 0005](../docs/adr/0005-testing-strategy.md) (a live matrix
  asserting decoded shape is the correctness gate)
- **Blocked by:** [0013](0013-probe-portable-core.md) — there is nothing portable to build until
  the port lands. The packaging skeleton can be written against Windows first.

## 1. Problem statement

There is no way to obtain a Probe except to build one, and building one requires Visual Studio,
a CPython installed at a hardcoded path, and a Windows machine.

For an operator that is the end of the conversation. For a gcscope maintainer it is worse than
it looks: with no automated build there is nothing that would notice the port regressing on a
platform nobody is currently sitting at, and the platform where the most interesting defect
lives — publication ordering on weak memory — is one no maintainer runs day to day.

There is also a gap that opens the moment the repos merge. [0014](0014-read-probe-regions.md)
makes gcscope's decoder and the Probe's writer two halves of one contract. Path-filtered CI —
which is what keeps a 27-leg matrix and a wheel matrix from running on each other's pull requests
— means a change to the decoder can break Probe reading with no leg going red on the pull request
that did it. The contract would then break in a workflow that did not run.

## 2. Solution

`pip install gcscope-probe` works on Linux, macOS and Windows, on x86-64 and arm64, for CPython
3.13 and 3.14, with no compiler on the operator's machine.

Wheels are built and tested by CI across that matrix and published from a tag. The arm64 legs run
on real ARM hardware rather than under emulation, because the correctness fix they exist to guard
is one emulation can hide. And gcscope's own pull requests carry a single always-on leg that
builds a Probe, attaches to it, and asserts the invariants — so the shared contract cannot break
from the reader's side unnoticed, without every gcscope change paying for the full wheel matrix.

## 3. User stories

1. As an **operator**, I want to install a Probe with `pip` and no toolchain, so that adding it
   to a container image is a one-line change.
2. As an **operator on Alpine**, I want a musllinux wheel, because containers are where this gets
   deployed and there is no source fallback.
3. As an **operator**, I want a clear failure when I install on an unsupported interpreter, rather
   than a wheel that installs and refuses at import for reasons I have to read code to
   understand.
4. As a **gcscope maintainer**, I want a change to the decoder that breaks Probe reading to go red
   on my pull request.
5. As a **gcscope maintainer**, I want the wheel matrix *not* to run on pull requests that touch
   only the reader, so that turnaround stays what it was.
6. As a **gcscope maintainer**, I want the arm64 leg to exercise real weak memory, so that a green
   result means something.
7. As a **gcscope maintainer**, I want a release to be a tag and nothing else — no local build, no
   uploaded credential.
8. As a **gcscope maintainer**, I want a build environment missing the internal headers to fail
   with a named error rather than a compiler diagnostic about a missing include.

## 4. Implementation decisions

### Packaging

`setuptools`, one `Extension`, two sources. [0013](0013-probe-portable-core.md)'s decision to
compile the interpreter offsets in via a second translation unit removes the build-time code
generation that would have justified anything heavier — there is no generated source, nothing to
execute at build time, and nothing to sequence. `meson-python` and `scikit-build-core` were
weighed and rejected as build-graph machinery for a two-file target, at the cost of a toolchain
every future contributor has to learn.

`cibuildwheel` drives the matrix. Distribution name `gcscope-probe`, import module
`gcscope_probe`.

### Matrix

Two minors × seven platform tags:

| Platform | Tags |
|---|---|
| Linux | manylinux and musllinux, `x86_64` and `aarch64` |
| macOS | `arm64`, `x86_64` |
| Windows | `AMD64` |

Free-threaded builds are not in the matrix; [0013](0013-probe-portable-core.md) refuses them at
import, and shipping a `t` wheel that loads and reports a permanently zero `heap_size` would be
worse than shipping none.

**Wheels only, no sdist.** Deliberate, and revisitable if users ask. It makes musllinux
load-bearing rather than optional — without a source fallback, no musllinux wheel means Alpine
has no path at all.

**Native arm64 runners, not emulation.** This is the one matrix decision that is not about speed.
The defect [0013](0013-probe-portable-core.md) fixes — a release fence followed by a plain store,
which does not order publication on weak memory — cannot manifest under an emulator that does not
reproduce reordering. A QEMU-green aarch64 leg would prove nothing about the only bug the port
introduces, while looking exactly like proof.

### Workflow split

`probe.yml` owns the expensive axes: the full `cibuildwheel` matrix, the in-process tests, the
arm64 run. Path-filtered to Probe changes.

`rust.yml` gains exactly **one** unconditional leg — Ubuntu, 3.14 — that builds a Probe, runs a
target process, attaches with gcscope and asserts [0014 §5](0014-read-probe-regions.md)'s
invariants. A couple of minutes on every gcscope pull request, in exchange for the shared contract
being unable to break silently from the reader's side.

Rejected: **folding everything into `rust.yml`** (every reader change pays for the wheel matrix),
and **full separation with path filters both ways** (fast, and leaves the contract unguarded in
exactly the direction that matters).

### Environment assertions

Two checks that turn confusing failures into named ones, both in the shape of
[ADR 0004](../docs/adr/0004-per-platform-image-layout.md) — platform facts are verified, not
assumed:

- **`Include/internal` present** in each build environment, checked before compiling. Its absence
  otherwise surfaces as a missing-header diagnostic that reads like a bug in the source.
- **The header symbol is genuinely exported** from each built wheel — present in the PE export
  table, the ELF `.dynsym`, the Mach-O export trie. A build that picks up `-fvisibility=hidden`
  produces a module that imports and runs perfectly and is invisible to discovery, with no error
  anywhere. Asserted per platform on the built artifact, not inferred from compiler flags.

### Release

Tag-triggered on `gcscope-probe-v*`, reusing `probe.yml`'s build job. **PyPI trusted publishing
(OIDC)** rather than a stored token, consistent with this repo's existing pinned-action-SHA
posture. gcscope's own tags do not trigger a wheel release, and vice versa: the two artifacts
share a repo and not a release train.

Versioning: the PyPI version is what an operator pins; `gcscope_probe_header.version` is what
gcscope validates. They move independently, and neither implies the other.

## 5. Seams and testing decisions

- **Seam:** the built wheel — installed into a real interpreter and attached to from outside.
  Testing a build system through anything other than its artifact tests the intent rather than the
  result.
- **New seam needed:** none. The contract leg reuses `PySession` exactly as
  [0014](0014-read-probe-regions.md) does.
- **What makes a good test here:** assert the *artifact's* properties — that the symbol is
  exported, that the wheel refuses the interpreters it should, that a Probe built by CI decodes
  through gcscope's own path. A leg that only proves compilation succeeded proves the least
  interesting thing.
- **Prior art:** `rust.yml`'s `live-smoke` job for attach-and-assert-shape under CI permissions
  (`ptrace_scope`, macOS `taskport`), and `matrix-unpinned` for the pattern of a CI leg that
  exists to stop a guard being silently disabled.
- **Cases:**
  1. Every wheel in the matrix imports on its target interpreter and publishes Records.
  2. Symbol-export assertion passes per platform, and **fails** if visibility is forced hidden —
     verified by asserting the check itself catches it, not merely that it passes today.
  3. The contract leg in `rust.yml` runs on a pull request touching only reader code, and goes red
     if the decoder stops reading a valid Probe region.
  4. `probe.yml` does **not** run on a reader-only pull request, and does run on a Probe-only one.
  5. Installing on 3.12, on 3.15, or on a free-threaded build fails with a named reason.
  6. The arm64 leg runs on native hardware — asserted, not assumed, since a misconfigured runner
     label silently reverts to emulation.

## 6. Out of scope

- **An sdist.** Deferred, not rejected. Adding one later changes no decision here; the runtime
  guards from [0013](0013-probe-portable-core.md) already fail closed on an unvalidated build.
- **conda-forge, distro packaging, vendoring guidance.** Downstream of a first release existing.
- **Publishing gcscope itself to PyPI or crates.io.** The workspace dissolved the path-dependency
  problem that was the only forcing reason; whether gcscope wants library consumers is a separate
  question.
- **Signing wheels or attestations beyond what trusted publishing provides.** Worth revisiting
  once the threat-model note in [0014 §7](0014-read-probe-regions.md) is answered.
- **Benchmarks in CI for the Probe.** [ADR 0009](../docs/adr/0009-performance-guarded-by-shape.md)
  applies unchanged: the per-invocation cost is already below the noise floor of the Collection it
  observes, and a wall-clock CI benchmark would measure the runner.

## 7. Further notes

**Name reservation, worth doing before anything else in this spec.** `gcscope-probe`, `gcprobe`
and `gcscope` are all currently unregistered on PyPI. Reserving all three costs nothing and
prevents someone else owning the name of a tool that reads process memory. `gcprobe` is *not* the
chosen name — a plain search for it returns medical assay results, which is why the name moved —
but leaving it available to a third party is a different question from not using it.

**Open question for when this is picked up.** Whether the contract leg in `rust.yml` builds the
Probe from source each run or consumes a cached artifact from the most recent `probe.yml` run. The
first is simpler and always current; the second is faster but can go stale in exactly the way this
leg exists to prevent. Lean toward the first unless it proves slow in practice.
