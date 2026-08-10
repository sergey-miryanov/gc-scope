# 0029 — Deallocate the macOS task port gcscope opens

- **Status:** Not started — blocked on
  [0028](0028-process-reader-owns-the-handle.md)
- **Kind:** bug — resource leak
- **Effort:** M
- **Origin:** review of `src/memory/reader.rs`, 2026-08-09
- **Respects:** [ADR 0004](../docs/adr/0004-per-platform-image-layout.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md)

## 1. Problem

On macOS, every process handle gcscope opens leaks a mach port send right. Nothing releases
it, and nothing can: the right is held inside a type that never gives it back.

An operator sees nothing at first. A long `gcscope monitor` against a process tree that
contains anything gcscope can open but cannot attach to grows gcscope's own port name space
for as long as it runs.

## 2. Evidence

`read-process-memory` 0.1.6 defines the macOS handle as a bare port name with **no `Drop`**:

```rust
/// On macOS a `ProcessHandle` is a mach port.
#[derive(Clone)]
pub struct ProcessHandle(mach_port_name_t);

impl TryFrom<Pid> for ProcessHandle {
    fn try_from(pid: Pid) -> io::Result<Self> { Ok(Self(task_for_pid(pid)?)) }
}
```

`task_for_pid` returns a send right to the target's task port. The only `impl Drop` in the
crate is Windows' (`Arc<ProcessHandleInner>` → `CloseHandle`, correct); Linux and FreeBSD hold
a `Pid` and have nothing to release. macOS holds a right and releases nothing.

