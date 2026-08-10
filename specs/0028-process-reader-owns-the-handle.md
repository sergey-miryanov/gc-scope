# 0028 — Give the process handle an owner in `memory::reader`

- **Status:** Not started — ordered after [0024](0024-detect-policy-takes-its-reads.md),
  [0025](0025-interpreter-scan-takes-its-reads.md) and
  [0027](0027-process-tree-search-takes-its-reads.md)
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/memory/reader.rs`, 2026-08-09
- **Respects:** [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md),
  [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)

## 1. Problem statement

Nothing changes for an operator. The module's central rule survives as a naming convention and
a request in a doc comment, and the code enforces neither.

`memory::reader` exposes four functions in two pairs — `read_memory`/`read_memory_h` and
`read_u64`/`read_u64_h` — distinguished by a suffix, plus `open_handle` returning the upstream
`read_process_memory::ProcessHandle` raw. The `_h` half reads through a handle the caller
holds; the unsuffixed half opens and drops a fresh handle per call. The module's own doc
comment has to ask for the right one:

> Prefer this + the `*_h` readers over the per-call `read_memory`/`read_u64` on any hot path
> (interpreter walks, poll loops): those open and close a fresh handle on every single read.

Count the call sites and you see how well a request competes with a type: **ten one-shot calls
in `src/` against seven handle-holding ones.** The hot path is not among the ten, since
`PySession` holds a handle and reads through it per
[ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md), so nobody is paying for this in
performance. The cost is that the correct pattern lives in one consumer, spelled out by hand.

`PySession` already is that pattern: a `handle: ProcessHandle` field, `read_memory`, `read_u64`
and `read_i64` methods that delegate to the `_h` functions, and a doc line ("hands out cheap
reads through the single held handle") describing a type `memory::reader` does not have. Every
other path (attach-time symbol resolution, cookie validation, the pre-3.13 scan) has to either
reimplement it or take the per-call open, and all of them take the per-call open.

An owning type is also the only place one defect could ever be fixed. `read-process-memory`'s
`ProcessHandle` implements `Drop` on **Windows** (`Arc<ProcessHandleInner>` →
`CloseHandle`, correct) and **not on macOS**, where it is a bare `mach_port_name_t` from
`task_for_pid` that is never `mach_port_deallocate`d. Every `open_handle` on macOS leaks a
port send right. It is bounded today — `PySession::attach` and `soft_reattach` are the only
callers and neither is per-poll — so this spec does not fix it. It is the reason to want
somewhere a fix could go, and today there is nowhere.

## 2. Solution

The handle gets an owner in the module that owns reading:

```rust
// memory::reader
pub struct ProcessReader { /* handle + pid */ }

