# 0002 — Restore the `pyruntime` clause in module discovery

- **Status:** Not started
- **Kind:** bug — regression
- **Effort:** XS
- **Origin:** 2026-07-18 review (finding C11); introduced when module discovery was extracted
  into `memory::binary`.
- **Respects:** [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
  [ADR 0004](../docs/adr/0004-per-platform-image-layout.md)

## 1. Problem

An application that embeds CPython under a library named `pyruntime.dll` /
`libpyruntime.so` cannot be inspected at all. Every command — `gc-stats`, `find-runtime`,
`tui`, `monitor` — fails at the first step with *"No python-related modules found"*, even
though the interpreter is loaded in that process and perfectly readable.

## 2. Evidence

`memory::binary::find_python_modules` screens each mapped module path on a single
substring, `python`. The filter it replaced tested two:

```rust
// the pre-extraction filter
lower.contains("python") || lower.contains("pyruntime")
```

The `pyruntime` half was dropped when the function moved into `memory::binary`. That
function is now the single funnel for module discovery — `memory::process`,
`remote_debugging::version` and `offsets::read_offsets` all route through it — so the loss
propagates to every command uniformly rather than degrading one path.

The other `PyRuntime` matches in the crate are **section**-name checks (`.PyRuntime` on
ELF, `PyRuntime` on Mach-O, `PyRuntim` on PE — see ADR 0004). Those run only *after* a
module has been selected, so they cannot compensate for a module that was never returned.

## 3. Scope

**Affected:** every subcommand, for embedder processes whose interpreter library omits
`python` from its filename. Any Python version, any platform.

**Not affected:** a normal `python` / `python3.13` / `libpython3.13.so` process, matched by
the surviving clause. That is the entire live matrix, which is why no CI leg catches this:
[ADR 0005](../docs/adr/0005-testing-strategy.md)'s gate spawns real interpreters, and a
real interpreter always has `python` in its path.

**Related but out of reach:** `list_pids::list_python_processes` screens on the *process*
name. An embedder's process is named after the host application, so no substring rule
recovers it there; `list-pids` will keep missing embedders regardless of this change.

## 4. Proposed change

1. Restore the second clause in `find_python_modules`.
2. Lift the predicate out of the maps loop into `fn is_python_module(path: &str) -> bool`,
   so the rule is a named, testable thing rather than an `if` buried in an iteration that
   needs a live process to reach. This is what makes step 3 possible.
3. Add unit coverage for the predicate, so the next extraction cannot silently drop a
   clause the way this one did.

## 5. Seams and testing decisions

- **Seam:** the extracted `is_python_module` predicate, unit-tested in `memory::binary`.
  Higher seams exist (`PySession::attach`, the CLI) but none can observe this rule without
  a live embedder process, which CI cannot produce.
- **New seam needed:** yes, and it is the fix's own step 2 — a pure predicate over a path
  string. It sits at the highest point the rule can live: the rule *is* a function of the
  path, and nothing above it adds information.
- **What makes a good test here:** assert the accept/reject decision over realistic module
  paths from all three platforms. This is a string predicate, so a unit test is complete
  coverage of the decision — unusually for this codebase, nothing about it can be wrong in
  a way only a live process would reveal.
- **Prior art:** the path- and version-string unit tests in `remote_debugging::version`
  (`parse_exact_parses_the_shapes_detect_actually_sees`), which cover a parsing rule the
  same way.
- **Cases:**
  1. Accepts `libpython3.13.so`, `python.exe`, `Python` (framework path), `pyruntime.dll`,
     `libpyruntime.so`. The last two fail today.
  2. Rejects `libc.so.6`, `kernel32.dll`, `ntdll.dll`.
  3. Case-insensitivity is preserved (the filter lowercases first).
  4. Regression: the live matrix stays green. This widens the filter, so no currently
     discovered target may change behavior.

## 6. Out of scope

- Discovering embedders in `list-pids` (see §3) — a different filter, on a different input,
  with no substring rule available.
- A user-supplied `--module-name` / `--library` override. If arbitrary names need
  supporting, that is a feature with its own interface decisions, not part of restoring a
  regression.

## 7. Further notes

Worth a line in the README's limitations once this lands: gcscope finds an embedded
interpreter by *module name*, so an embedder that renames the library to something
containing neither `python` nor `pyruntime` is still invisible. That is a documented
boundary rather than a bug — but only once it is documented.
