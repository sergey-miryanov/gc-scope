# 0036 — Take the process read out of `OffsetTable`

- **Status:** Not started — ordered after [0028](0028-process-reader-owns-the-handle.md)
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/remote_debugging/offsets/offset_table.rs`, 2026-08-10
- **Respects:** [ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md),
  [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

Nothing changes for an operator. `OffsetTable` is the layout description
[ADR 0003](../docs/adr/0003-layout-driven-gc-stats-decode.md) built so a version's geometry is
stated once and decoded from bytes, and it also reads a process.

`OffsetTable::read_gc_stats(&self, handle: &ProcessHandle, iid: i64)` is the only reason
`offset_table.rs` names any of `read_process_memory::ProcessHandle`, `crate::memory::reader`,
`anyhow::Context` or `anyhow::Result`. Each is used **exactly once** in an 829-line file,
all four inside that one method. Delete it and the module is pure.

It earns very little for that. Its body is two shape guards, one read, and a call to
`decode_gc_stats`, which is already `pub`, already pure, and documented as such ("Pure — no
process access — so the per-version entry geometry and field offsets are testable without a
target").

**The two callers pay for it by mutating a clone.** `read_gc_stats` reads the address off
`self.gc_stats_addr`, but the address is resolved per interpreter at runtime, so both call
sites clone the whole table to set one field:

```rust
// PySession::gc_stats_global
let mut global_table = table.clone();
global_table.gc_stats_addr = self.gc_stats_region_addr(gc_addr)?;
global_table.read_gc_stats(&self.handle, iid)

// PySession::read_interpreter_stats, per iteration of the walk
interp_table.gc_stats_addr = Some(addr);
interp_table.read_gc_stats(&self.handle, iid).map(Some)
```

That is per-call data travelling through a field on a layout description, and the code already
knows it: `gc_stats_per_interpreter`'s comment says "the walk's own copy: `gc_stats_addr` is
the one field an iteration sets, and the rest is the same for every interpreter." The clone
exists for one `Option<u64>`, and it is what forces `read_interpreter_stats` to take a
`&mut OffsetTable` it otherwise has no use for.

**Read-then-decode is already what the rest of the tree does.** `stats_buffer_len` and
`decode_gc_stats` are both `pub`, and two other consumers compose them directly:
`snapshot::collect::parse_gc_entries`, whose doc calls `decode_gc_stats` "the exact path the
monitor uses", and `tests/probe.rs` at three sites, which is an external crate reading its own
buffer. `read_gc_stats` is the odd one out, with only the two callers above.

**The tell is in its own tests.** `offset_table.rs`'s decode tests set
`table.gc_stats_addr = Some(0x1000)` with the comment *"any non-None value; decode never reads
it"* — four times. That line is ceremony owed entirely to `read_gc_stats`' early return, in
tests that do not call it.

## 2. Solution

`OffsetTable` describes a layout and decodes bytes. It does not read a process. `PySession`
does the read — it is the type that holds the reader — and calls the pure decoder, which is
what the rest of the crate already does.

## 3. User stories

1. As a gcscope maintainer reading `offset_table.rs`, I want a file that cannot touch a
   process, so that "is this testable without a target" is answered by the module rather than
   by checking each method.
2. As a gcscope maintainer, I want the stats address passed as an argument rather than set on a
   cloned table, so that the walk's per-interpreter value is visibly per-call and cannot be
   confused with the version's geometry.
3. As a gcscope maintainer, I want the "no stats region is a shape fact, a failed read is an
   error" rule (C6) pinned by a test, so that the distinction the doc comment draws is proven
   rather than described.
4. As an operator running `monitor --all` against a multi-interpreter target, I want each
   interpreter's own stats, so that moving the address out of the table does not quietly make
   the walk read one region three times.

## 4. Implementation decisions

**`read_gc_stats` is deleted, not relocated.** Moving it onto `PySession` unchanged would
carry the `gc_stats_addr`-on-a-clone convention with it, which is half the problem.

**`PySession` grows one private helper**, so the two call sites do not duplicate the guards or
the context message:

```rust
// PySession — private
fn read_and_decode_stats(&self, table: &OffsetTable, addr: u64, iid: i64) -> Result<Vec<GcStat>>
```

It takes the address as an argument, asks `table.stats_buffer_len()`, reads through the held
reader, and returns `table.decode_gc_stats(&raw, iid)`.

**The two guards keep their semantics, and this is the part to get right.** A `None` stats
address and a `None` `stats_buffer_len` each yield `Ok(vec![])`: shape facts rather than
failures. A failed *read* propagates as `Err`, carrying the
`"Failed to read gc_stats buffer at {addr:#x} ({total} bytes)"` context verbatim. Both callers
resolve the address before calling, so the first guard moves to where the resolution happens
(`gc_stats_global` keeps its `?`; `read_interpreter_stats` already has a
`let Some(addr) = … else { return Ok(None) }`) and the second stays in the helper.

**Two clones and a `&mut` go with it.** `gc_stats_global` stops cloning the table;
`gc_stats_per_interpreter` stops building `interp_table`; `read_interpreter_stats` loses its
`interp_table: &mut OffsetTable` parameter and reads the shared `&OffsetTable` like everything
else in the walk. This is a consequence of the change rather than its goal, but it is the part
a reviewer will see first.

**`gc_stats_addr` the *field* stays.** Whether a layout table should carry a live process
address at all is a real question — `gc_stats_addr_is_per_interp` documents it, `pre_3_13.rs`
asserts it starts `None`, and the constructors in `offsets/mod.rs` set it — and answering it
means auditing every construction site. This spec removes the *mutation* at the two call sites
and the method that read it; it does not settle what the field is for. See §6.

**Rejected — doing this instead of [0028](0028-process-reader-owns-the-handle.md)'s migration.**
Tempting, since it deletes the seventh handle-holding site rather than porting it. But 0028 is
a mechanical migration whose value is that every call site is checkable by eye, while this
reworks two call sites and decides the fate of a public method. Different reviews. 0028 ports
the parameter; this retires it.

**Rejected — having `parse_gc_entries` and this helper share a wrapper.** They decode to
different types (`GcStat` versus the TUI's `GcEntry`) and `collect.rs` already reads its own
buffer for its own reasons. The shared thing is `decode_gc_stats`, and it already is shared.

## 5. Seams and testing decisions

- **Seam:** `offset_table.rs`'s existing `#[cfg(test)]` module for the decoder (untouched), and
  `PySession`'s live targets for the composition. The helper is one level below `gc_stats`,
  which is where the existing `tests/monitor.rs` fault seam already reaches it.
