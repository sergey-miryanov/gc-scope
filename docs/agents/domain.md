# Domain docs

How the engineering skills should consume this repo's documentation when exploring the
codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root. It does not exist yet, which is fine; see below.
- **`docs/adr/`**: decisions already taken, indexed by `docs/adr/README.md`. Read the ADRs
  covering the area you are about to work in.
- **`specs/`**: work that is specified but not built, indexed by `specs/README.md`. An ADR
  tells you why the code is the way it is; a spec tells you what is already known to be
  wrong with it. Check both before proposing a change.
- **`docs/version-support.md`**: the CPython-side forces behind most of the design.
- **`docs/testing-policy.md`**: which kind of test a given change calls for.

If a file does not exist, **proceed silently**. Do not flag its absence or propose creating
it upfront. `/domain-modeling`, reached through `/grill-with-docs` and
`/improve-codebase-architecture`, creates these lazily, when terms or decisions actually get
resolved.

## Layout

Single-context. One crate, one context:

```
/
├── CONTEXT.md          ← not yet written
├── docs/adr/           ← decisions taken
├── specs/              ← specified, not built
└── src/
```

A root `CONTEXT-MAP.md` would mark this as multi-context. There is none, and the `fuzz/`
crate is a test harness rather than a second context.

## Use the glossary's vocabulary

When your output names a domain concept (an issue title, a refactor proposal, a hypothesis,
a test name), use the term as `CONTEXT.md` defines it once that file exists. Until then
`CLAUDE.md` and `docs/version-support.md` carry the working vocabulary, and it is precise on
distinctions that matter: an **entry** is one per-generation ring position, never a "slot",
which is reserved for CPython's own `__slots__`; a **layout** is not a version, since several
releases share one. Do not drift to synonyms.

If the concept you need has no name yet, treat that as a signal. Either you are inventing
language the project does not use (reconsider), or there is a real gap (note it for
`/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it rather than overriding it in silence:

> Contradicts ADR 0006 (layout registration integrity), but worth reopening because...
