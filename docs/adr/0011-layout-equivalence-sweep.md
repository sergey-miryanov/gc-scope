# 0011 — Layout equivalence is swept, not assumed

**Status:** Accepted — implemented 2026-08-03. Amends
[ADR 0006](0006-layout-registration-integrity.md) (one premise, one consequence); extends
[ADR 0010](0010-pre-3-13-offsets-stay-hand-maintained.md) §2 to 3.13/3.14.

## Context

The registry held one module per version hex, and two unchecked assumptions decided whether a
build resolved: patch releases of a minor **share** a layout (so an unregistered final may
borrow its minor's), pre-releases **do not** (so one without a module is refused). Sweeping
every CPython tree we had shows both are wrong, in opposite directions.

**Patch releases are not reliably frozen.** CPython restructured `_gc_runtime_state` inside
the shipped 3.14 line, keeping the offsets we read only by hand-inserting placeholders —
`/* dummy members to preserve other offsets */`:

```
              sizeof(_gc_runtime_state)   generation_stats@
3.14.0 / .4              240                    120
3.14.5 / .6              264                    120
```

**Pre-releases sometimes are.** 3.15.0b1, b2 and b3 are byte-identical, so b2 was refused for
want of a module describing what b1's already described. The registry had also accumulated
duplicates (`v_3_13_1` ≡ `v_3_13_13_…`, `v_3_15_0b1` ≡ `v_3_15_0b3`).

Underneath sits a sharper gap: **inline builds (3.13, 3.14) have no runtime guard.** Their
`gc` sub-struct publishes only `size` and `collecting`, so `generation_stats`'s offset is
compiled in and unconfirmable; `verify_ring_stats_size` runs only on the ring path. The live
matrix does not close it — it tests whichever single patch `setup-python` offers, and catches
a wrong offset only if the garbage trips its `0..=1e12` bound. A ±8 shift reads
`threshold|count` (~1.3e10), passes green, reports wrong numbers.

## Decision

1. **A generation-time sweep** — `gen-offsets.py --sweep DIR` — groups trees by layout,
   comparing **generated Rust rather than C headers**: bindgen has already resolved that
   `_Py_DebugOffsets` changed header at 3.14 and neutralized no-op attributes (3.13 later
   added `_Py_NONSTRING` to `char cookie[8]`, which a text diff flags and which means
   nothing). Headers only — no build, no interpreter — so it checks *every* patch release.

2. **One module per distinct layout, plus an `ALIASES` table**, giving resolution a middle
   tier: exact hex → **verified alias** → same-minor fallback. An alias requires the block,
   the `gc_generation_stats` struct **and** the inline offset to match — block identity alone
   would alias a `+inc` build onto its clean release (`GC_CANDIDATES` handles that pair). It
   is proof, so it resolves as `Full` silently, where a fallback is an assumption and warns.
   **Tags only:** a branch tip's equivalence rots silently, and `main` matches 3.15.0b4 today.

3. **Each minor is anchored at its `.0`**, so fallback resolves *backwards* — never a build
   described by its own future, as the old anchors did by making 3.13.0 borrow 3.13.1's.

4. **`VERIFIED_GC_SIZES` covers the inline gap by membership, not equality.** Every 3.13+
   build publishes `gc.size`; the sweep records which values produce each layout's inline
   offset, and an unrecognized one **warns**. Equality would be wrong both ways — 3.14.5
   changed the size without moving the offset, and 3.13.x/3.14.0 share 240 with *different*
   offsets. Warning rather than refusing because `verify_ring_stats_size` fires on
   contradiction (proof of wrongness) while this fires on unfamiliarity, and 3.14.5 shows
   unfamiliar ≠ wrong; refusing would break gcscope on every new patch release.

5. **CI in two layers.** *Per-PR, no network:* unit tests assert the tables are coherent with
   the registry, plus a job rejecting a pinned patch version in the live-smoke matrix.
   *Weekly:* a separate `offsets-sweep` workflow exports `Include/` per release tag and
   sweeps, outside `rust.yml` so a tick doesn't drag 27 live legs with it.

## Consequences

- **9 modules → 6**, ~6,800 → ~4,700 lines; 3.15.0b2 gained support as one alias row rather
  than an 827-line module.
- **All 22 shipped 3.13.x/3.14.x releases now resolve by proof**, verified against
  `python/cpython`'s tags; they previously took the warning fallback. One row per release,
  never a range rule — that would re-encode the assumption 3.14.5 disproved.
- **Each new patch release needs an alias row**, or users on it fall back and see a warning.
  Self-correcting and harmless; the weekly job surfaces it.
- **`--trust-tags` is a caller assertion.** CI exports with `git archive`, leaving no `.git`,
  so tag-ness is asserted — sound only because the list comes from `git tag -l` filtered to
  finals. Pointed at a branch tip it would mint the stale alias §2 forbids.
- **Pre-release cycles stay out of the weekly sweep**: a new beta is caught by the floating
  3.15 live leg going red, and sweeping them would keep the job red on dropped alphas.
- Three latent generator bugs fixed on the way, all Windows-only and silent: regenerated
  modules were written in the locale codepage and were **not valid UTF-8**, an I/O round-trip
  rewrote every LF as CRLF, and ADR 0006 §5's nav-struct guard had never executed.