impl ProcessReader {
    pub fn attach(pid: u32) -> Result<Self>;
    pub fn read(&self, addr: u64, size: usize) -> Result<Vec<u8>>;
    pub fn read_u64(&self, addr: u64) -> Result<u64>;
}
```

`open_handle` and the four free functions go. Holding a handle stops being the documented
preference and becomes the only way to read, with the cost of *not* holding one visible at the
call site as an `attach` next to the read.

## 3. User stories

1. As a gcscope maintainer writing a new read path, I want the API to give me a reader rather
   than a choice between two spellings, so that the efficient one is not something I have to
   know to ask for.
2. As a gcscope maintainer, I want `PySession`'s "held handle" pattern to be a type instead of
   a field plus three delegating methods, so that the attach-time paths can have it too.
3. As a gcscope maintainer looking at the macOS port leak, I want one place that owns handle
   lifetime, so that a `Drop` is a change to one type rather than a change to a dependency.
4. As a maintainer reviewing this, I want it to be a mechanical migration with no call site
   opening more or fewer handles than before, so that the diff is checkable by eye.

## 4. Implementation decisions

**Migration only. Every call site opens exactly what it opened before.** A site that took the
per-call open becomes `ProcessReader::attach(pid)?.read(…)` — one handle, same as today, now
visibly so. A site that held a handle holds a `ProcessReader`. Hoisting any of them up a level
is not part of this change; see §7. This rule is what keeps the spec an S and the diff
reviewable, and it is the one that stops a rewrite hiding inside a move
([0021 §4](0021-version-grammar-module.md#4-implementation-decisions)).

**Call sites, all of them.** In `src/`, ten one-shot and seven handle-holding:

| Site | Today | After |
|---|---|---|
| `remote_debugging::session` (`attach`, `soft_reattach`, the three read methods, `layout_still_valid`) | `open_handle` + `*_h` | field becomes `ProcessReader`; `layout_still_valid` takes `&ProcessReader` |
| `check_interpreter` ×4 | per-call | absorbed by [0025](0025-interpreter-scan-takes-its-reads.md) into one adapter |
| `version::read_version_from_process` ×2 | per-call | absorbed by [0024](0024-detect-policy-takes-its-reads.md) into one adapter |
| `offsets::mod` ×2 (`read_offsets`, the version word) | per-call | `attach` at each |
| `process::validate_cookie` | per-call | `attach` at the call |
| `main.rs` (`read` subcommand) | per-call | `attach` at the call, one read and no more |
| `offsets::offset_table::read_gc_stats` | `handle: &ProcessHandle` parameter | `reader: &ProcessReader` |

**The seventh handle-holding site is in `remote_debugging::offsets`, and it is the only one
that is not `memory` or `session`.** `OffsetTable::read_gc_stats(&self, handle: &ProcessHandle,
iid: i64)` takes the raw upstream handle and is called from `PySession::gc_stats_global` and
`read_interpreter_stats`. It cannot be left alone: this spec deletes `open_handle`, so nothing
would produce the `ProcessHandle` it wants.

Its parameter becomes `&ProcessReader`. That puts an L1 type in an L2 public signature, which
is *with* [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)'s arrow rather than
against it — `offsets` already depends on `memory::reader`, and this narrows what it depends on
rather than widening it.

**Rejected — having `ProcessReader` hand out its inner `ProcessHandle`** so `read_gc_stats`
keeps its signature. It is the smaller diff and it defeats the spec: the upstream handle is
`Clone`, so an accessor returns exactly the escape hatch that makes non-`Clone` on the wrapper
meaningless, and it leaves [0029](0029-macos-task-port-lifetime.md) unable to reason about who
owns the port. The whole case for an owning type is that the handle stops being passed around
raw.

**Rejected — deleting `read_gc_stats` here.** It is the right end state, and it is not a
migration: the two callers currently smuggle a per-call address in by mutating
`gc_stats_addr` on a cloned `OffsetTable`, so removing the method means reworking both call
sites and deciding what a layout table is for. That is [0036](0036-offset-table-stops-reading.md),
which retires this row rather than competing with it.

**This is why the ordering is real, not a preference.** 0024 and 0025 turn six of the ten
one-shot sites into closures inside two adapters. Doing this spec first means editing six
sites those specs then rewrite; doing it after leaves four, one of which is a CLI subcommand
that reads once and exits. 0027 lands in the same file as `validate_cookie` and is smaller
than the conflict it would otherwise create.

**Keep `pid` in the struct, and keep the error messages verbatim.** The pid is worth having
for a future `revalidate` and for messages that could name it. Changing
`"Failed to read process memory"` to name the pid is an improvement and a separate one — the
live tests and CI checks that match on messages should move deliberately, not as a side effect.

**`ProcessReader` is not `Clone`, deliberately.** The upstream handle is `#[derive(Clone)]` on
every platform, which is exactly what makes a `Drop` unsafe to add naively: two clones
deallocating one mach port is worse than leaking it. Making the owner non-`Clone` is what
leaves the door open, and costs nothing — no call site clones a handle today.

**Rejected — adding `ProcessReader` and keeping the free functions.** An additive change that
leaves both spellings standing preserves the problem: two ways to read, one of them
preferred by a comment. The four functions and `open_handle` go in the same change.

