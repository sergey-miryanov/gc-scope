# 0006 — Layout registration and resolution integrity

**Status:** Accepted — implemented 2026-07-20 … 07-21. (Part of the work that supersedes
`docs/tests-harness-plan.md`; complements [ADR 0005](0005-testing-strategy.md).)

## Context

Offsets come from bindgen'd `_Py_DebugOffsets` structs (3.13+) selected by version hex, or
hardcoded tables (pre-3.13). The failure mode is not a crash: **a wrong-but-plausible layout
fails open** — it reads mapped memory and returns garbage, caught only by the `"xdebugpy"`
cookie or a shape/ring check. Four forces make this sharp:

- `_Py_DebugOffsets` is ABI-frozen across a minor's *patch* releases but **not** across its
  pre-release cycle — 3.15.0b1 shrank `gc_generation_stats`, and 3.15.0b4 inserted a field
  that shifted every later field. Approximating a pre-release from a neighbour fails open.
- Two builds can share a version hex: a clean release and a gc-instrumented **`+inc`** build
  (both `0x030f00b1`), told apart only by `generation_stats_size`.
- An **in-development** version (3.16 dev = `main`) drifts continuously; there is no oracle
  for which snapshot is current, and it cannot be pinned in CI without recording its exact
  commit.
- The `LAYOUTS` registration row is the one step not compiler-enforced (a copy-paste that
  keeps the previous variant compiles and mis-decodes at runtime).

## Decision

1. **Exact-or-refuse for pre-releases.** `resolve_fallback_layout` substitutes a same-minor
   layout only when both target and candidate are **final** (`level == 0xF`); a pre-release
   with no exact layout is refused, never approximated.
2. **Fail-closed on decode.** `PySession::gc_stats` hard-errors when the process-published
   ring size disagrees with the compiled layout — the durable guard against any future
   mid-cycle struct change, on every OS, with no new test.
3. **Same-hex candidates by ring size.** `+inc` registers as a `GC_CANDIDATES` entry — a
   distinct GC layout under the *shared* nav variant — selected at read-time by the
   published ring size. Candidates within a hex must have distinct ring sizes (a compiled-in
   invariant, unit-tested).
4. **Provenance in every generated module** — `// gcscope-source: <repo>@<40-sha>`. A build
   off a release tag is marked `ONGOING`; the CI leg for an ongoing version reads its pinned
   commit from that line, so **the layout file is the single source of truth** — regenerate
   the offsets and the CI pin moves with them, no workflow edit.
5. **Registration guards in `scripts/gen-offsets.py`.** At most one `ONGOING` layout may be
   registered at a time (two drifting dev snapshots have no oracle). A `--suffix` same-hex
   build's `_Py_DebugOffsets` must be **byte-identical** to the registered nav variant's,
   refused on drift with the differing sub-struct named — the safety net for a same-hex
   candidate on an unfrozen base (e.g. a future `+inc2` on 3.16, which must be built from the
   same commit as clean 3.16).
6. **CI verifies the builds `setup-python` can't supply** by building them from source on
   Linux — `+inc` from its fork branch, 3.16 from the provenance-pinned commit — and running
   them through the same live driver ([ADR 0005](0005-testing-strategy.md)). Chosen over a
   machine-specific local manifest harness: CI builds exactly what it needs, so there is no
   target to discover and skip.

## Consequences

- A mis-registration — copy-pasted `LAYOUTS` row, a drifted `+inc`, a second ongoing
  version — is caught at generation time or by a live leg, not by a user reading garbage.
- The provenance pin makes "regenerate the 3.16 offsets" a one-step change that carries CI
  with it; the shape assertions of ADR 0005 are what surface a resolution error live.
- Retires the previously-planned local, manifest-driven test harness.
