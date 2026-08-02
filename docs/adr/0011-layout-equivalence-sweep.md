# 0011 — Layout equivalence is swept, not assumed

**Status:** Accepted — implemented 2026-08-03. Amends
[ADR 0006](0006-layout-registration-integrity.md) (one premise, one consequence) and extends
[ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md) §2 across both eras.

## Context

The 3.13+ registry held one module per registered *version hex*. Two assumptions decided
whether a build resolved, and neither was ever checked:

1. **Patch releases of a minor share a layout**, so an unregistered final may borrow its
   minor's layout (`resolve_fallback_layout`).
2. **Pre-releases do not**, so one without its own module is refused outright.

Comparing every CPython source tree we had shows both are wrong, in opposite directions.

**Patch releases are not reliably frozen.** CPython restructured `_gc_runtime_state` *inside*
the shipped 3.14 line:

```
                sizeof(_gc_runtime_state)   generation_stats@
  3.14.0 / .4              240                    120
  3.14.5                   264                    120
```

3.14.5 swapped `young`+`old[2]` (24+48=72 bytes) for `generations[3]` (3×24=72) and replaced
`work_to_do`/`visited_space`/`phase` with same-sized `dummy1/2/3` — CPython's own comment
reads `/* dummy members to preserve other offsets */`. The decode survived because they took
deliberate care, not because a rule prevented the change. Assumption 1 got the right answer
by luck here, and nothing would have said otherwise had it not.

**Pre-releases sometimes are frozen.** 3.15.0b1, b2 and b3 produce byte-identical blocks. b2
was being refused for want of a module describing exactly what b1's module already described.

The registry had also accumulated duplicates: `v_3_13_1` ≡ `v_3_13_13_…` and `v_3_15_0b1` ≡
`v_3_15_0b3`, byte-identical but for one version constant — ~1,365 lines encoding nothing new.

Underneath both sits a sharper gap. **Inline builds (3.13, 3.14) have no runtime guard at
all.** Their `gc` sub-struct publishes only `size` and `collecting`, so `generation_stats`'s
offset is computed at generation time and compiled in, and the target cannot confirm it.
`verify_ring_stats_size` — ADR 0006's fail-closed check — runs only on the ring path. The live
matrix does not close this either: it tests whichever single patch `setup-python` currently
offers, and detects a wrong offset only if the resulting garbage trips its `0..=1e12`
plausibility bound. A ±8 shift reads `threshold|count` (~1.3e10) or `collecting`, passes
green, and reports wrong numbers.

## Decision

1. **A generation-time sweep** — `gen-offsets.py --sweep DIR` — groups CPython trees by
   layout. It compares **generated Rust, not C headers**: bindgen has already resolved that
   `_Py_DebugOffsets` moved from `pycore_runtime.h` to `pycore_debug_offsets.h` at 3.14, and
   normalized away comments and no-op attributes (3.13 later added `_Py_NONSTRING` to
   `char cookie[8]`, which a text diff reports and which means nothing). It needs **only
   headers** — no build, no interpreter — so it can check every patch release of a minor,
   which the live matrix structurally cannot.

2. **One module per distinct layout, plus an `ALIASES` table.** Resolution gains a middle
   tier: exact hex → **verified alias** → same-minor fallback. An alias is minted only when
   the block, the `gc_generation_stats` struct **and** the computed inline offset all match.
   Block identity alone is deliberately insufficient — a clean release and a `+inc` build
   share a block and differ in the stats struct, and that pair must stay distinct
   (`GC_CANDIDATES` handles it). An alias is proof, so it resolves as `Full` and warns
   nothing; a fallback is an assumption, and still warns.

3. **Aliases only for immutable tags.** A branch tip's equivalence is true at one commit and
   rots silently — `main` matches 3.15.0b4 exactly today and will stop without any signal.
   Ongoing builds keep their own module, so ADR 0006's provenance pin still has somewhere to
   live.

4. **Each minor is anchored at its `.0`.** Fallback then always resolves *backwards* — 3.13.0
   describing 3.13.2 — never forwards. The previous anchors made a 3.13.0 target borrow
   3.13.1's layout: a build described by its own future, which nothing justifies.

5. **`VERIFIED_GC_SIZES` closes the inline gap, by membership and not equality.** Every 3.13+
   build publishes `gc.size` = `sizeof(_gc_runtime_state)`. The sweep records which values it
   has seen produce each layout's inline offset; at runtime an unrecognized value **warns**.
   Equality would be wrong in both directions: 3.14.5 changed the size without moving the
   offset (rejecting a build that decodes perfectly), while 3.13.x and 3.14.0 share size 240
   with *different* offsets (128 vs 120, so a match proves nothing).

6. **Warn, not refuse — because the evidence differs in kind.** `verify_ring_stats_size`
   fires on **contradiction**: the layout gcscope is about to use reconstructs a size the
   process directly denies. This fires on **unfamiliarity**, and 3.14.5 is the proof that
   unfamiliar ≠ wrong. Refusing would break gcscope on every new patch release over a fact it
   cannot establish. Once per PID, since the monitor loop calls `gc_stats` every tick and a
   warning at poll rate is one operators learn to ignore.

7. **CI in two layers.** *Per-PR, no network:* `cargo test` asserts the tables are coherent
   with the registry — every alias resolves, none chains, no hex both registered and aliased
   — plus a job rejecting a pinned patch version in the live-smoke matrix. *Weekly:* a
   separate `offsets-sweep` workflow exports `Include/` for every final release of every
   shipped minor and sweeps them, failing with the uncovered tag named. It lives outside
   `rust.yml` so a weekly tick doesn't drag 27 live-smoke legs with it, and derives which
   minors to sweep from `LAYOUTS` rather than a hardcoded list.

## Consequences

- **9 modules → 6**, ~6,800 → ~4,800 lines. 3.15.0b2 gained support as a one-line alias
  rather than an 827-line module; `v_3_13_13_…`, `v_3_15_0b3` (duplicates) and `v_3_15_0a8`
  (superseded, unsupported by choice) are gone.
- **The unpinned live matrix is now load-bearing in both eras and enforced.** ADR 0010 §2
  made it a constraint for pre-3.13; it matters identically for 3.13/3.14, and `matrix-unpinned`
  now fails the build rather than trusting a comment.
- **The warning is only as good as the sweep is fresh.** The verified sets start from the
  trees swept locally; every other patch release warns until the weekly job populates them.
  If that job is disabled or left red, the warning becomes noise on ordinary builds and is
  worse than nothing — freshness is the mechanism, not a nicety.
- **`--trust-tags` is a caller assertion.** CI exports releases with `git archive`, leaving no
  `.git` to interrogate, and asserts tag-ness instead. Sound only because the tag list comes
  from `git tag -l` filtered to finals; pointed at a directory that may hold a branch tip it
  would mint exactly the rotting alias decision 3 forbids.
- **Pre-release cycles stay out of the weekly sweep.** A new beta is caught by the floating
  3.15 live-smoke leg going red (ADR 0006's arrangement, unchanged); sweeping them would keep
  the job permanently failing on superseded alphas we deliberately dropped.
- Three latent generator bugs fixed on the way, all Windows-only and all silent: regenerated
  modules were written in the locale codepage and were **not valid UTF-8** (so they would not
  compile), a `read_text`/`write_text` round-trip rewrote every LF as CRLF, and ADR 0006 §5's
  same-hex nav-struct guard had never executed.
