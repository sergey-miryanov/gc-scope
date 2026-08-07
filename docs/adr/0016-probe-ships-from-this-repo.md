# 0016 — The Probe ships from this repo, on its own release train

**Status:** Accepted — decided 2026-08-06. (Answers "why is a CPython C extension living in a
Rust repo?", which is the question the directory layout provokes. Complements
[ADR 0008](0008-reader-consumer-package-layering.md).)

## Context

The Probe began as a separate prototype — one commit, no remote — and the natural instinct is
to leave it separate: different language, different toolchain, different distribution channel,
different audience. Three facts argue the other way, and one of them is not negotiable.

**The path dependency is unpublishable.** The prototype's verifier depends on `gcscope` by
path (`../../gcscope`). That cannot be published. Keeping the repos separate forces a choice
between publishing gcscope to crates.io as a library — a public contract maintained
indefinitely for exactly one internal consumer — and abandoning the verifier. A workspace
member dissolves the problem instead of solving it.

> **Amended 2026-08-07, implementing the move.** The verifier stopped being a program instead
> of becoming a workspace member. It existed so a *separate repository* could reach gcscope's
> decoder, and the move removes that need: `tests/probe.rs` reaches the decoder directly on
> the live-attach harness this repo already has, and its four invariants are assertions rather
> than printed output that someone has to run and read. Having no second crate answers the
> force above more completely than a workspace would, so we dropped the conversion and the
> root `Cargo.toml` stays one package. The decision below is untouched: the Probe ships from
> this repo.

**The layout contract is bidirectional and silent.** The Probe writes a region this repo's
decoder reads. Its compile-time assertions encode the reader's expectations. Split across
repos, the assertion and the truth it protects live in different places, and drift surfaces as
misattributed numbers rather than a red build — the failure mode
[`docs/version-support.md`](../version-support.md) §6 is written about.

**The offsets machinery is shared, in one direction.** The Probe does not inherit the registry
([ADR 0013](0013-probe-offsets-are-compiled-in.md)), but it does consume the generated ring
layout and it does need the weekly sweep extended to the fields it compiles in. Both live in
`scripts/gen-offsets.py`. Across repos, that becomes a versioned artifact one repo publishes
for another it also owns.

Against merging: the two have genuinely different release cadences and opposite trust
profiles. gcscope is a tool you run; the Probe is code you put inside your own production
process. Bundling them into one artifact would make the second inherit the first's surface
area, and would force a Probe release for every CLI change.

The timing also mattered. The prototype had one commit and no remote — no history to preserve,
no external clone to break, no issue links to rewrite. The merge would never be cheaper than
at the moment it was decided.

## Decision

1. **One repo.** The Probe lives here as `gcscope_probe/`; its verifier becomes a gcscope
   integration test (`tests/probe.rs`), which is what removes the path dependency. *(As
   decided this was "a Cargo workspace member" — see the amendment above.)*
2. **Two release trains.** The Probe is versioned and tagged independently and published to
   PyPI on its own tag; gcscope's tags do not trigger a wheel, and vice versa. Merging the
   repos is not merging the products.
3. **The wire version is independent of the package version.** The region header's `version`
   is what gcscope validates; the PyPI version is what an operator pins. Neither implies the
   other.
4. **Path-filtered CI, plus one always-on contract leg.** The wheel matrix runs only on Probe
   changes and the reader matrix only on reader changes — but `rust.yml` carries a single
   unconditional leg that builds a Probe, attaches to it and asserts the invariants. Without
   it, path filters would leave the shared contract breakable from the reader's side with no
   leg going red on the pull request that broke it. That leg is the price of the filters and
   is not optional.

   > **Amended 2026-08-07, building the leg** (`rust.yml: probe-contract`). A `probe-scope` job
   > diffs the pull request and skips the leg when nothing under `gcscope_probe/`, `src/`,
   > `tests/`, `scripts/gen-offsets.py`, the manifests or `rust.yml` itself changed, so
   > "unconditional" above is now approximate. The force survives: the gate opens on all of
   > `src/`, so a change breaking the layout contract from the reader's side cannot reach main
   > without this leg having run. In exchange a docs-only or spec-only pull request keeps its
   > current turnaround. `paths:` in the `on:` block cannot do this, since it gates the whole
   > workflow file and every other job in `rust.yml` has to keep running.

   > **Also amended: how the leg gets a Probe that compiles.** We sequenced the leg before the
   > port to Linux it depends on, so the port landed with it. `<windows.h>` gave way to C11
   > `<stdatomic.h>`, `__declspec(dllexport)` to a visibility macro, and the integration test's
   > hand-written PE export parse to one goblin match over PE and ELF. The aarch64
   > publication-ordering fix stayed behind on purpose: it wants the native arm64 leg
   > `specs/0015-publish-probe-wheels.md` adds, and an x86-64 leg cannot show it working.
   >
   > The leg is Linux only. Nothing in CI compiles the Probe on Windows, so the platform that
   > was its only proven one now rests on someone building it by hand. Spec 0015's wheel matrix
   > closes that.
5. **gcscope is not published as a library** to satisfy this. The move removed the only
   forcing reason; whether gcscope wants library consumers is a separate decision that should
   be made on its own merits.
6. **Rejected: vendoring the constants with a scheduled drift job.** It detects, late and out
   of band, exactly the divergence this arrangement exists to prevent early.

## Consequences

- A reader who finds C, `pyproject.toml` and a wheel matrix in a Rust repository gets an
  answer here rather than assuming it accreted.
- `rust.yml` gains a permanent job whose purpose is to guard a contract rather than to test a
  feature, in the same spirit as `matrix-unpinned`. Removing it to speed up CI would silently
  reintroduce the failure mode the filters create.
- The repository name and the PyPI package name do not match — `gc-scope` hosting
  `gcscope-probe`. A README problem, not an architectural one, but it is the first thing a
  PyPI visitor sees.
- Two audiences now read one repository: contributors to a memory-reading CLI, and operators
  evaluating whether to put a C extension inside their production process. The second group
  arrives with different questions, and the top-level README has to serve them without
  burying the first.
- If the Probe ever outgrows this arrangement — its own contributors, its own issue flow, a
  cadence that fights gcscope's — extraction stays available and is a mechanical move, at the
  cost of reinstating a published layout artifact.