**Rejected — a trait over the reads.** The consumers that want substitutable reads take
closures ([0024](0024-detect-policy-takes-its-reads.md),
[0025](0025-interpreter-scan-takes-its-reads.md),
[0027](0027-process-tree-search-takes-its-reads.md)), which is the crate's established shape
and needs no trait. A trait here would exist to be implemented once.

**Rejected — moving `PySession`'s read methods onto `ProcessReader` wholesale.** `read_i64`
and the stats-region logic are `remote_debugging` concerns sitting on top of raw reads
(ADR 0008's arrow); only the handle and the two primitive reads move down.

## 5. Seams and testing decisions

- **Seam:** `tests/memory.rs::find_runtime_locates_the_cookie`, whose tail already reads the
  `"xdebugpy"` cookie through all four entry points — `read_memory`, `read_u64`, then
  `open_handle` + `read_memory_h` — and is the only place asserting the spellings agree. Its
  tail becomes the assertion that `ProcessReader` agrees with itself across `read` and
  `read_u64` at one address. Note it is `--ignored` and gated on 3.13+, so it proves the new
  API on the legs that have a cookie to read and nowhere else.
- **New seam needed:** none. This is the argument for doing it as a migration: every path
  already has a test, and none of them is being asked to prove something new.
- **What makes a good test here:** the existing shape — read the `"xdebugpy"` cookie two ways
  and assert the *bytes*, not that reading succeeded. A reader that returns the wrong address's
  contents passes any "it read something" check, which is convention 6 in its original form.
- **Prior art:** `PySession`'s existing held-handle methods — the design being extracted, so
  the target shape is already in the tree and already reviewed.
- **Cases:**
  1. `tests/memory.rs` and `tests/probe.rs` compile against the new API. Both are separate
     crates seeing only the public surface, and `probe.rs` imports `open_handle` and
     `read_memory_h` by name in its header — so the compiler enumerates the public breakage
     for us, and there is no third consumer.
  2. The cookie reads the same through `read` and `read_u64` on a live child, as today.
  3. Live matrix unchanged across all three OSes — the only evidence that a real handle on a
     real platform still reads. macOS matters most: it is the platform whose handle is a mach
     port and the one where `attach` can fail for reasons the others do not have.
  4. `cargo clippy` clean, and no `#[allow]` added to cover a lifetime the struct now owns.

## 6. Out of scope

- **Implementing `Drop`.** Deallocating the mach port needs `unsafe`, a macOS-only `cfg`, and
  certainty about ownership that the upstream `Clone` currently denies. This spec makes it
  possible and does not do it; it wants its own change, a macOS leg, and a look at whether
  upstream should carry the fix instead.
- **Hoisting any handle up a level.** Reasoning in §4, follow-up in §7.
- **Naming the pid in read errors.** Reasoning in §4.
- **`PySession`'s caching, revalidation or `read_i64`.** Untouched;
  [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md) governs them and this changes
  none of it.
- **Anything about `proc_maps` or `remoteprocess` handles.** Different crates, different
  lifetimes, and neither is what `reader` owns.
- **Getting `OffsetTable` out of the reading business.** This spec migrates
  `read_gc_stats`' parameter; [0036](0036-offset-table-stops-reading.md) deletes the method and
  the `gc_stats_addr` mutation both its callers perform. Reasoning in §4.

## 7. Further notes

**The follow-up this enables.** Once a reader is a value, hoisting one is a local
change: `try_find_runtime` opens one and hands `&ProcessReader` to `validate_cookie` for every
module instead of one per module; the 0025 and 0024 adapters each hold one for their whole
scan. Each is a few lines *after* this lands and a cross-cutting edit before it. None is
urgent — all are attach-time — so they are worth doing when the surrounding code is being
touched anyway, not as a sweep.

**What this does not claim.** The per-call open is not costing anyone anything measurable
today: on Linux the handle is a pid, the hot path already holds one, and the ten one-shot
sites are all attach-time and bounded. The case for this change is that the crate's own
guidance about handles currently lives in a comment, and that the one real defect in the
area — the macOS port leak — has nowhere to be fixed.
