# Specs — open work

One file per unit of *forward-looking* work: something identified, understood and
specified, but not yet built. A spec states the problem, the evidence for it, the
proposed change, and the seam it will be tested through — it does not record a decision.

Complements the two backward-looking docs:
[`docs/adr/`](../docs/adr/README.md) (decisions already taken) and
[`docs/version-support.md`](../docs/version-support.md) (the CPython-side forces).

## Open specs

| Spec | Kind | Effort | Summary |
|------|------|--------|---------|
| [0001](0001-pid-dialog-small-terminal.md) | Bug — **crash** | S | `render_dialog` underflows its centering subtraction on terminals shorter than 12 rows |
| [0002](0002-pyruntime-module-discovery.md) | Bug — regression | XS | Module discovery lost the `pyruntime` name clause — embedders shipping `libpyruntime.so` are undiscoverable |
| [0003](0003-remote-walk-and-read-hardening.md) | Bug — safety | S | Unaligned `&[u64]` view over a `Vec<u8>`, and an unbounded interpreter walk |
| [0004](0004-free-threaded-validation-reporting.md) | Bug — reporting | S | `read-runtime` scores `free_threaded = 1` as a *failed* check on a build gcscope fully supports |
| [0005](0005-tui-ring-geometry-from-layout.md) | Bug — correctness | S | Last hardcoded ring-geometry copy: the TUI tree's entry subtree is wrong on free-threaded targets |
| [0006](0006-version-parser-unification.md) | Bug — drift | S | Two version grammars; the byte scanner lacks the serial clamp its twin applies |
| [0007](0007-tui-static-content-caching.md) | Feature — efficiency | M | Per-frame rebuild of immutable TUI content (offsets tree, hex dump, color maps) |
| [0008](0008-shared-formatters-and-pid-table.md) | Feature — cleanup | M | Two hex formatters; PID-row assembly duplicated across the CLI table and the TUI picker |
| [0009](0009-venv-launcher-child-retarget.md) | Feature — ergonomics | M | Windows venv shim PIDs fail single-shot commands; `attach` should re-target to the child |
| [0010](0010-tree-last-child-connector.md) | Bug — cosmetic | S | `tree_prefixes` never emits the last-child connector its doc comment promises |

**Suggested order:** 0001 (the only crash) → 0002 (one line, unblocks embedders) → 0003 →
0004 (smallest user-visible wrongness) → then the cosmetic and efficiency tail in any
order.

## Templates

- [TEMPLATE-bugfix.md](TEMPLATE-bugfix.md) — something is broken.
- [TEMPLATE-feature.md](TEMPLATE-feature.md) — enhancements, ergonomics, cleanups: the
  change is *wanted* rather than *broken*. Adds a user-perspective solution statement and
  user stories.

Pick by whether the change fixes something or adds something, not by size. Both templates
carry the same §5 seams-and-testing section, because that is the part that decides whether
the work is actually finishable.

## Conventions

Distilled from the `to-spec` skill and adapted for a repo-local folder rather than an issue
tracker.

**1. Anchor on symbols, never line numbers.** Cite `memory::binary::find_python_modules`,
not `binary.rs:40`. Line numbers rot within one refactor; the predecessor of this folder
became unusable that way, and re-verifying its 33 findings by hand cost more than writing
them did. Quote code only where the defect or decision **is** the code, trimmed to the
decision-rich part and labelled with the symbol it lives in.

**2. Sketch the seam before the solution.** Every spec says how it will be tested, at what
level, before anyone starts. Prefer an existing seam; use the highest one that can observe
the change; keep the total number of seams in the codebase as low as possible. A new seam
is proposed at the highest point it can live, and an honest public signal beats a
`test-hooks` gate — see [ADR 0005](../docs/adr/0005-testing-strategy.md), which chose
`PySession::layout_source()` over a hook for exactly this reason.

**3. State the problem from the operator's perspective.** What someone running gcscope
sees, before any mention of the faulty expression. Feature specs go further and carry user
stories.

**4. Use the project's vocabulary, and respect the ADRs.** One ring position is an
**entry**, never a "slot" (that word is reserved for CPython's `__slots__`/type slots).
Resolution tiers are **Full / LayoutOnly / Legacy**; layout resolution is **exact / verified
alias / fallback**. Link the ADRs a spec must not contradict in its header, and if
implementing it overturns one, amend the ADR rather than the code alone.

**5. Say what is out of scope.** Explicitly, with reasons. This is what keeps a spec
landable.

**6. Assert shape, not success.** gcscope's characteristic bug is a wrong struct offset,
which executes the same lines as a right one and emits a full table of plausible garbage. A
test that checks "we read something" proves nothing about a decode path.

### Lifecycle

Delete a spec when it lands — this folder is the open set, not a history. Git keeps the
record. If implementing it settled a durable design question, that question graduates to an
[ADR](../docs/adr/README.md); if it merely fixed something, it graduates to nothing and the
file just goes.

A spec whose current behavior is locked by a characterization test is marked **Pinned** in
its status line, and names the test — fixing it means updating that test in the same change,
deliberately.

## Provenance

These supersede `.scratchpad/`, removed on 2026-08-03. That folder held two review
artifacts (`review.md` of 2026-07-17 and `fix-plan.md` of 2026-07-18, the latter subsuming
the former) plus three deferred plans. Of the 33 findings in `fix-plan.md`, 25 were
verified landed — including eight the plan's own status lines still showed as open, which
two large refactors resolved since:

| Finding | How it was resolved |
|---|---|
| C3 — 3.15.0a7 half-wired | Resolved by removal; the registry is now `v_3_13_0` / `v_3_14_0` / `v_3_15_0b1` (+`_gcinc`) / `v_3_15_0b4` / `v_3_16_0a0` with `ALIASES` + `GC_CANDIDATES` ([ADR 0011](../docs/adr/0011-layout-equivalence-sweep.md)) |
| C5 — version scan aborted on the first bad candidate | `scan_for_version_string` advances past every failed candidate |
| C8 — dead `find-runtime --check` fallback | Deleted; the subcommand takes only a PID |
| E3 — PID picker hardcoded `verify=true` | `list_python_processes` no longer takes a verify flag |
| R5 — formatting toolkit copy-pasted | Consolidated into `tui::format` |
| R6 — stat-aggregation twins | Share the extracted `entries_by_generation` |
| Cross-cutting — live test harness | `tests/live_smoke.rs` + the 3 OS × 3.8–3.15t CI matrix ([ADR 0005](../docs/adr/0005-testing-strategy.md)) |

The remaining eight became specs 0001–0003 and 0005–0008; the three deferred plans became
0004, 0009 and 0010. Nothing else was carried forward: the rest was either landed work or
line numbers two refactors stale.
