# 0027 — Split the process-tree search from what it reads

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/memory/process.rs`, 2026-08-09
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0004](../docs/adr/0004-per-platform-image-layout.md)

## 1. Problem statement

An operator runs `gcscope gc-stats <pid>` against a venv launcher, a `py` shim or a
`multiprocessing` parent, and gcscope finds the real interpreter one or two processes down —
or tells them it could not, in a message assembled from what the walk tried. Which of those
happens, and what the message says, is decided entirely by `search_pid_and_children`.

Its rules are policy, and there are more of them than the twelve-line body suggests:

- the process itself is tried **before** its children, at every level;
- the walk is depth-first in child order, and the **first** success wins — later children and
  their subtrees are never visited;
- `MAX_DEPTH` is checked **after** the self-attempt, so a process at depth 3 is still tried
  but its children are not enumerated: four levels of processes, three levels of recursion;
- a node with no children calls `try_find_runtime` a **second** time, purely to produce an
  error to return;
- a node *with* children **discards its own failure** — `if let Ok(found)` drops the `Err`, so
  the message an operator sees lists why each child failed and never says why the process
  they named did;
- child errors are aggregated as `  child <pid>: <err>` and nested, so a grandchild's
  aggregate appears inside a child's line;
- stopping at the depth cap and stopping at a childless leaf produce **different** messages
  for the same underlying situation.

None of that has a test. `src/memory/process.rs` has no `#[cfg(test)]` module at all, and the
function takes a `u32` and calls `try_find_runtime` (which reads every module of the target
off disk) and `get_child_pids` (which opens the process through `remoteprocess`) itself. So
observing any rule above means having a live process tree of the right shape — and the shapes
that matter are ones a test cannot arrange: a four-deep chain, a parent whose second child is
the interpreter, a process that reports itself as its own child.

That last one matters. `get_child_pids` returns whatever the OS reports and swallows every
error into an empty `Vec`, so the walk's termination rests on `MAX_DEPTH` alone. It is the
same class of guard [0003](0003-remote-walk-and-read-hardening.md) item B adds to the
interpreter chain, and just as untested.

## 2. Solution

The walk becomes a free function that takes its two reads — *does this process have the
runtime* and *what are this process's children* — as arguments, and `search_pid_and_children`
becomes the adapter supplying the real ones. Behavior is unchanged; every rule above becomes a
unit test that runs in `cargo test` in milliseconds, with no Python and no process tree.

## 3. User stories

1. As an operator attaching to a venv launcher, I want the retarget to the real interpreter to
   keep working exactly as it does, so that the change is invisible to me. Spec
   [0009](0009-venv-launcher-child-retarget.md) builds on this path; it should build on a
   tested one.
2. As a gcscope maintainer, I want "self before children, depth-first, first success wins"
   pinned, so that a reordering — or a change to visit children first, which is tempting when
   chasing a shim — fails locally rather than on a matrix leg that happens to spawn one.
3. As a gcscope maintainer, I want the depth cap to be what provably terminates the walk, so
   that a process tree the OS reports as cyclic returns an error instead of recursing until
   the stack goes.
4. As an operator reading a failure, I want the message rules — which errors are aggregated,
   which are dropped, which of the two "not found" texts I get — to be stated somewhere a
   change would trip over, since the message is the only diagnostic this path has.

## 4. Implementation decisions

The same shape as `session::walk_interpreter_chain`, and as
[0024](0024-detect-policy-takes-its-reads.md) and
[0025](0025-interpreter-scan-takes-its-reads.md) propose for the other two "policy fused to a
pid" functions in the crate:

```rust
// memory::process — private, unit-tested in-file
fn search_from(
    pid: u32,
    depth: u32,
    find: &dyn Fn(u32) -> Result<(u64, String)>,
    children_of: &dyn Fn(u32) -> Vec<u32>,
) -> Result<(u64, String)>
```

`search_pid_and_children` keeps its signature and becomes two lines: build the closures, call
`search_from`.

**`&dyn Fn`, not `impl Fn`, and this is the one place in the crate that deviates.** The
function is recursive, so the closures must be passed on by reference at every level; `&dyn`
says that without threading generic parameters through the recursion, and the walk runs once
per attach, so the indirect call is free. Elsewhere — `walk_interpreter_chain`, and 0024/0025
— `impl Fn` stays right, because none of those recurse.

**`children_of` returns `Vec<u32>`, not `Result`.** `get_child_pids` already swallows every
failure into an empty vector, so "this process has no children" and "this process could not be
opened" are the same answer today. Giving the closure a `Result` would invent a distinction
the production code does not make and that no test could then hold it to. If that conflation
is ever worth fixing, it is a change to `get_child_pids` and its callers, with its own
reasoning.