- **New seam needed:** none. This spec *removes* IO from a module, so the pure surface grows
  rather than the instrumented one.
- **What makes a good test here:** the C6 distinction, asserted as `Ok(vec![])` versus `Err` —
  not as "it returned". The two are the same shape to a caller checking `is_ok()`, which is why
  the rule has survived this long as prose.
- **Prior art:** `tests/probe.rs`' three `stats_buffer_len` + `decode_gc_stats` sites — an
  external crate already composing exactly what this helper composes, which is the evidence the
  primitives are sufficient.
- **Cases:**
  1. `offset_table.rs` names none of `ProcessHandle`, `memory::reader` or `anyhow::Context`,
     and `cargo build` succeeds. The property the spec buys, and the compiler checks it.
  2. The four decode tests drop their `gc_stats_addr = Some(0x1000)` line and still pass. If
     one of them needs it, the premise was wrong.
  3. Guards: a table with `stats_buffer_len() == None` yields `Ok(vec![])`, and a read that
     fails yields `Err` carrying the address and byte count. The pair, asserted together.
  4. `gc_stats_global` and `gc_stats_per_interpreter` no longer clone `OffsetTable`, and
     `read_interpreter_stats` takes `&OffsetTable`.
  5. Regression: `tests/monitor.rs` and `tests/probe.rs` pass unedited, and the live matrix is
     green on both stats kinds — `InlineArray` and `RingBuffer` resolve their addresses
     differently and take different lengths, so one leg does not cover the other.
  6. Regression, and the sharp one:
     `tests/live_smoke.rs::live_monitor_records_every_interpreter_not_only_the_first`. The walk
     now passes an address per iteration instead of writing one into a table it reuses;
     collapsing that to a single shared address is the plausible mistake, and this is the test
     that sees it.

## 6. Out of scope

- **Removing the `gc_stats_addr` field.** Reasoning in §4. It is the next question, not this
  one, and it wants an audit of every construction site plus a judgement about whether
  `gc_stats_addr_is_per_interp` still describes anything once no method reads the field.
- **Any change to `decode_gc_stats`, `stats_buffer_len` or `entry_byte_offset`.** Untouched;
  they are the primitives this spec composes and they already have their own tests.
- **`snapshot::collect`'s decode path.** It already reads its own buffer and calls the pure
  decoder. Nothing to do.
- **[0028](0028-process-reader-owns-the-handle.md)'s migration.** It lands first and this
  retires the row it adds. Doing them in one change means the mechanical diff stops being
  checkable by eye, which is 0028's whole argument for being an S.
- **The `#![allow(dead_code)]` at the top of `offset_table.rs`.** Whether it is still earned
  after a method is deleted is worth a look and is not this spec.

## 7. Further notes

Ordered after [0028](0028-process-reader-owns-the-handle.md), and cheaper for it: with
`ProcessReader` in place, the helper reads through a value `PySession` already holds, so the
only question left is where the guards go. Landing it before 0028 would mean reworking these
two call sites against the raw upstream handle and then again when the owner arrives.

Write case 3 first. The `Ok(vec![])`-versus-`Err` rule is the load-bearing part: a build with
no decodable stats must not look like a failed read, or the monitor routes a healthy target
into the give-up ladder. Today that rule lives in a doc comment on the method this spec
deletes.
