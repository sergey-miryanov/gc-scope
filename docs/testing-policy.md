# Testing policy

Which test to write, and when. The reasoning lives in the ADRs, which this file links
rather than repeats: [0005](adr/0005-testing-strategy.md) for the layers and why the live
matrix is the gate, [0009](adr/0009-performance-guarded-by-shape.md) for benchmarks,
[0004](adr/0004-per-platform-image-layout.md) for platform facts,
[0011](adr/0011-layout-equivalence-sweep.md) for layout coverage.

Two questions decide it. How does this code fail, and what is the strongest oracle
available for the property you want to hold?

## Pick by failure mode

| How the code fails | Catches it | Gives false comfort |
|---|---|---|
| Returns a wrong value in silence | Live test against an independent oracle | Unit test with a hardcoded expectation |
| Panics, hangs, or reads out of bounds | Randomized property, then fuzz | Live test; it exercises one input |
| A guard never fires and nobody notices | Mutation audit | Any green suite |
| State corrupts across calls | Lifecycle test | Single-shot CLI run |
| Slows down as the workload grows | Op-count invariant | Wall-clock benchmark |
| Logic error in code that needs no process | Unit test | |

## The ladder

Add the cheapest kind that has a real oracle for the property:

    unit → randomized property → live → lifecycle → fuzz → mutation audit → benchmark

Skipping a cheaper kind that would have caught the bug is a review finding, and so is
adding an expensive kind to a property a cheaper one already pins.

## Unit tests

**Add when** the logic runs without a process: encoding, table lookup, geometry,
selection, formatting, dedup.

**Extract a seam rather than skip the layer.** `OffsetTable::decode_gc_stats` takes
`&[u8]` and `monitor::context::select_fresh` takes a `&[GcStat]` for this reason. Logic
that only runs against a live target gets tested against one interpreter, once.

**Do not count it as coverage** when correctness rests on a fact this crate does not
compile: a struct offset, a section name, an OS behavior, a CPython layout. The test
passes with wrong data, which is worse than having no test. Those facts belong to the
live matrix.

**Cost:** the whole suite runs in under a second with no Python installed.

## Live and integration tests

**Add when** the change's correctness depends on something external: a CPython build, an
image format, a process lifecycle, a platform's memory map. This is the only layer that
catches a wrong offset, since a wrong one executes the same instructions as a right one.

**Ask the target, never hardcode the answer.** Where the target is the only oracle,
compare against what it reports: `sys.version_info`, its own published `_Py_DebugOffsets`,
the `Py_GIL_DISABLED` flag.

**Assert shape, not success.** A mis-keyed decode returns a full table of garbage and
passes any non-empty check. Assert the expected `(kind, entries)`, the exact
`(generation, entry)` index set, and a relationship between values the fixture controls.
`tests/fixtures/spin.py` seeds collections 20/5/1 so a decode that aliases two generations
is distinguishable from a correct one.

**Cost:** an interpreter, attach permission, and a leg of the CI matrix (3 OSes × 3.8
through 3.15 plus 3.15t). Mark it `#[ignore]` so the default `cargo test` stays green
where attach is not permitted.

## Lifecycle tests

**Add when** the behavior spans calls and has no CLI surface: a cache that must hit the
second time, a session that must survive re-attach, state that must not leak between
ticks. `tests/lifecycle.rs` covers the two `PySession` paths no one-shot run reaches.

**Prefer an honest signal to a test hook.** `PySession::layout_source()` ships in the
binary, so the tested configuration is the shipped one. A `#[cfg(feature = "test-hooks")]`
path would test something users never run.

**Cost:** ptrace permission, one dedicated CI job.

## Randomized property tests

**Add when** a function is total over an input space too large to enumerate and you can
state a property that holds for every input. Three that hold here: a round trip (`Display`
then parse returns the original), an invariant (whatever comes back is representable), and
liveness (it returns at all).

**Fix the seed**, so a failure reproduces. `image_scan_survives_adversarial_bytes` uses
splitmix64 from a constant.

**Sweep the space when it is small enough to enumerate.**
`every_representable_version_round_trips_through_the_scanner` covers 102,900 cases in
milliseconds. A hand-written table names the cases someone thought of, which are the cases
a rule already handles.

**Reach for this before fuzzing.** It runs on every platform, needs no toolchain, and
found the `parse_macho` hang.

**Cost:** milliseconds to seconds, in the normal suite.

## Fuzz targets

**Add when all three hold:**

1. The bytes come from outside gcscope. Process memory, an on-disk image, a file a user
   points at.
2. The code branches structurally, so coverage guidance beats blind generation: headers,
   length fields, offsets, section tables.
3. A crash, hang, or out-of-bounds read is the bug you are hunting.

**Do not fuzz for wrong answers.** Without a property to assert, a fuzzer only proves the
code did not panic. State the property in the target, as
`fuzz/fuzz_targets/scan_image_for_version.rs` does for serial and release level, or use a
different layer.

**Cost:** a nightly toolchain, a corpus, CI minutes, and Linux only. Windows MSVC cannot
run it: the ASAN runtime mismatches rustc's LLVM, and `--sanitizer none` fails to link
libFuzzer's sancov symbols. The CI leg is a 120s smoke gate; run longer by hand when
touching the code it covers.

## Mutation audits

This layer tests the tests, answering "does any assertion pin this line?", which a green
suite cannot.

**Run it after** adding a guard whose only job is to prevent something: a bound, a clamp,
an early return, an error branch. Those are the mutants that survive. Also after a review
finds a test asserting less than its name claims, and periodically over paths where a
silent wrong answer is the failure mode.

**A surviving mutant is a missing assertion.** Add it, or record why the line cannot be
pinned. The `narches` bound in `parse_macho` carries such a note: its only effect is
latency, so only a timing assertion could tell the difference, and that flakes.

**Never a gate.** Minutes per run plus human triage of unviable mutants makes a red leg
noise.

**Cost:** roughly 20 minutes for one module locally.

## Benchmarks

None exist. [ADR 0009](adr/0009-performance-guarded-by-shape.md) sets the threshold: a
real scaling axis, a cost that is our algorithm rather than syscalls, and a stable
invariant or an SLA to judge a number against. All three must hold.

**Count operations instead.** Where wall-clock is dominated by syscalls, allocation, or
the scheduler, a timing test measures the platform and flakes across machines. Assert
attaches, memory reads, or allocations as a function of the workload.

**Trigger to revisit:** running the monitor against a tree of tens of PIDs and seeing
per-tick cost matter, or restructuring tree discovery and dedup.

## Rules for every kind

1. **A test name is a claim.** It must fail when that claim is violated. A name promising
   more than the body asserts has slipped through twice here.
2. **Prefer an independent oracle to a constant.** Ask the interpreter, invert the
   function, or assert a relationship. A literal copied from the implementation tests
   nothing.
3. **Say what a test does not prove**, and where the rest of the coverage lives. The
   adversarial tiers assert that the scan returns something representable, and nothing
   about which version is right.

## Not required

- A unit test for a value that comes from outside the crate; it cannot fail correctly.
- A fuzz target for a pure function over a small input space. Sweep or randomize instead.
- A mutation leg in CI.
- A benchmark, until ADR 0009's trigger fires.
- A live leg per version for a change that touches no version-dependent code.
