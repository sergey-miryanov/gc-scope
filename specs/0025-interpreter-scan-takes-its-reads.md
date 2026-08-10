# 0025 — Split the pre-3.13 runtime check into a policy and its reads

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/remote_debugging/check_interpreter.rs`, 2026-08-09
- **Respects:** [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)

## 1. Problem statement

An operator on Python 3.8–3.12 either attaches or is told `Failed to find _PyRuntime`. There
is nothing in between, because `check_interpreter::check_runtime` is the *only* thing
standing between a resolved `_PyRuntime` symbol and gcscope believing it — 3.13+ finds the
runtime by the `"xdebugpy"` cookie and never enters this module
([ADR 0002](../docs/adr/0002-version-split-runtime-finding.md), and the closing comment in
`check_interpreter.rs` says so).

`check_interpreter_addresses` is where that decision gets made, and it is a policy
about how to react to bad remote data:

- candidates are tried **in word order, first match wins**;
- a candidate outside mapped memory is skipped;
- a `threads_head` read that fails is skipped, not fatal;
- a NULL or unmapped `threads_head` is skipped;
- an `interp` back-pointer read that fails is skipped, not fatal;
- a back-pointer that disagrees with the candidate is skipped — the scan continues;
- nothing self-consistent found ⇒ error, and the caller turns that into "not this module".

`check_interpreter.rs` has **no `#[cfg(test)]` module**. Not one of those rules has a test.
The only coverage is `tests/remote_debugging.rs::find_runtime_pre_3_13_locates_the_runtime`,
which is `--ignored`, needs a pre-3.13 interpreter on the machine, and skips itself with a
`SKIP` line when `GCSCOPE_TEST_PYTHON` does not point at one. On most developer machines and
most CI legs, nothing covers this module.

As with [0024](0024-detect-policy-takes-its-reads.md), most of the list cannot be staged by a
live interpreter. A healthy CPython does not offer a mapped-but-unreadable candidate, a NULL
`threads_head` on the interpreter the runtime points at, or a word that survives every check
except the back-pointer. Those arms are there for a torn or hostile target, which the live
matrix cannot produce on demand.

