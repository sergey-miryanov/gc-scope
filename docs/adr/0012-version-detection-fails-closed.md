# 0012 — Version detection fails closed

**Status:** Accepted — decided 2026-08-04. (Applies
[ADR 0006](0006-layout-registration-integrity.md)'s posture one step upstream, at the
version that selects the layout. [ADR 0002](0002-version-split-runtime-finding.md)
consumes the version this decides how to obtain.)

## Context

The version selects the layout every later step decodes through. Two of its three sources
are exact and involve no parsing — the `Py_Version` symbol (3.11+) and the
`_Py_DebugOffsets` field the target publishes (3.13+). The third parses: below 3.11 nothing
exports the version, so `scan_for_version_string` reads the `PY_VERSION` literal out of raw
image bytes. That is the **sole** version source for 3.8/3.9/3.10 on every platform, and it
is fed arbitrary binary data.

Two forces make the parsing decisions sharp:

- **The hex is lossy at the source.** `patchlevel.h` packs the serial into four bits with a
  bitwise *or*: `(PY_RELEASE_LEVEL << 4) | PY_RELEASE_SERIAL`. A serial past `0xF` corrupts
  the level nibble rather than overflowing harmlessly — a 3.15.0b17 build reports its own
  `Py_Version` as `0x030f00b1`, i.e. **b1**, and an alpha 17 reports itself as a beta. That
  hex is a registered `LAYOUTS` row, so agreeing with the interpreter means decoding one
  build with another's offsets: the fails-open mode ADR 0006 guards, one step earlier than
  it guards.
- **The scanner competes with garbage.** Every accepted shape is another way for unrelated
  bytes to match before the real literal. `.rodata` holds paths, float literals and build
  tags — `v3.14.0-dirty` sits a few bytes from the literal in a 3.14 image.

[version-support §2](../version-support.md#2-knowing-which-version-you-are-looking-at)
states these forces; until this ADR nothing decided against that section.

## Decision

1. **An unrepresentable version is refused, not approximated.** A serial past `0xF` yields
   `None` and detection fails. Clamping (what the deleted `PythonVersion::from_string` did)
   names a `b15` that does not exist; mirroring `patchlevel.h` is the only way to agree with
   the interpreter, and resolves a *different registered build's* layout to get there. Being
   right or refusing beats agreeing.
2. **One grammar, and it is strict.** `parse_exact` accepts a bare fully-qualified `X.Y.Z`
   with an optional `aN`/`bN`/`rcN` suffix and nothing else. No permissive second parser is
   kept for hypothetical callers: two grammars drift, and these already had — the clamp
   existed in one and not the other, so the same interpreter yielded different versions
   depending on which path ran.
3. **Locating and parsing are separate jobs.** The scanner anchors, delimits a candidate by
   the run of characters the grammar can consume, and hands it over. Delimiting by character
   run rather than by grammar is what makes one parser sufficient — finding a candidate's end
   would otherwise walk the grammar a second time, which is how the duplication arose.
4. **A failed candidate advances exactly one byte**, never past the candidate: a real version
   can be glued to a bad one (`3.999.0-3.13.1`). Stated as an invariant because the natural
   optimization — skip to the candidate's end — reintroduces the C5 defect.
5. **The scanner only ever tightens.** Python 4, `X.Y` without a micro, and non-ASCII
   encodings are out of scope by policy. Widening needs a build that fails without it, not a
   shape that seems reasonable to accept.

## Consequences

- Detection fails loudly on an unrepresentable version instead of decoding as its neighbour.
  "Could not detect Python version" is the correct answer there, and an operator hitting it
  should read this ADR rather than reach for the clamp.
- The grammar has one implementation and one test suite; every rule above has a row in it,
  since a rule without one can drift back.
- The 3.8–3.10 live legs are the gate on ELF and Mach-O: no other version source exists
  there, and the tightening rules are sensitive to what the linker placed next to the
  literal. Windows was cleared at decision time — old and new scanner over the real `.rdata`
  and whole-image bytes of 35 installed CPython images, zero disagreements — and the other
  two formats rely on the matrix ([ADR 0005](0005-testing-strategy.md)).
