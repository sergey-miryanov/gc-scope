# 0033 — Give the snapshot poller's failure ladder a seam that reaches it

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** S
- **Origin:** review of `src/snapshot/poller.rs`, 2026-08-10
- **Respects:** [ADR 0005](../docs/adr/0005-testing-strategy.md),
  [ADR 0001](../docs/adr/0001-pysession-resolve-once-facade.md)

## 1. Problem statement

`SnapshotPoller::poll` is what stands between the TUI and a target that blinks. Its whole body
is a failure ladder:

```rust
match collect_data(&self.session, &self.request) {
    Ok(data) => Ok(data),
    Err(e) => match self.session.revalidate() {
        Revalidated::Fresh   => collect_data(&self.session, &self.request),
        Revalidated::Changed => Err(e.context("target PID is now held by a different program")),
        Revalidated::Dead    => Err(e.context("target process is gone")),
    },
}
```

One of the three arms is exercised.
`tests/tui.rs::poller_errors_after_the_target_process_exits` polls a healthy interpreter,
kills it, waits for the OS to reap it and asserts the error names the gone process — the
**Dead** arm, end to end, plus the `Ok` path as its first line.

**Fresh** is the one that matters and the one nothing reaches. It is the arm the TUI relies on
every frame: a read blips, `revalidate` soft re-attaches, the retry succeeds and the session
survives. Nothing proves it does — nor that the retry happens exactly once rather than
looping.

**A seam for this was already built, and it does not reach the code it was built for.**
`poller.rs` carries a `#[cfg(feature = "test-hooks")] from_session`, documented as existing
"so the revalidate ladder can be driven off a terminal" and pointing at
`MonitorContext::insert_session_for_test` as its model. It has **zero callers** anywhere in
`src/` or `tests/`. But the deeper problem is that using it would not help: the fault hook the
model depends on cannot make this ladder run.

`PySession::inject_gc_stats_faults` arms `PySession::gc_stats`, and that is the only place
`take_injected_fault` is consumed. `collect_data` never calls `gc_stats`: it reads through `session.gc_stats_region_addr`,
`session.read` and `session.read_u64`, decoding the raw buffer itself with `parse_gc_entries`.
So arming faults on a session and handing it to `SnapshotPoller::from_session` produces a
poller that succeeds. The seam was modelled on the sibling's and inherited a hook that does
not reach this ladder.

`retarget` has a second untested rule in the same 81 lines: it "attaches the new PID first and
swaps the held session in only on success, so a failed attach leaves the current session live"
— the same commit-only-once-it-resolves rule the TUI's PID picker follows. Nothing checks that
a failed retarget leaves the poller attached to the old process rather than to nothing.

## 2. Solution

Add the fault seam that reaches `collect_data`, then use it and the seam that already exists
to drive the ladder, in a live `--ignored` target mirroring `tests/monitor.rs`.

## 3. User stories

1. As a maintainer of the TUI, I want a transient read glitch to be proven not to drop a live
   session, so that the behaviour the TUI depends on every frame is asserted rather than
   assumed from its sibling's tests.
2. As a maintainer, I want the two `.context()` messages an operator actually sees on a lost
   target to be pinned, since they are the only diagnosis this path offers.
3. As a maintainer, I want a failed `retarget` to demonstrably leave me attached to the
   process I was watching, so that a mistyped PID in the picker cannot silently detach me.
4. As a maintainer, I want `from_session` to have a caller, so that a `#[cfg(test-hooks)]`
   seam is a thing the tests use rather than a thing the tests were going to use.

## 4. Implementation decisions

**Add `inject_read_faults`, consumed by the session's read primitives.**

```rust
// PySession — beside inject_gc_stats_faults
#[cfg(feature = "test-hooks")]
pub fn inject_read_faults(&self, n: u32);
```

Consumed at the top of `PySession::read` **and** `PySession::read_u64`. Both are needed:
`collect_gc_stats` reaches the buffer through `read`, while `gc_stats_region_addr` and the
interpreter navigation go through `read_u64`, and `read_i64` delegates to `read_u64` so it
inherits. Gating both makes "the next N reads fail" true across the whole surface
`collect_data` touches, which is what a test needs to be deterministic about *which* read
fails.

**`revalidate` stays outside the instrumented surface, and cases 1 and 2 rest on it.** The
ladder's middle step reads memory too: `revalidate` calls `process::read_cmdline`, then
`soft_reattach`, which opens a fresh handle, re-runs `find_runtime_versioned` and checks the
version word through `reader::read_u64_h` — all of it reaching the process directly rather than
through `PySession::read`/`read_u64`. So none of it consumes the countdown, and that is
what makes N armed faults mean N failing reads *inside `collect_data`* rather than
some of them being spent recovering from the previous one. With 1 armed the first pass fails
and the retry has a clear run; with 2, both passes fail. Route `soft_reattach` through the
session's own read methods later and case 2 stops meaning "the retry happens exactly once"
without anything saying so — which is why this is a decision here rather than something the
next reader rediscovers from a confusing failure.

**A second countdown, not a generalised one.** Leaving `inject_gc_stats_faults` exactly as it
is keeps `tests/monitor.rs` untouched, so this spec cannot perturb the one target that already
covers the monitor's ladder. Two small independent counters beat one shared counter whose
semantics both targets have to agree on — and the two surfaces *nest* rather than sit side by
side, which is the sharper reason not to merge them later: `gc_stats` reaches memory through
`read_u64`/`read_i64`, so an armed read fault fires inside `gc_stats` as well. The counters do
not interfere only because no test arms both. That is a property of the tests, not of the
seams.

