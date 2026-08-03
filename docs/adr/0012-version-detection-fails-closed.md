# 0012 — Version detection fails closed

**Status:** Accepted — decided 2026-08-04. (Applies [ADR 0006](0006-layout-registration-integrity.md)'s
posture one step upstream, at the version that selects the layout. Complements
[ADR 0002](0002-version-split-runtime-finding.md), which consumes the version this
decides how to obtain.)

## Context

The version resolves before anything else dispatches on it, and it is what selects the
layout every later step decodes through. Two of the three sources are exact — the
`Py_Version` symbol (3.11+) and the `_Py_DebugOffsets` field the target publishes about
itself (3.13+) — and neither involves parsing. The third does: below 3.11 nothing exports
the version, so the only record is the `PY_VERSION` string literal in the image, and
`scan_for_version_string` reads it out of raw bytes. That path is the **sole** version
source for 3.8, 3.9 and 3.10 on every platform, and it is fed arbitrary binary data.

Three forces make the parsing decisions sharp:

- **The hex is lossy at the source.** `patchlevel.h` packs the release serial into four
  bits with a bitwise *or*: `(PY_RELEASE_LEVEL << 4) | PY_RELEASE_SERIAL`. A serial past
  `0xF` does not overflow into nothing — it corrupts the level nibble. A hypothetical
  3.15.0b17 build reports its own `Py_Version` as `0x030f00b1`, i.e. **b1**; an alpha 17
  reports itself as a **beta** 1. So the string and the symbol cannot be made to agree on
  such a build except by reproducing the corruption.
- **`0x030f00b1` is a real `LAYOUTS` row.** Reproducing the corruption therefore does not
  produce a harmless wrong label — it resolves a *different, registered* build's layout and
  decodes with it. That is precisely the fails-open mode [ADR 0006](0006-layout-registration-integrity.md)
  exists to prevent, arriving one step earlier than ADR 0006 guards.
- **The scanner competes with garbage.** Every shape it accepts is another way for
  unrelated bytes to match *before* the real literal — a `.rodata` section holds paths,
  build tags and float literals that look like versions. `v3.14.0-dirty` sits a few bytes
  from the literal in a 3.14 image.

The forces are stated in [version-support §2](../version-support.md#2-knowing-which-version-you-are-looking-at);
until this ADR that section had no decision recorded against it.

## Decision

1. **A version that cannot be represented is refused, not approximated.** A release serial
   past `0xF` yields `None`, and detection fails with "could not detect" rather than
   returning a neighbour. The two alternatives both lose:
   - *Clamp to `0xF`* (the behavior the deleted `PythonVersion::from_string` had) names a
     `b15` build that does not exist, in the error an operator reads.
   - *Mirror `patchlevel.h`* achieves genuine agreement with what the interpreter says about
     itself, and is the only option that does — by resolving a registered layout for a
     different build and decoding silently. Agreement is not the goal; being right or
     refusing is.
2. **One grammar, and it is strict.** A single parser (`parse_exact`) accepts a bare
   fully-qualified `X.Y.Z` with an optional `aN`/`bN`/`rcN` suffix and nothing else — no
   leading `Python `, no surrounding whitespace, no trailing content, no optional micro. A
   permissive second parser is not kept "for other callers": two grammars drift, and this
   one already had — the clamp existed in one and not the other, so the same interpreter
   could produce two different versions depending on which path ran.
3. **Locating and parsing are separate jobs.** The scanner anchors, delimits a candidate by
   the run of characters the grammar can consume, and hands that run to the parser; it does
   not parse. Delimiting by the character run rather than by the grammar is what makes one
   parser sufficient — finding a candidate's end would otherwise mean walking the grammar a
   second time, which is how the duplication arose.
4. **A failed candidate advances the scan by exactly one byte**, never past the candidate. A
   real version can be glued to a bad one (`3.999.0-3.13.1`), and skipping the failure skips
   the version. This restates the C5 fix as an invariant rather than an incident, because the
   natural "optimization" is to advance to the candidate's end.
5. **The scanner gets less permissive over time, never more.** Python 4, `X.Y` without a
   micro, and non-ASCII encodings are out of scope by policy, not by omission. A widening
   needs a build that actually fails without it — not a shape that seems reasonable to accept.

## Consequences

- Detection fails loudly on an unrepresentable version instead of quietly decoding as its
  neighbour. On a build gcscope genuinely cannot name, "could not detect Python version" is
  the correct answer, and an operator hitting it should read this ADR rather than reach for
  the clamp.
- The grammar has one implementation, so a future change to it is made once. The unit tests
  in `remote_debugging::version` are assertions about that one grammar; each rule in
  decisions 1–4 has a row, since a rule without one can drift back.
- Decision 4 is a constraint on the scan loop, not a note. Any rewrite that advances by more
  than a byte must first show that no version can follow a failed candidate — which the
  `3.999.0-3.13.1` case disproves.
- The 3.8–3.10 live legs are the gate for this path on ELF and Mach-O, since it has no other
  version source there and the tightening rules are sensitive to what the linker put next to
  the literal. Windows PE was cleared at decision time by running the pre- and post-change
  scanners over the real `.rdata` and whole-image bytes of 35 installed CPython images with
  zero disagreements; the other two formats rely on the matrix
  ([ADR 0005](0005-testing-strategy.md)).