**Where it accumulates.** `PySession::attach` opens the handle on its *first* line, before
`find_runtime_versioned` can decide the process is not a Python one — so a failed attach still
costs a port right. `MonitorContext::poll` deliberately does not cache a failed attach ("A
failed attach is an invalid process for this tick and is deliberately not cached"), so the
attempt repeats. The leak therefore scales with *attach attempts* rather than with distinct
processes, which is what makes it unbounded. `soft_reattach` adds to it: it opens a fresh
handle and overwrites `self.handle`, dropping the old right on the floor.

How many attempts a given tree produces depends on the discovery and `WaitPolicy` interaction
in `run_loop`, which this spec does not need to settle: the fix is the same either way, and
the mechanism above is enough to say the count is not bounded by the number of processes.

**Why it has never been noticed.** Every other subcommand attaches once and exits, at which
point the kernel reclaims the whole name space. Only `monitor` and `run` live long enough,
only on macOS, and the symptom is growth in a resource nothing in the test suite looks at.

## 3. Scope

**Affected:** macOS only, and only the port name space of gcscope's own process — no target
process is harmed, and no read returns wrong data. This is a resource leak, not a correctness
bug, which is why it sits behind [0028](0028-process-reader-owns-the-handle.md) rather than in
front of it.

**Not affected:** Windows (upstream `Drop` is correct), Linux and FreeBSD (the handle is a
pid).

**Why CI misses it:** nothing counts ports. The macOS legs attach, read, assert a shape and
exit, which is exactly the lifetime in which the leak is invisible.

## 4. Proposed change

**The port must be obtained by gcscope, because it cannot be recovered from the handle.**
Upstream's macOS `ProcessHandle` has no accessor, no `Deref` (that impl is Windows-only) and
no `Into` — the port name goes in through `impl TryFrom<mach_port_name_t> for ProcessHandle`
and never comes out. That impl is the only door, and it is the one the fix walks through:

- on macOS, `ProcessReader::attach` calls `task_for_pid` itself, keeps the port name, and
  builds the handle with `ProcessHandle::try_from(port)`;
- on every other platform it keeps `ProcessHandle::try_from(pid)` unchanged;
- a macOS-only `Drop` on `ProcessReader` deallocates the right.

**The guard that makes `Drop` safe, and the reason this is not a two-line change.** Upstream's
`task_for_pid` special-cases the caller's own pid:

```rust
if pid == unsafe { libc::getpid() } as Pid {
    return Ok(unsafe { mach::traps::mach_task_self() });
}
```

`mach_task_self()` is **not** a right gcscope acquired and must never be deallocated —
decrementing the task's own port user reference is a foot-gun that ends in an invalidated
task port, not a leak. And gcscope drives that path routinely and on purpose: `resolve_pid`
in `src/main.rs` maps a PID of `-1` to `std::process::id()` for roughly eight subcommands,
and `CLAUDE.md` documents `-1` as the way to target gcscope itself. So `Drop` deallocates
only when the port is not `mach_task_self()`.

**The handle must not escape the reader.** [0028](0028-process-reader-owns-the-handle.md)
already makes `ProcessReader` non-`Clone` for this reason; this spec is what turns that from
tidiness into a requirement, because upstream's `ProcessHandle` *is* `Clone` and a clone
outliving the deallocate would read through a dead port.

**Dependency.** A macOS-only direct dependency on `mach2`, which is already in the tree
transitively (0.4.3 and 0.6.0 both appear in `Cargo.lock`, alongside `mach` 0.3.2 pulled in by
`read-process-memory`). Pin the version already present rather than adding a fourth. No
non-macOS target gains a dependency.

## 5. Seams and testing decisions

- **Seam:** a `#[cfg(all(test, target_os = "macos"))]` module in `memory::reader`. The leak is
  a property of one type's lifetime, observable only on one platform, and there is no higher
  seam — every consumer above it would pass with the port leaking, which is how it got here.
- **New seam needed:** none in production code. The test needs `mach_port_names` to count
  gcscope's own name space, which is the same dependency the fix adds.
- **What makes a good test here:** a **count**, not a success. "Attach still works after N
  attaches" passes while leaking, since the name space is large, which is the failure mode
  itself. Count the ports, do N attach-and-drop cycles, count again, assert the
  difference is zero (not "small" — a leak of one per attach is exactly what this fixes).
- **Prior art:** `binary::parse_macho_refuses_a_fat_header_claiming_impossible_arch_count`,
  whose real assertion is a resource property (that it returns at all) rather than a value.
  Same genre: the interesting outcome is not in the return type.
- **Cases:**
  1. **The leak, on another process:** port count is unchanged across N attach-and-drop cycles
     against a live child. Fails today by exactly N.
  2. **The `mach_task_self` guard:** N attach-and-drop cycles against gcscope's *own* pid,
     then a read through a fresh reader still succeeds. A `Drop` missing the guard breaks this
     — and breaks it loudly, which is the point of running it after the cycles rather than
     during.
  3. **`-1` end to end:** `gcscope gc-stats -1` (and one more self-targeting subcommand) still
     works on the macOS legs. `resolve_pid` is the production path into case 2, and this is
     the honest public signal ADR 0005 prefers over trusting the unit test alone.
  4. **`soft_reattach` replaces rather than leaks:** the port count is unchanged across a
     revalidate that re-opens the handle.
  5. Live matrix green on all three OSes. Linux and Windows must be *untouched* — the `Drop`
     is `cfg`-gated, and the non-macOS `attach` path keeps `ProcessHandle::try_from(pid)`
     verbatim.
  6. `cargo clippy` clean on macOS, including the new `unsafe` blocks.

**A platform fact, verified rather than inferred** ([ADR 0004](../docs/adr/0004-per-platform-image-layout.md)).
Every claim above about `task_for_pid`, the missing `Drop` and the absent accessor comes from
reading `read-process-memory` 0.1.6's source, and every one of them is invisible from Windows
or Linux. None of this is landable without a macOS leg that runs cases 1–3.

## 6. Out of scope

- **Fixing it upstream.** A `Drop` in `read-process-memory` is the right long-term home and
  worth a PR, but it is someone else's release cadence, it must solve the same
  `mach_task_self` problem, and it does not help this repo until it ships. Do it in parallel
  if at all; do not make this spec wait on it.
- **Vendoring or forking `read-process-memory`.** Disproportionate for one `Drop`, and the
  `TryFrom<mach_port_name_t>` door means it is not necessary.
- **Reducing the number of attaches.** Not opening a handle until `find_runtime_versioned`
  succeeds would cut the failed-attach case, and caching a failed attach would cut the repeat.
  Both are real improvements to `PySession::attach` and `MonitorContext::poll`; both are
  behavior changes to the attach ladder and the `WaitPolicy` contract, and neither is a
  substitute for releasing what is opened. Separate specs if wanted.
- **Any other mach resource.** `proc_maps` and `remoteprocess` acquire their own handles with
  their own lifetimes; neither is what `memory::reader` owns, and neither is evidence about
  the other.
- **Auditing the leak's size in a real run.** The fix does not depend on the number.

## 7. Further notes

Ordered strictly after [0028](0028-process-reader-owns-the-handle.md), and this is the spec
that justifies it: 0028's case for an owning type was that guidance about handles lived in a
comment, which is a tidiness argument. This is the defect that needs somewhere to be fixed,
and it cannot be fixed while five call sites each hold a raw upstream handle with no owner and
no `Drop` to hang off.

Write case 2 first, before the `Drop` exists. It fails in the most instructive direction: a
`Drop` missing the guard passes case 1 and then takes out the interpreter. Nowhere else can
the reason for those five lines be written down where a "simplification" has to confront it.