`check_runtime` has the same shape and one rule of its own that matters more than it looks:
between its two reads and the scan it **assembles the block into words**, and how it does
that is untested and, per
[0003 §2](0003-remote-walk-and-read-hardening.md#a--unaligned-u64-view), wrong. The pointer
cast reads in *host* byte order while every other read in the crate goes through
`from_le_bytes`, and it drops up to seven trailing bytes without saying so. Both stay
invisible because every platform gcscope ships to is little-endian, and no test could see
either even on one that is not: the assembly sits between two `reader::` calls, reachable only
with a pid.

The reason none of it is testable is mechanical: both functions take a `u32` pid, call
`get_process_maps` and `reader::read_u64` themselves, and thread a `&[MapRange]` through a
shared helper. `proc_maps::MapRange` keeps its address fields private on every platform
(`range_start`/`range_end` on Linux, `base_addr`/`base_size` on Windows, `start`/`size` on
macOS), has no public constructor and a different shape per platform — so even the trivially
pure `maps_contains` cannot be handed a fixture.

That helper is also in the wrong module. `check_interpreter.rs` imports `proc_maps` directly
and calls `get_process_maps` itself, while `memory::regions` — one layer down, and the module
that owns `MapRange` — already exists to enumerate a process's mappings. So the L1 module is
bypassed and a region predicate lives in L2, against
[ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md)'s arrow. It is the same shape
as [0022](0022-image-lookup-into-memory-binary.md)'s finding, mirrored: there a reader reached
up to a consumer, here a consumer reaches past the reader to its dependency.

## 2. Solution

Both public functions in the module split the same way. The scan and the match become free
functions that take their two questions about the target — *is this address mapped* and *what
is the word at this address* — as arguments, and `check_interpreter_addresses` and
`check_runtime` become the adapters that answer them for a real pid. Behavior is unchanged;
what changes is that every rule above, plus the byte-order and short-block rules buried in
`check_runtime`'s word assembly, becomes a unit test that runs in `cargo test` in
milliseconds with no Python installed.

## 3. User stories

1. As a gcscope maintainer touching the pre-3.13 finding path, I want "first self-consistent
   candidate wins, in word order" pinned by a test, so that a reordering or an early return
   fails locally rather than on whichever CI leg happens to have a 3.8 interpreter.
2. As a gcscope maintainer, I want the skip-don't-abort rules covered, so that a change that
   turns one bad read into a fatal error is caught — today it would silently reduce gcscope
   to "attach works unless the first mapped word is junk".
3. As an operator attaching to a busy or mid-teardown 3.8–3.12 process, I want a candidate
   that fails one read to cost me that candidate and not the attach. This is the current
   behavior; the spec is what makes it stay true.
4. As a gcscope maintainer, I want `check_interpreter_addresses` and `check_runtime` short
   enough that their bodies are visibly *the IO*, so that "what does this touch" is answered
   by reading them.
5. As a gcscope maintainer, I want "is this address mapped" to live in `memory::regions` with
   the rest of the region logic, so that `remote_debugging` stops naming `proc_maps` and the
   next caller that needs the predicate finds it instead of writing a third one.
6. As a gcscope maintainer, I want the block-to-word assembly asserted little-endian by a
   test, so that the fix [0003](0003-remote-walk-and-read-hardening.md) makes to it has a
   guard that does not depend on someone one day porting gcscope to a big-endian target.

## 4. Implementation decisions

Mirror `session::walk_interpreter_chain` — the same situation, the same answer, already in
this crate: a policy about how to react to bad reads, hoisted into a free function taking its
reads as closures, because the reads need a target and the answers to a bad read do not.

```rust
// remote_debugging::check_interpreter — private, unit-tested in-file
fn find_self_consistent_interpreter(
    words: impl IntoIterator<Item = u64>,
    threads_head_offset: u64,
    thread_interp_offset: u64,
    is_mapped: impl Fn(u64) -> bool,
    read_u64: impl Fn(u64) -> Option<u64>,
) -> Option<u64>
```

`check_interpreter_addresses` keeps its signature and becomes the adapter: the ranges, the
closures, the call, and the `bail!` on `None`.

**`is_mapped` crosses the seam as a closure rather than `&[MapRange]`.** The constructor
problem in §1 forces it.

**The ranges and the predicate come from `memory::regions`, and `maps_contains` goes.**

```rust
// memory::regions
pub fn mapped_ranges(pid: u32) -> Result<Vec<(u64, u64)>>;
pub fn contains(ranges: &[(u64, u64)], addr: u64) -> bool;
```

The adapter is then two lines — `let ranges = regions::mapped_ranges(pid)?;` and
`|a| regions::contains(&ranges, a)` — and `check_interpreter.rs` stops importing `proc_maps`
at all, which is the L2→dependency bypass §1 describes. `regions` already owns `MapRange`; it
is where a caller should get "the mappings" and "is this address in one" from, and where the
boundary rule belongs next to the other region logic the module already tests.

Two reasons this beats keeping a private `in_any_range` inside `check_interpreter`: the
predicate is memory-domain logic and would otherwise stay in `remote_debugging` where nothing
can construct a fixture for it, and flattening to `(u64, u64)` is what makes it testable at
all — so the flattening should live with the thing that needs it, not in the consumer.

**One allocation per call, and the only part of this change that is not a pure move.**
`mapped_ranges` builds a `Vec` the current code does not. The call happens once per
`check_interpreter_addresses`, so once per module tried during a pre-3.13 attach. The cost is
nil; flag it in review rather than letting it arrive unannounced.

**`Option`, not `Result`, from the pure function.** Every failure inside the loop is already
swallowed and retried against the next candidate. The only thing the caller learns is whether
anything was found, and the message it then produces belongs to the adapter — exactly the
argument [0024 §4](0024-detect-policy-takes-its-reads.md#4-implementation-decisions) makes.

**The handle gets opened once, as a consequence rather than a goal.** Making the read a
closure hoists `reader::open_handle` to the adapter, so the closure calls `read_u64_h` and the
scan stops opening and dropping a fresh `ProcessHandle` per read, which `reader.rs`'s own doc
comment tells callers not to do. The volume is small (`scan_size` is
`runtime_interpreters_head + 64`, so 12–13 candidate words, at most two reads each), so this
is tidiness rather than a measured win: on Linux the handle *is* the pid and costs nothing,
while on Windows it is an `OpenProcess` and on macOS a `task_for_pid`. One observable
difference. If the handle cannot be opened, the adapter fails once with that reason instead of
failing 26 times and reporting "no valid PyInterpreterState". No caller sees it today, since
`check_runtime` discards the error into `false`, but the message is better for the day one
does.

**Rejected — a trait for the two questions.** Two closures are the shape
`walk_interpreter_chain` already established; a trait needs a production implementor whose
only reason to exist is to be substituted in tests.

**Rejected — a `test-hooks` gate.** ADR 0005 chose an honest signal over a gate for
`PySession::layout_source()`. A private free function with its own tests costs nothing at
runtime and adds no feature-flagged surface.

**`check_runtime` splits the same way, and the word assembly goes with it.**

```rust
// remote_debugging::check_interpreter — private, unit-tested in-file
fn runtime_matches(
    block: &[u8],
    expected: u64,
    threads_head_offset: u64,
    thread_interp_offset: u64,
    is_mapped: impl Fn(u64) -> bool,
    read_u64: impl Fn(u64) -> Option<u64>,
) -> bool
```

Taking the block as **bytes** rather than words is what earns this split. Assembly holds the
function's untested rules: that the words are little-endian, and that a trailing partial word
is dropped rather than read short. Leaving it outside, between two `reader::` calls, is what
makes the remainder look like `a == b`. Inside, it is four rules a test can state.

`check_runtime` then keeps its signature and its `-> bool`, and its body is the two reads,
the closures, and one call. Every read failure still means "not this runtime" rather than an
error, which is the fail-closed posture its doc comment describes.

**Rejected — sourcing `expected` from the block.** `runtime_interpreters_head` is inside the
scanned window, so the head is read twice: once as its own `u64`, once as part of the block.
Taking `expected = words[runtime_interpreters_head / 8]` would make the comparison
self-consistent by construction and save a read. It would also change what is compared, so it
is a behavior change with a race argument behind it, not part of a move — see §7.

## 5. Seams and testing decisions

- **Seam:** `find_self_consistent_interpreter`, unit-tested in a new in-file `#[cfg(test)]`
  module. It is the highest seam that can observe the policy — above it sits only
  `check_interpreter_addresses`, which cannot be reached without a live process, which is
  the problem.
- **New seam needed:** yes, and this is it. One level below the public entry point, private
  to the module, no production caller but the adapter, no feature flag. `regions::contains` is
  not a new seam — it is a pure function tested in the module it belongs to, alongside
  `format_region`, which is the same arrangement for the same reason.
- **What makes a good test here:** stubs returning *specific* words, asserting *which
  address* was chosen. `assert!(…is_some())` passes when the wrong candidate wins, which is
  the failure this exists to catch (convention 6). Use distinct, non-zero offsets for
  `threads_head` and `interp` so that a test cannot pass with the two swapped.
- **Prior art:** `session::walk_interpreter_chain` and its tests — same shape, same reason,
  and the closest thing the crate has to a house style for this.
- **Cases:**
  1. The happy path: one candidate whose `threads_head` read yields a mapped tstate whose
     `interp` read yields the candidate back. Assert the returned address.
  2. First match wins: two self-consistent candidates, assert the earlier word.
  3. A candidate whose back-pointer disagrees is skipped and a **later** candidate wins.
     This is the rule that stops the scan accepting the first plausible-looking word.
  4. An unmapped candidate is skipped without any read being attempted (assert via a counter
     in the stub closure — the ordering is what keeps a bogus address out of `read_u64`).
  5. A `threads_head` read that returns `None` is skipped; a later candidate wins.
  6. A NULL `threads_head`, and an unmapped one, are each skipped.
  7. An `interp` read that returns `None` is skipped; a later candidate wins.
  8. No candidates, and all candidates failing, both yield `None` — and
     `check_interpreter_addresses` still produces its `bail!`.
  9. Regression guard: `tests/remote_debugging.rs::find_runtime_pre_3_13_locates_the_runtime`
     and the 3.8–3.12 live-smoke legs are unchanged and remain the arbiter that a *real*
     runtime is still found.

  For `runtime_matches`:

  10. **Little-endian assembly.** A block whose bytes spell one candidate read little-endian
      and a *different* value read big-endian, with only the little-endian one
      self-consistent. This is the case that has never existed and that spec 0003 §2 says is
      wrong today; it is the reason the assembly moves inside.
  11. A block whose length is not a multiple of 8 drops the trailing bytes and does not
      panic — today's `bytes.len() / 8` silently, and the replacement explicitly.
  12. `expected` matched by a *later* self-consistent candidate but not the first ⇒ `false`.
      The comparison is against the first match only; this is the head/main rule of §7 seen
      from the other side, and it is the one a reader is most likely to assume otherwise.
  13. `expected == 0` (a runtime caught mid-`Py_Initialize`) ⇒ `false`, because no mapped
      candidate can equal it. Fails closed, and no live target stages it on demand.
  14. An empty or sub-8-byte block ⇒ `false`.

  For `regions::contains`, in `memory::regions`' existing test module — which already tests
  `format_region` against hand-built values, so the fixture style is settled:

  15. The boundary: `start` is in, `start + size - 1` is in, `start + size` is **out**. The
      rule `maps_contains` spells as `addr < m.start() + m.size()` and that nothing today can
      state.
  16. An address before the first range, after the last, and in a gap between two ⇒ `false`;
      an address in the second of two ranges ⇒ `true`.
  17. Empty ranges ⇒ `false` for any address, which is what a process whose maps could not be
      read must produce.

## 6. Out of scope

- **Any change to the scan's rules.** This spec pins current behavior. If a case turns out
  to be wrong, that is a bugfix spec, not a quiet fix inside a refactor.
- **Sourcing `expected` from the block instead of its own read.** Reasoning in §4, argument
  in §7. It is the one change here that a live target could notice.
- **Making the unaligned `&[u64]` cast safe.** That is
  [0003](0003-remote-walk-and-read-hardening.md) item A, and this spec deliberately does not
  absorb it — see §7, which is now the more important of the two ordering notes.
- **Widening the scan window.** `scan_size = runtime_interpreters_head + 64` comes from
  `memory::process::find_runtime_pre_3_13` and is a pre-3.13 offsets question
  ([ADR 0010](../docs/adr/0010-pre-3-13-offsets-stay-hand-maintained.md)), not a scan
  question.
- **Relocating the module.** `check_interpreter` staying in `remote_debugging` while its sole
  caller sits in `memory` is the same inverted arrow
  [0022 §4](0022-image-lookup-into-memory-binary.md#4-implementation-decisions) names and
  explicitly leaves standing.
- **`memory::binary`'s direct `get_process_maps` call.** `find_python_modules` also reaches
  `proc_maps` rather than going through `regions`, but it wants each mapping's *path*, not its
  extent, so it is not the same call and consolidating it is not free. It is also L1 calling
  its own dependency, which the layering allows; only the L2 bypass this spec removes is a
  layering matter. Leave it, or fold it in separately if `regions` ever grows the accessor it
  would need.
- **Anything else in `memory::regions`.** `list_regions`, `print_region` and `format_region`
  are untouched. Narrowing `format_region` to private is
  [0030](0030-format-region-is-not-public-api.md) — worth folding into this change since it
  opens the same file, but tracked separately so it does not ride along unannounced.

## 7. Further notes

**[0003](0003-remote-walk-and-read-hardening.md) item A has to land first.** Item A replaces
`check_runtime`'s unaligned `&[u64]` cast with
`chunks_exact(8).map(u64::from_le_bytes)` and gives `check_interpreter_addresses` an iterator
instead of a slice. Moving the assembly into `runtime_matches` **deletes that cast as a side
effect** — which would make this cleanup silently fix a UB-and-byte-order bug, exactly the
"rewrite hiding inside a move" that [0021 §4](0021-version-grammar-module.md#4-implementation-decisions)
rules out. Land 0003 item A on its own, with the live matrix as its gate, and this spec is
then a pure move that puts the now-correct assembly somewhere a test can see it. Case 10
becomes the regression guard 0003 could not write for itself, which is the real reason to
want both.

**Why `expected` keeps its own read, for now.** It is read separately from the block, and
`runtime_interpreters_head` is inside the block, so the head is read twice at two moments. If
a sub-interpreter is linked in between them, `expected` is one head and the block holds
another, the comparison fails, and a perfectly good runtime is rejected — attach then reports
"no `_PyRuntime`" for a race. Sourcing `expected` from the block closes that by construction
and removes a read. It is a small, defensible change and it is still a change: it needs the
pre-3.13 legs and the 3.12 sub-interpreter fixture as its gate, and a spec of its own or an
explicit line in the commit that makes it. Note it is *this* extraction that makes it a
two-line change, which is a decent argument for doing them in this order.

**Why case 2 describes something real.** `check_runtime`
requires the *first* self-consistent candidate in the scanned block to equal
`interpreters_head`, and a second one sits nearby: `struct pyinterpreters` puts
`main` immediately after `head` (3.10 `Include/internal/pycore_runtime.h:81-94`, 3.12
`:87-104`), so `main` sits at `runtime_interpreters_head + 8`, well inside the 64-byte window.
On a target with a live sub-interpreter the chain is "newest first", so `head != main` and
both pointers pass the back-pointer check. Word order is what picks the right one — `head` is
scanned first and matches `expected`.

That path is already exercised: `live_smoke.rs::live_monitor_records_every_interpreter_not_only_the_first`
gates on 3.12+, 3.12 is a pre-3.13 build and so attaches through `find_runtime_pre_3_13`, and
`subinterp_spin.py` prints `READY` only once *both* interpreters have collected — so the
sub-interpreter is alive and at the head of the chain when `check_runtime` runs. The unit
test is therefore not filling a gap; it states the rule at the level where a refactor would
break it, on a machine with no Python, and records why the rule exists.

Independent of [0021–0024](README.md), though it is the same genre of change and reads well
next to [0024](0024-detect-policy-takes-its-reads.md): the two are the crate's remaining
"policy fused to a pid" functions, and together they are the last places where deciding what
gcscope believes about a target cannot be tested without one.
