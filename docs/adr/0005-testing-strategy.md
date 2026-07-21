# 0005 — Testing strategy: a live matrix is the correctness gate

**Status:** Accepted — implemented 2026-07-20 … 07-21. (Supersedes the completed
`docs/tests-harness-plan.md`, now deleted.)

## Context

gcscope's characteristic bug is a **wrong struct offset**, and a wrong offset executes
exactly the same lines as a right one. No unit test catches it — the logic is correct, the
*data* it reads is off by a field. Two further forces shape the gate:

- **A version that isn't installed can't be exercised**, so coverage is bounded by what a
  runner can obtain — and the OS axis matters, because that is where the finder forks and
  per-version module packaging varies (see [ADR 0002](0002-version-split-runtime-finding.md),
  [ADR 0004](0004-per-platform-image-layout.md)).
- **"Attached, got a non-empty table" is not correctness.** A mis-keyed decode emits a full
  table of *garbage* and passes such a check. This was not hypothetical: a fallback bug
  (`setup-python` resolved `3.15` to an unregistered `3.15.0b4`, decoded through the wrong
  candidate) stayed green under non-empty checks and was caught the first time shape was
  asserted.
- **Two paths no one-shot CLI run reaches** — the layout-cache hit and soft-reattach — are
  in-process `PySession` state with no CLI surface (the open item of [ADR 0001](0001-pysession-resolve-once-facade.md)).

## Decision

A layered gate, each layer matched to what it can actually prove:

1. **Unit layer** — in-file `#[cfg(test)]` tests over pure logic reachable without a
   process (version encoding, the `LAYOUTS` registry, GC-shape selection, ring/inline
   geometry, stat decode, monitor dedup). Two seams were extracted to make the
   version-dependent logic reachable off a live target: `OffsetTable::decode_gc_stats`
   (pure, takes `&[u8]`) and `monitor::select_fresh`. Runs in <1s with no Python; catches
   logic regressions, never offset errors.
2. **Live-smoke matrix — the correctness gate.** For each `(OS, version)` it spawns a real
   interpreter, attaches, decodes, and asserts **shape**: the expected `(kind, entries)`
   derived from the target's own `sys.version_info` + GIL flag, the exact
   `(generation, entry)` index set, and a strict `collections` pyramid across generations.
   The shared fixture (`tests/fixtures/spin.py`) seeds collections **20/5/1** so the
   generations are tellable apart — an even rotation would make a correct decode and one
   that aliases two generations onto the same entries indistinguishable. Shape, not
   "non-empty", is the whole point. `setup-python` supplies 3.8–3.15 + 3.15t; source-built
   legs supply the rest (see [ADR 0006](0006-layout-registration-integrity.md)).
3. **Lifecycle tests** (`tests/lifecycle.rs`, `#[ignore]`d, run in a permissioned CI leg)
   reach the two in-process paths. They are enabled by the **lib/bin split** — `tests/*.rs`
   is a separate crate that sees only the public API — plus one small honest signal,
   `PySession::layout_source() -> LayoutSource` (`Parsed`/`Cached`), chosen over a
   `#[cfg(feature = "test-hooks")]` hook so the tested configuration is the shipped one.
4. **Fail closed.** When the live path cannot confirm what it decoded, hard-error rather
   than emit plausible garbage (the ring-size mismatch guard; see ADR 0006).

## Consequences

- **The matrix is not sampled down.** The full `(OS × version)` cross-product runs because
  a wrong offset or platform fact fails the *specific* leg that depends on it; a sample
  would rest on an assumption the finder's per-OS/per-version forks don't justify.
- The `#[ignore]`d lifecycle tests keep the default `cargo test` green everywhere (no attach
  permission needed); a dedicated Linux job grants ptrace and runs them as a blocking gate.
- Closes the open item of [ADR 0001](0001-pysession-resolve-once-facade.md): the cache-hit
  and soft-reattach paths are now observed.
