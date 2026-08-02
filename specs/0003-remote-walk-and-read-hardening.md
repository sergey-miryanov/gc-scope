# 0003 — Harden the pre-3.13 word scan and the interpreter walk against hostile remote data

**Status:** Not started
**Kind:** bug — safety
**Effort:** S
**Origin:** 2026-07-18 review (finding C14, and the residue of C1 after the hang fix).
**Respects:** [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
[ADR 0010](../docs/adr/0010-pre-3-13-offsets-stay-hand-maintained.md)

Two independent hardening items against *remote* data — bytes read out of a foreign
process, which may be stale, torn, or garbage. Grouped because they are the last two places
where such data reaches unguarded code, and each is a few lines.

## 1. Problem

**A.** On Python 3.8–3.12, locating the runtime reinterprets a byte buffer read from the
target as a slice of 64-bit words through an unaligned pointer cast. It works on every
platform gcscope ships to today and will keep working — right up until a compiler upgrade
or a non-x86 target decides otherwise, at which point the failure is a miscompile, not a
diagnosable bug.

**B.** With `gc-stats --all` or under `monitor`, an interpreter chain that does not
terminate — a torn read during teardown, a garbage `next` pointer looping back — spins
forever at 100 % CPU with no output and no error. The operator sees a hang.

## 2. Evidence

### A — unaligned `&[u64]` view

`remote_debugging::check_interpreter::check_runtime` builds its word view like this:

```rust
let words: &[u64] = unsafe {
    std::slice::from_raw_parts(
        bytes.as_ptr() as *const u64,
        bytes.len() / std::mem::size_of::<u64>(),
    )
};
```

`Vec<u8>` guarantees only 1-byte alignment; `from_raw_parts` requires the pointer to be
aligned for `u64`. That is undefined behavior by Rust's rules regardless of what x86
tolerates. Two lesser problems ride along: the integer division silently drops up to seven
trailing bytes, and the cast reads in **host** byte order while every other read in the
crate goes through `from_le_bytes`.

The remaining `from_raw_parts` calls in the crate — in `offsets::display`, `offsets::mod`
and `offsets::validation` — are `*const u8` views of the `cookie` array. Alignment 1, sound
as written; leave them.

### B — unbounded walk

`PySession::gc_stats_per_interpreter` follows the chain until a NULL `next`. The original
hang — a NULL stats pointer taking a `continue` that skipped the advance — is fixed, and
the advance is now unconditional. What remains is that the loop trusts remote memory to
form a *finite* chain, with no cap and no error path if it doesn't.

## 3. Scope

**A — affected:** Python 3.8–3.12 attach only. `check_runtime` is the cookie-less anchor
for pre-3.13 runtimes and its sole caller is `memory::process::find_runtime_pre_3_13`;
3.13+ finds the runtime by the `"xdebugpy"` cookie and never enters the module (ADR 0002).

**B — affected:** `gc-stats --all` and `monitor` / `run`. The default single-interpreter
path breaks after the first iteration and cannot loop.

**Not affected by either:** decoding. Neither item changes which bytes are read or how they
are interpreted once read — A changes *how* the same little-endian words are assembled, B
adds a terminator.

**Why CI misses them:** A is UB that current compilers implement as the intended read, so
every leg is green and will stay green; B needs a corrupt target, which the matrix by
construction never produces.

## 4. Proposed change

**A.** Replace the cast with the safe assembly the rest of the crate uses:

```rust
let words = bytes
    .chunks_exact(8)
    .map(|c| u64::from_le_bytes(c.try_into().unwrap()));
```

`check_interpreter_addresses` currently takes `&[u64]`; give it an
`impl Iterator<Item = u64>` rather than collecting. That removes the intermediate entirely
and fixes the byte-order inconsistency in the same edit.

**B.** Cap the walk, and treat exceeding the cap as an error rather than a silent
truncation:

```
const MAX_INTERPRETERS: usize = 1024;
```

On overrun, `bail!` naming the address the chain was at. A visible error beats a hang, and
beats quietly reporting a prefix of the interpreters as though it were all of them —
consistent with the fail-closed posture of
[ADR 0006](../docs/adr/0006-layout-registration-integrity.md).

## 5. Seams and testing decisions

- **Seam (A):** `tests/live_smoke.rs` on the 3.8–3.12 legs — the CLI-level shape assertion.
  This is the highest seam in the codebase and it is the *only* one that proves the change:
  `check_runtime` decides whether a pre-3.13 runtime is found at all, so a mis-assembled
  word makes attach fail outright on exactly those legs. Nothing needs writing.
- **Seam (B):** none available, and none worth building. Driving the cap requires a
  synthetic interpreter chain, which means a reader seam under `PySession` — a new low-level
  seam for a defensive branch, against the grain of ADR 0005's preference for honest
  high-level signals. Verify by inspection plus the regression case below.
- **New seam needed:** no. This is the argument for keeping both items in one spec: A is
  covered by an existing gate and B is not worth a seam, so the change is landable without
  touching the test architecture.
- **What makes a good test here:** for A, the matrix asserts decoded **shape** on the
  affected legs — a wrong word assembly cannot produce a right shape. "Attach succeeded" is
  explicitly not enough (ADR 0005), and here it is not even the interesting property.
- **Prior art:** the pre-3.13 coverage in `tests/remote_debugging.rs`
  (`find_runtime_pre_3_13`), and the live-smoke shape assertions.
- **Cases:**
  1. Live matrix green on 3.8, 3.9, 3.10, 3.11, 3.12 across all three OSes — the real gate
     for A.
  2. `gc-stats --all` against a sub-interpreter fixture: unchanged output, prompt return —
     the regression guard for B.
  3. `cargo clippy` clean; the `chunks_exact` form should need no pointer-cast allowance.

## 6. Out of scope

The crate's other `unsafe`. The `read_struct` path — `std::ptr::read` over bytes cast
through a `#[repr(C)]` mirror — is the deliberate core mechanism of the offsets layer
([ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md)) and the reason the
generated `v_*.rs` structs exist at all. If it is ever revisited, `read_unaligned` is the
drop-in, but that is a separate change needing its own justification and its own matrix
run.

## 7. Further notes

Item B's cap value is arbitrary by design: 1024 interpreters is far beyond any real
embedding, so the constant is a runaway backstop, not a limit anyone should tune. If a
legitimate target ever trips it, that is a bug report worth having.