**A new `test-hooks` seam is the exception ADR 0005 already made here, not a new precedent.**
[ADR 0005](../docs/adr/0005-testing-strategy.md) prefers an honest public signal to a gate,
and `PySession::layout_source()` is the case it chose that way. This is the other case:
`tests/monitor.rs`'s own header states the reason — "reproducing a *transient* read failure on
a genuinely live process needs a fault seam" — and the process must stay alive for
`revalidate` to return `Fresh` at all. There is no public signal that can stand in for a read
that fails once and then stops failing.

**Live, `--ignored`, `test-hooks`-gated — not a unit test.** `from_session` takes a real
`PySession`, which only `attach` produces, so the target spawns `spin.py` exactly as
`tests/monitor.rs` does and runs under
`cargo test --features test-hooks --test snapshot -- --ignored`. Saying so up front matters:
this adds no coverage to the default `cargo test` run.

**A new `tests/snapshot.rs`, not an extension of `tests/monitor.rs`.** The two targets cover
sibling ladders in different modules with different observable contracts — `PollStatus` plus
lifecycle events there, `Result<CollectedData>` plus `.context()` strings here. Keeping them
apart keeps each target's header honest about what it proves.

**Rejected — asserting `Revalidated::Changed`.** It requires a PID to be reused by a different
program between two polls, which a test cannot force. See §5.

## 5. Seams and testing decisions

- **Seam:** the existing `SnapshotPoller::from_session`, plus the new `inject_read_faults`.
  The first exists and is unused; the second is what makes it useful.
- **New seam needed:** one, and only because the existing one provably cannot reach
  `collect_data`. Reasoning above.
- **What makes a good test here:** arm a *finite* number of faults on a process that stays
  alive, so `revalidate` returns `Fresh` and the retry succeeds: the
  control flow is under test, not a mocked session. `tests/monitor.rs`'s
  `poll_once_with_faults(n)` helper is the shape to copy.
- **Prior art:** `tests/monitor.rs` in full, and specifically
  `poll_recovers_via_fresh_retry_after_a_transient_read_failure` /
  `poll_gives_up_when_the_retry_also_fails` — the same two-case pair against the sibling
  ladder.
- **Cases:**
  1. **Fresh recovers:** arm **1** fault, poll once ⇒ `Ok`. The first `collect_data` fails,
     `revalidate` returns `Fresh` on a live process, the retry succeeds. The rule the TUI
     depends on every frame, and the reason this spec exists.
  2. **The retry also fails:** arm **2** faults ⇒ `Err`. Pins that the retry happens exactly
     once and is not a loop — the distinction case 1 alone cannot make. It is also the guard on
     §4's `revalidate` note: were the ladder's own reads instrumented, they would swallow the
     second fault and this case would return `Ok`.
  3. **Already covered, deliberately not duplicated:** the `Ok` path and the **Dead** arm are
     `tests/tui.rs::poller_errors_after_the_target_process_exits`, which kills the target,
     waits for it to be reaped and asserts the error names it.
     `tests/lifecycle.rs::revalidate_is_clean_after_process_exits` covers the session-level
     half. Add nothing here; the new target's header should say where they live.
  4. **`retarget` commits only on success:** retarget to a PID that cannot be attached ⇒
     `Err`, **and** `poller.pid()` still returns the original, **and** a subsequent `poll`
     still succeeds. The third assertion is the one that matters — an `Err` with a wrecked
     session passes the first two.
  5. **`retarget` on success swaps:** retarget to a second live interpreter ⇒ `pid()` reports
     it and `poll` returns that process's data.
  6. Regression: `tests/monitor.rs` passes unedited — the property that keeps the new counter
     honest — and `tests/tui.rs` is untouched.

**What stays uncovered, and why it is acceptable.** The `Changed` arm needs a PID reused by a
different program between two polls, which no test can arrange. The *decision* behind it is
already tested: `session::classify_cmdline` has five unit tests over that judgement,
and the arm here only wraps it in a message. `tests/monitor.rs` makes the same concession
explicitly — its process-exit test says which internal branch runs "is irrelevant; the
observable contract is InvalidProcess + a single Died."

## 6. Out of scope

- **Changing the ladder, the retry count or the messages.** This pins them. The messages
  become assertions, so editing them afterwards is a deliberate act with a failing test
  attached, which is the point.
- **`collect_data` and everything below it.** Its pure half — `parse_gc_entries`,
  `avg_collection_time_per_gen`, `collections_rate_from_entries` — already has 23 unit tests
  and needs nothing from this.
- **A multi-PID snapshot mode.** `poller.rs`'s doc comment sketches
  `HashMap<u32, SnapshotPoller>`; that is a feature, and it would want these tests to exist
  first.
- **Removing `from_session` instead.** Deleting an unused seam is the other way to resolve
  "nothing calls it", and it is the wrong one: the ladder still would not be covered, and the
  next person would rebuild the same hook.

## 7. Further notes

Independent of every other open spec, and the only one in the group whose production change is
confined to `#[cfg(feature = "test-hooks")]` code — two counters and two guard lines. Nothing
compiled into a release build changes.

Write case 4 first. It needs no fault seam, since a nonexistent PID is enough, so it lands
while the rest is still being wired. It also covers the rule with the most user-visible
failure mode: a mistyped PID in the picker leaving the operator watching nothing.
