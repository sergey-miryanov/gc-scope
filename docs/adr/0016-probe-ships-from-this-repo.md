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

1. **One repo.** The Probe lives here as `gcscope_probe/`; its verifier becomes a Cargo
   workspace member, which is what removes the path dependency.
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
5. **gcscope is not published as a library** to satisfy this. The workspace removed the only
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