**Nothing about the messages changes.** The two `bail!` strings, the `  child <pid>: ` prefix
and the `\n` join move verbatim. They are what the tests assert against, so editing them in
the same change would leave the assertions describing the new text rather than pinning the old
— the "rewrite hiding inside a move" that
[0021 §4](0021-version-grammar-module.md#4-implementation-decisions) rules out.

**Rejected — folding in the double `find` call on leaves.** Removing the second call means
keeping the first call's error instead of discarding it, which changes what a leaf's message
says. It is a good change; it is not a move. See §7.

**Rejected — a trait, or a struct of two function pointers.** Two closures are the crate's
established shape, and neither alternative buys anything a recursive call cannot already pass
along.

## 5. Seams and testing decisions

- **Seam:** `search_from`, unit-tested in an in-file `#[cfg(test)]` module. The highest seam
  that can observe the walk — above it sit only `find_runtime` and `find_runtime_module`,
  which need a pid, which is the problem.
- **New seam needed:** yes, and this is it: one level below the public entry points, private,
  no production caller but the adapter, no feature flag.
- **What makes a good test here:** a stub `find` backed by a fixed set of "interpreter" pids
  and a **call counter**, plus a `children_of` backed by a literal tree. The counter is what
  makes the ordering rules assertable: "child b was never tried" says more than "child a was
  returned", and it catches a walk that finds the right answer by visiting everything.
- **Prior art:** `session::walk_interpreter_chain` and its tests; and
  `binary::parse_macho_refuses_a_fat_header_claiming_impossible_arch_count`, whose "real
  assertion is that this returns at all" is exactly the shape case 6 needs.
- **Cases:**
  1. Runtime at the named pid: `find` called once, `children_of` **never** called.
  2. Runtime in a child: the returned `(addr, path)` is the child's, not the parent's.
  3. Depth-first, first success wins: children `[a, b]` where both would succeed ⇒ `a`'s
     result, and `find` was never called on `b`.
  4. A grandchild at depth 2 is reached and returned.
  5. The cap: the only interpreter sits at depth 4 ⇒ error, and `find` was never called on it.
     Its sibling at depth 3 ⇒ found, because the cap is tested after the self-attempt.
  6. `children_of(p) == [p]` — the process the OS reports as its own child — returns an error
     rather than recursing. Like the fat-header test, the real assertion is that it returns.
  7. A childless leaf calls `find` **twice**. Pins today's cost: every module of the target is
     read off disk a second time to produce a message.
  8. A node with children returns a message naming each child's failure and **not** its own.
     Pins the dropped `Err`; see §7.
  9. Message shape: `  child <pid>: <err>` per line, and a grandchild's aggregate nested
     inside its parent's line.
  10. Which text: childless below the cap ⇒ `find`'s own error propagated; at the cap ⇒
      "Could not find valid PyRuntime in process N or its children", even with no children.
  11. Regression guard: `tests/memory.rs::find_runtime_locates_the_cookie` and the live matrix
      unchanged — they remain the only evidence the walk works against a real tree.

## 6. Out of scope

- **Any change to the walk's rules or its messages.** This spec pins current behavior. Cases
  7, 8 and 10 each document something worth improving; improving them is §7's follow-up, with
  its own commit and its own reasoning.
- **`MAX_DEPTH`'s value.** Three is a judgement about how deep a launcher chain gets, unchanged
  here. The tests take the depth as a parameter of the fixture, not the constant, so a future
  change to it does not rewrite them.
- **`try_find_runtime`'s internals** — the section finders and the cookie check. That is
  [0026](0026-unit-test-section-finders.md); this spec stubs the whole function out.
- **Searching children concurrently, or preferring a child that looks like an interpreter.**
  [0009](0009-venv-launcher-child-retarget.md) is the spec that touches retarget policy, and it
  will be easier to write against a tested walk.
- **Distinguishing "no children" from "could not enumerate children".** Reasoning in §4.

## 7. Further notes

**The follow-up this unlocks, in three lines.** Replace `if let Ok(found) = find(pid)` with a
`match` that keeps the `Err`, then use it in both places that currently work around not having
it: return it directly for a childless leaf (deleting the second `find` call, case 7) and add
it to the aggregate for a node with children (case 8), so an operator attaching to a launcher
finally sees why the process *they named* failed alongside why each child did. It is a
message change and a behavior change to how many times the disk is read, so it wants its own
commit — but it is small, and this extraction is what lets it be written test-first, changing
cases 7, 8 and 10 deliberately rather than discovering afterwards that they moved.

**Relationship to [0026](0026-unit-test-section-finders.md).** Both add tests to
`src/memory/process.rs`, which has none; whichever lands first creates the `#[cfg(test)]`
module and the second just adds to it. They are otherwise independent — 0026 needs synthetic
images and tests the pure arithmetic, this one needs no fixtures at all and tests the walk
above it. Neither blocks the other, and this one is the smaller of the two.
