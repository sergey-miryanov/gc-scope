# Architecture Decision Records

Each ADR captures one durable decision — the forces that drove it, what was
decided, and the consequences — not a step-by-step implementation plan. When a
later decision changes an earlier one, the earlier ADR gets a short note rather
than being rewritten.

Forward-looking work that hasn't been decided/built yet lives in a `*-plan.md`
next to this folder, not here (e.g. `docs/venv-launcher-child-retarget.md`).

| ADR | Decision |
|-----|----------|
| [0001](0001-pysession-resolve-once-facade.md) | `PySession`: resolve a process once, expose it through a tiered `Resolved` enum |
| [0002](0002-version-split-runtime-finding.md) | Runtime finding splits by version: `xdebugpy` cookie (3.13+) vs `_PyRuntime` symbol + navigation heuristic (pre-3.13) |
| [0003](0003-layout-driven-gc-stats-decode.md) | GC-stats decode is keyed by layout *kind*, letting 3.8–3.12 reuse the inline path |
