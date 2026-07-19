# 0002 — Version-split runtime finding

**Status:** Accepted — implemented 2026-07-19. (Supersedes the old
`docs/pre-3-13-runtime-finding-plan.md`.)

## Context

CPython added the `"xdebugpy"` cookie — inside `_Py_DebugOffsets`, in a dedicated
`PyRuntime` section — in **3.13** (PEP 768). `find_runtime` was anchored
**entirely** on that cookie: locate the on-disk section, translate to a load
address, validate by reading `"xdebugpy"` from memory. Pre-3.13 has no cookie and
no such section, so `find_runtime` could never locate the runtime — the entire
`pre_3_13.rs` offset table was dead code, reachable only after a finding step that
always failed.

A structural heuristic, `check_interpreter_addresses`, does the cookie's job
without one: it confirms a candidate runtime by round-tripping
`candidate → *(candidate + threads_head) → tstate → *(tstate + interp)` and
checking it equals the stored `interpreters_head`. It needs a *candidate address*
to test.

## Decision

Split runtime finding by version, dispatched in `attach` after detecting the
version:

- **3.13+** → the `"xdebugpy"` cookie (`find_runtime_module`), unchanged.
- **pre-3.13 (3.8–3.12)** → resolve the **`_PyRuntime` symbol** from the binary's
  symbol table (generalized goblin resolution: `resolve_symbol_{elf,pe,macho}`),
  then validate the candidate with `check_interpreter_addresses`
  (`find_runtime_pre_3_13`). A blind data-segment scan is the documented fallback
  if a platform strips the symbol — **not built**.

The heuristic is an **internal finding tool only**. It was removed from every
user-facing surface — the `find-runtime --check` and `list-pids --verify` flags,
the `V` column, the TUI verify path — **and** from the 3.13+ same-minor
fallback-layout validation (`navigation_validates` deleted; the fallback now just
picks the nearest micro). `check_runtime`'s sole caller is the pre-3.13 finder;
`PySession::verify` was removed.

## Consequences

- `find-runtime` / `list-pids` / `diagram` / `tui` resolve pre-3.13 runtimes and
  versions. On Windows/PE `_PyRuntime` is exported across 3.8–3.12, so no scan
  fallback is needed there. **ELF/Mach-O symbol visibility is unverified** — Mach-O
  underscore-prefixes C symbols (`__PyRuntime`), PE export presence varies; confirm
  when those platforms are exercised.
- Known blind spot — **venv launcher shims**: a Windows redirector `python.exe`
  runs the real interpreter as a child, whose `_PyRuntime` lives in a separate
  address space, so a single-shot `attach` on the launcher PID fails. Target the
  child PID (`list-pids` surfaces it). Plan of record:
  `docs/venv-launcher-child-retarget.md`. Reconcile at the same time: the recursive
  `search_pid_and_children` returns `(addr, path)` but **drops the child PID**, so
  it can locate a runtime it can never read.
