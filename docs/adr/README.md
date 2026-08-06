# Architecture Decision Records

Each ADR captures one durable decision — the forces that drove it, what was
decided, and the consequences — not a step-by-step implementation plan. When a
later decision changes an earlier one, the earlier ADR gets a short note rather
than being rewritten.

Forward-looking work that hasn't been decided/built yet lives in
[`specs/`](../../specs/README.md), not here — one file per open item, deleted when it
lands.

[`docs/version-support.md`](../version-support.md) covers the other half of the
story: what varies across CPython 3.8–3.16 and why each difference makes attaching
hard — the forces, stated without deciding anything. The ADRs below record what
gcscope decided in response to them. The **Force** column links back to the section
that poses the problem each decision answers.

| ADR | Decision | Force |
|-----|----------|-------|
| [0001](0001-pysession-resolve-once-facade.md) | `PySession`: resolve a process once, expose it through a tiered `Resolved` enum | — |
| [0002](0002-version-split-runtime-finding.md) | Runtime finding splits by version: `xdebugpy` cookie (3.13+) vs `_PyRuntime` symbol + navigation heuristic (pre-3.13) | [2](../version-support.md#2-knowing-which-version-you-are-looking-at), [3](../version-support.md#3-locating-the-runtime) |
| [0003](0003-layout-driven-gc-stats-decode.md) | GC-stats decode is keyed by layout *kind*, letting 3.8–3.12 reuse the inline path | [5](../version-support.md#5-reaching-the-gc-statistics), [6](../version-support.md#6-decoding-the-entries) |
| [0004](0004-per-platform-image-layout.md) | Per-platform image layout (section names, fat binaries, symbol decoration, image base) is discovered and CI-verified, not assumed | [1](../version-support.md#1-finding-the-interpreter-in-the-process), [7](../version-support.md#7-platform-image-facts-cannot-be-inferred) |
| [0005](0005-testing-strategy.md) | Testing is layered — unit tests for pure logic, a live `(OS × version)` matrix asserting decoded *shape* as the correctness gate, and `#[ignore]`d in-process lifecycle tests | [8](../version-support.md#8-what-the-forces-cost-together) |
| [0006](0006-layout-registration-integrity.md) | Layout registration/resolution fails closed — exact-or-refuse fallback, ring-size mismatch guard, provenance-pinned ongoing builds, and gen-offsets guards | [4](../version-support.md#4-deciding-which-layout-describes-the-build) |
| [0007](0007-gcstat-layout-driven-view.md) | `GcStat` is a lean layout-driven view over raw entry bytes (not a fixed field superset), giving one decode primitive shared by the exporter and TUI panel | [6](../version-support.md#6-decoding-the-entries) |
| [0008](0008-reader-consumer-package-layering.md) | Package layering `memory → remote_debugging → {snapshot, monitor} → cli`: one reader source of truth, two parallel consumer shapes, CLI-free subsystems | — |
| [0009](0009-performance-guarded-by-shape.md) | No benchmarks yet — a wall-clock monitor-loop benchmark measures syscalls not our code; guard cost as complexity/op-count invariants instead, with a defined trigger to revisit | — |
| [0010](0010-pre-3-13-offsets-stay-hand-maintained.md) | Pre-3.13 offsets stay hand-transcribed — generation would relocate the transcription, not make the era self-describing; the closed set is guarded by the unpinned live matrix | [4](../version-support.md#4-deciding-which-layout-describes-the-build) |
| [0011](0011-layout-equivalence-sweep.md) | Layout equivalence is swept from source, not assumed — one module per distinct layout plus a proven alias table, and a `gc.size` membership warning for inline builds | [4](../version-support.md#4-deciding-which-layout-describes-the-build) |
| [0012](0012-version-detection-fails-closed.md) | Version detection fails closed — one strict grammar, an unrepresentable version refused rather than clamped, and a scanner that only ever tightens | [2](../version-support.md#2-knowing-which-version-you-are-looking-at) |
| [0013](0013-probe-offsets-are-compiled-in.md) | Probe offsets are compiled in, not registered — a Probe is built against the interpreter it runs in, so the registry's problem does not exist there; runtime guards and the sweep carry the residual patch-drift risk | [4](../version-support.md#4-deciding-which-layout-describes-the-build), [5](../version-support.md#5-reaching-the-gc-statistics) |
| [0014](0014-probe-regions-discovered-by-module-export.md) | Probe regions are discovered through the module export table — nothing in CPython points at one; validation fails closed and a declared layout digest catches within-entry field moves that sizes cannot | [1](../version-support.md#1-finding-the-interpreter-in-the-process), [5](../version-support.md#5-reaching-the-gc-statistics), [7](../version-support.md#7-platform-image-facts-cannot-be-inferred) |
| [0015](0015-probe-counters-are-seeded.md) | Probe counters are seeded from CPython's own, so they stay Lifetime totals; `duration` and `candidates` cannot be and are declared Install-relative | [5](../version-support.md#5-reaching-the-gc-statistics), [6](../version-support.md#6-decoding-the-entries) |
| [0016](0016-probe-ships-from-this-repo.md) | The Probe ships from this repo on its own release train — one tree for the shared layout contract, two release trains, path-filtered CI plus one always-on contract leg | — |
