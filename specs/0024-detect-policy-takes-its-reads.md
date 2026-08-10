# 0024 — Let version detection's policy be tested without a live process

- **Status:** Not started
- **Kind:** feature — cleanup
- **Effort:** M
- **Origin:** review of `src/remote_debugging/version.rs`, 2026-08-09
- **Respects:** [ADR 0012](../docs/adr/0012-version-detection-fails-closed.md),
  [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
  [ADR 0005](../docs/adr/0005-testing-strategy.md)

## 1. Problem statement

`remote_debugging::version::detect` decides which version gcscope believes, and the version
selects the layout every later decode reads through. Get it wrong and gcscope resolves one
build's offsets against another's memory: the fails-open mode
[ADR 0006](../docs/adr/0006-layout-registration-integrity.md) and
[ADR 0012](../docs/adr/0012-version-detection-fails-closed.md) both exist to prevent, arriving
one step earlier than either of them looks.

The rules it applies are all policy, none of it IO:

- the `Py_Version` symbol beats the image scan, **across all modules** — a scan hit in the
  first module does not beat a symbol hit in the third;
- a decoded word whose major is not 3 is refused, and the scan phase still runs afterwards;
- a word that `from_hex` refuses (a zeroed read) falls through the same way;
- a module unreadable on disk is skipped, not fatal;
- the version word is read as 8 bytes, and re-read as 4 when that read fails **or** when the
  8-byte word decodes to nothing;
- first match wins, in module order.

Not one of those has a test. `detect` takes a `u32` PID and calls `binary::find_python_modules`,
`std::fs::read` and `reader::read_memory`, so observing any of it means having a live
interpreter of the right vintage. Most of the list cannot be produced by a live interpreter at
all: no real CPython publishes a zeroed `Py_Version`, or a word decoding to major 4, or a
module that vanishes between enumeration and read. The rules for a target behaving badly are
the ones nothing exercises.

## 2. Solution

The policy becomes a free function that takes its two reads as arguments, and `detect` becomes
the thin adapter that supplies the real ones. Behavior is unchanged in every case; what
changes is that each rule above becomes a unit test that runs in `cargo test` with no Python
installed.

## 3. User stories

1. As a gcscope maintainer changing detection order, I want the "symbol beats scan across all
   modules" rule pinned by a test, so that a refactor that makes the two phases per-module
   fails loudly instead of silently preferring a scan hit.
2. As a gcscope maintainer, I want the degenerate reads — zeroed word, major 4, short read,
   unreadable module — covered, so that the fail-closed posture ADR 0012 chose is proven
   rather than asserted in a doc.
3. As an operator whose interpreter is mid-teardown when gcscope enumerates it, I want a
   module that disappears to be skipped rather than to abort detection, so that attaching to a
   busy process tree is not a coin flip. This is the current behavior; the spec is what makes
   it stay true.
4. As an operator on 3.8–3.10, I want the scan phase to still run after a module publishes a
   symbol that decodes to nonsense, so that the build with no source but the scanner is not
   lost to another module's bad word.
5. As a gcscope maintainer, I want `detect` itself to stay a short function whose body is the
   IO, so that reading it answers "what does this touch" immediately.

## 4. Implementation decisions

Mirror `session::walk_interpreter_chain`, which the codebase already uses for this situation:
a control-flow policy about how to react to bad reads, hoisted into a free function taking its
reads as closures, because the reads need a target and the answers to a bad read do not.

```rust
// remote_debugging::version — private, unit-tested in-file
fn detect_from_modules(
    modules: &[(String, usize)],
    read_image: impl Fn(&str) -> Option<Vec<u8>>,
    read_mem: impl Fn(u64, usize) -> Option<Vec<u8>>,
) -> Option<PythonVersion>
```

`read_version_from_process` loses its PID and becomes `version_word(read_mem, addr)` — the
8-then-4 fallback is policy about a short read, so it belongs inside the pure part, not in the
adapter.

`detect` keeps everything that is IO and everything that shapes the error message:
`find_python_modules`, the empty-modules bail, the closures over `std::fs::read` and
`reader::read_memory`, and the final `bail!` naming the pid. It reads as a page of setup and
one call.

**Keep the two-pass structure**, including that a module's bytes are read from disk once per
pass. Collapsing to one pass would change the rule in story 1. Caching the bytes across passes
would hold every Python module's image in memory at once, which for a framework build is tens
of megabytes per PID, and `list_pids` calls `detect` once per candidate PID. A memoizing
`read_image` adapter becomes a legal later change once the closure is the seam, but it is not
this spec.

**`Option`, not `Result`, from the pure function.** Every failure inside the loop is already
swallowed and retried against the next module; the only thing the caller needs to know is
whether anything was found, and the message it then produces is about the pid, which the pure
function does not have.

**Rejected — a `test-hooks` gate on `detect`.** ADR 0005 chose an honest public signal over a
gate for `PySession::layout_source()`, and the same applies: a private free function with its
own tests costs nothing at runtime and does not add a feature-flagged surface.

**Rejected — a trait for the two reads.** Two closures are the shape the codebase already
uses, and a trait would need an implementor in the production path that exists only to be
substituted in tests.

**Rejected — making `detect` take an injectable module list only.** Stubbing
`find_python_modules` alone leaves `fs::read` and `read_memory` live, so none of the
degenerate-read cases become reachable. The reads are the part worth substituting.

## 5. Seams and testing decisions

- **Seam:** `detect_from_modules` itself, unit-tested in-file. It is the highest seam that can
  observe the policy — above it sits only `detect`, which cannot be reached without a live
  process, which is the whole problem.
- **New seam needed:** yes, and this is it. Proposed at the highest point it can live: the
  policy loop, one level below the public entry point, private to the module, with no
  production caller other than `detect` and no feature flag.
- **What makes a good test here:** stub closures returning *specific* bytes, asserting the
  *decoded version*, not that detection succeeded. A test asserting `is_some()` passes when
  the wrong module wins, which is the failure this guards against. The image bytes can be the
  same literals the grammar tests already use (`b"junk\x003.10.16+\x00"`), so no fixture image
  is needed: `scan_image_for_version` falls back to scanning the whole buffer when no
  read-only section is found, which is the path arbitrary bytes take.
- **Prior art:** `session::walk_interpreter_chain` and its tests — same shape, same reason,
  and the closest thing the codebase has to a house style for this.
- **Cases:**
  1. Symbol beats scan across modules: module A's image spells `3.10.16`, module C's
     `Py_Version` reads `0x030d01f0` → `3.13.1`.
  2. A word decoding to major 4 is refused **and the scan phase still runs**, yielding the
     image's version.
  3. A zeroed word (`from_hex` → `None`) behaves the same way.
  4. An unreadable module (`read_image` → `None`) is skipped, and a later module wins.
  5. The 4-byte fallback, **both** of its triggers. (a) `read_mem(_, 8)` fails and
     `read_mem(_, 4)` returns the word. (b) `read_mem(_, 8)` *succeeds* and its word is one
     `from_hex` refuses, and the 4-byte read still runs. `read_version_from_process` falls
     through on either, and a version that only staged (a) would pass a refactor to "4 bytes
     only when the 8-byte read errors" — the behaviour change this case exists to catch.
  6. First match wins in module order, with two modules both publishing a valid symbol.
  7. Empty modules, and all modules failing, both yield `None` — and `detect`'s message still
     names the pid.
  8. Regression guard: `tests/remote_debugging.rs::detect_matches_the_interpreter_version`
     is unchanged and still the arbiter on the live matrix.

## 6. Out of scope

- **Any change to the detection rules.** This spec pins the current behavior; if a case turns
  out to be wrong, that is a bugfix spec and an ADR 0012 amendment, not a quiet fix inside a
  refactor.
- **Caching module bytes across the two passes, or across `detect` calls.** Reasoning above.
  Worth revisiting when the `list_pids` picker is profiled, not before.
- **The repeated `open_handle` per `read_memory` call.** `detect` opens a fresh process handle
  per read; the closure makes holding one trivial later, but that is an efficiency change with
  its own measurement.
- **`find_python_modules` and its ordering.** The policy consumes whatever order it is given;
  whether that order is the right one is a separate question, and
  [spec 0002](0002-pyruntime-module-discovery.md) is already open against that function.

## 7. Further notes

Best done after [0021](0021-version-grammar-module.md) and
[0022](0022-image-lookup-into-memory-binary.md): once the grammar and the image parsing have
moved out, what is left in `version/mod.rs` is `detect`, `version_word` and
`scan_image_for_version`'s two-region policy — which makes the extraction here almost the
whole remaining file, and easy to see whole.

Open question to settle when picked up: whether `scan_image_for_version` should also take its
region lookup as an argument, so "read-only section first, whole image second" is testable
without a fixture image. It is one more closure and the same argument. Against it:
that would be a *second* seam in a module about to have one, and
[0023](0023-unit-test-symbol-resolution.md) may cover the region lookup well enough from below.
