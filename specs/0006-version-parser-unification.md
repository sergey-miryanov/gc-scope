# 0006 — One strict version grammar: delete `from_string`, leave the scanner the only parser

- **Status:** Not started. Re-scoped 2026-08-04 — the original framing rested on a false
  premise (see §7).
- **Kind:** bug — drift
- **Effort:** S
- **Origin:** 2026-07-18 review (finding R2). The related C5 defect — the scan aborting on
  its first bad candidate — has since landed; the duplication it rode on has not.
- **Respects:** [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
  [ADR 0006](../docs/adr/0006-layout-registration-integrity.md)

## 1. Problem

gcscope parses a CPython version string two different ways, and the two have already
diverged: `PythonVersion::from_string` clamps a pre-release serial into the nibble the
version hex has room for, and `scan_for_version_string` does not. The version is what
selects the layout everything else decodes through, so the operator's exposure is a
`gc-stats` or `tui` run that reports *"Unsupported Python version"* for a version gcscope
supports.

Nothing an operator can run today reaches the divergence — it needs a double-digit
pre-release serial, which CPython has never shipped. What makes this worth doing anyway is
where the duplicated grammar lives: `scan_for_version_string` is not the rarely-taken
fallback the original spec assumed, it is the **only** version source for CPython 3.8, 3.9
and 3.10 on every platform (§2). A second, unexercised copy of the grammar sitting beside a
load-bearing one is the drift risk, not the serial itself.

## 2. Evidence

**The scanner is the sole version source below 3.11.** `Py_Version` was added to the C API
in 3.11, as recorded in the `tests/remote_debugging.rs` module doc comment and in
`docs/version-support.md` ("3.11+: an exported variable"). `version::detect` tries
`resolve_symbol_in_bytes(…, "Py_Version")` over every module first and only then falls to
the string scan, so on 3.8/3.9/3.10 the first loop always comes up empty and every attach —
`gc-stats`, `monitor`, `run`, `list-pids`, `tui` — takes the scanner. Both pre-3.11 traces
in `docs/attach-traces.md` show exactly that sequence (`Py_Version symbol → absent (added in
3.11)`, then `string scan → …`).

**The two grammars.** `PythonVersion::from_string` clamps:

```rust
let serial = parse_digits(&mut chars)?;
(level, serial.min(0xF))
```

`scan_for_version_string` re-implements the same grammar over raw bytes — the `a`/`b`/`rc`
suffixes, the digit runs, the component boundaries — and builds the struct inline with the
serial taken straight from `parse_micro_digits`, unclamped. `"3.15.0b17"` therefore yields
`serial = 17` from the scanner and `serial = 15` from `from_string`.

**Neither answer is the interpreter's.** `patchlevel.h` packs the hex with a bitwise *or*,
not a sum: `(PY_RELEASE_LEVEL << 4) | PY_RELEASE_SERIAL`. For beta 17 that is
`0xB0 | 0x11 = 0xB1` — a real 3.15.0b17 interpreter's own `Py_Version` reads `0x030f00b1`
and reports itself as **b1**, which is a live `LAYOUTS` row. For alpha 17 it is
`0xA0 | 0x11 = 0xB1`: an alpha that reports itself as a beta. The encoding is lossy at the
source, so "make the paths agree" is unachievable except by reproducing the corruption —
which would hand a b17 build the b1 layout and decode it silently, the failure mode
[ADR 0006](../docs/adr/0006-layout-registration-integrity.md) exists to prevent.

**`from_string` has no callers.** Its `#[allow(dead_code)]` is load-bearing: every use in
the crate is a test. The permissive behaviors it documents — `trim`, stripping `"Python "`,
an optional micro, trailing content — exist for a `python --version` consumer that was never
built.

## 3. Scope

**Affected:** `version::detect`'s string path, on every command. That is the sole version
source on **3.8, 3.9 and 3.10** across Linux, Windows and macOS, plus any stripped or
embedder build on any version.

**Not affected:** the `Py_Version` symbol path (3.11+), which reads a live `PY_VERSION_HEX`
and never sees a string; and the 3.13+ published-field path, where the process replaces the
detected version before layout resolution.

**What CI covers and what it does not.** The live matrix's 3.8–3.10 legs are pure scanner
tests on three platforms, so the scanner's *mainline* behavior is well covered — the
original spec's "the scanner is never the path under test" was wrong. What the matrix does
not cover: any pre-release string through the scanner (the matrix ships no pre-release below
3.11, and 3.15.0bN resolves through the symbol), and the whole-image fallback in `detect`
that runs when the read-only data section holds no match.

## 4. Proposed change

One grammar, and it is the strict one. The scanner keeps its job — *locating* candidates and
delimiting them — and gains the only parser in the module.

1. **Delete `PythonVersion::from_string` and `parse_micro_digits`.** Nothing outside tests
   calls either once step 3 lands. Deleting the permissive parser rather than keeping it
   beside the strict one is what actually removes the drift; keeping it would leave two
   grammars with a mode flag between them.
2. **Add the strict parser** — a private `parse_exact(&str) -> Option<PythonVersion>` in
   `remote_debugging::version`, built on the existing `parse_digits` and its checked `u8`
   arithmetic. It requires a fully-qualified `X.Y.Z`, accepts an optional `aN` / `bN` / `rcN`
   suffix, and must consume the **entire** input. No `trim`, no `"Python "` strip, no
   optional micro. A serial that does not fit the 4-bit field is **rejected**, not clamped:
   gcscope refuses to name a build it cannot represent, per §2.
3. **Rewrite `scan_for_version_string`** around it:
   - anchor on `3` followed by `.`;
   - reject the anchor if the preceding byte is a **version character** — `[0-9 . a b c r]`,
     the set the grammar can consume — because then the `3` is inside a longer token;
   - take the run of version characters starting at the anchor. It is ASCII by construction,
     so `str::from_utf8` over it cannot fail;
   - `parse_exact` the run, and require the byte at the run's end to be a terminator — NUL,
     space, `(`, `"`, newline, carriage return, tab — or end-of-buffer;
   - on any failure advance by **exactly one byte** and keep scanning.
4. **Drop `#[allow(dead_code)]` from `parse_digits`**, which `parse_exact` now reaches.
   `to_hex` keeps its; it is still test-only.
5. **Update the two docs that describe the old rules:** the `specs/README.md` row for this
   spec (it states the superseded problem) and the scanner's rule list in
   `docs/version-support.md`, whose "the `3` must not follow a digit" becomes the wider
   version-character guard and whose implied permissive parser no longer exists.

### Two deliberate behavior changes

Both tighten the scanner, which §6 argues is the right direction for a parser fed arbitrary
binary data.

- **The anchor guard widens from "not a digit" to "not a version character."** Today's
  `i = j` advance after a failed terminator check happens to skip inner candidates; the
  mandatory one-byte advance (below) does not, so `"3.13.1a3.12.0\0"` would newly be
  accepted as `3.12.0` without this. The wider guard restores today's `None` and also
  rejects `"1.3.12.0\0"`, which today's guard lets through.
- **The advance is always one byte.** `scan_for_version_string` currently advances to the
  parsed token's end on a terminator failure. That must go: with the token delimited by the
  version-character run, advancing past a failed candidate would skip a real version glued to
  it — `"3.999.0-3.13.1\0"` finds `3.13.1` today only because the overflow failure advances
  by one.

### Why the rejected alternatives lose

- *Clamp to `0xF` (the original spec's target).* Reports `b15`, a build that does not exist,
  in the error the operator reads. Neither refusal nor truth.
- *Mirror `patchlevel.h`.* Achieves genuine cross-path agreement and is the only option that
  matches what the interpreter says about itself — by reproducing a lossy encoding that
  resolves b17 to the b1 layout and decodes with it. Silent wrong offsets beat no answer only
  if you never read the answer.
- *Keep `from_string` and give it a strict mode.* One grammar with two modes is still two
  behaviors to keep in step, and the permissive mode has no caller to justify it.
- *Use `Display` as the boundary oracle* — parse a window, then require the bytes to equal the
  rendering. Elegant, and the micro rule falls out free, but it couples 3.8–3.10 attach to a
  formatting function; a well-meaning change to `Display` would break detection with nothing
  but a red live-matrix leg to say so.

## 5. Seams and testing decisions

- **Seam:** the existing in-crate unit tests over `scan_for_version_string`, extended, plus
  the `from_string` suite retargeted onto `parse_exact`. Both are pure functions over
  bytes/`&str`. The seam above them (`version::detect`) needs a real interpreter, and the
  live matrix already exercises it on every 3.8–3.10 leg.
- **New seam needed:** none.
- **What makes a good test here:** assert the decoded `PythonVersion`, table-driven over the
  shapes `detect` actually sees plus the shapes that must be refused, so the table documents
  the grammar rather than the code. Every rule the rewrite introduces gets a row; a rule with
  no row is a rule that can drift back.
- **Prior art:** `scan_finds_an_embedded_fully_qualified_version`,
  `scan_skips_a_version_without_a_micro_and_keeps_looking`,
  `from_string_rejects_overflowing_component`, `display_round_trips_from_string`.
- **Cases:**
  1. **Serial overflow is refused.** `b"3.15.0b17\0"` → `None`, and `parse_exact("3.15.0b17")`
     → `None`. This is the divergence, and the answer is refusal on both sides of it.
  2. **The C5 property, sharpened.** `b"3.999.0-3.13.1\0"` → `3.13.1`. The hyphen matters:
     the original spec's space-separated form passes even with a broken advance, because the
     space terminates the failed candidate.
  3. **Past a charset-delimited failure.** `b"3.12.0zzz3.13.1\0"` → `3.13.1`.
  4. **The tightened guard.** `b"3.13.1a3.12.0\0"` → `None` (matches today, via the new rule)
     and `b"1.3.12.0\0"` → `None` (newly refused).
  5. **A realistic rodata false positive.** `b"libpython3.13.so.1.0"` → `None`: the run stops
     at `s`, leaving `"3.13."`, which has no micro.
  6. **End-of-buffer terminates.** `b"3.12.0"` with no trailing byte → `3.12.0`.
  7. **The fully-qualified rule and the terminator rule hold**, unchanged:
     `b"3.13 then 3.13.4 "` → `3.13.4`; `b"3.12.0z"` → `None`; `b"3.12.0\""` and `b"3.12.0("`
     → `3.12.0`; `b"lib13.12.0"` → `None`.
  8. **Three cases invert, deliberately.** `"Python 3.11.0"`, `"3.12.0 (tags/v3.12.0, …)"`
     and `"3.12"` were accepted by `from_string` and are refused by `parse_exact` — they move
     from the accept table to the reject table. The rest of that suite carries over.
  9. **Live: 3.8, 3.9, 3.10.** Baseline the detected version on each before the change,
     re-run after, require identical output. These are the versions that have no other source.
  10. **Live: real read-only data.** A throwaway scratch binary extracts `.rdata` from every
      installed `python3*.dll` and runs the old and new scanner over the real bytes, asserting
      identical results. This is the only evidence that can disprove the tightened guard's one
      real risk — a genuine `PY_VERSION` literal preceded by a version character. Not
      committed.
  11. **Live matrix green** — confirms the symbol path (3.11+) was not disturbed.

## 6. Out of scope

Widening what the scanner accepts: Python 4, `X.Y` without a micro, non-ASCII encodings. The
goal is one grammar, not a more permissive one — every extra accepted shape is another way to
match garbage before the real version string, and this parser is fed arbitrary binary data.

Also out of scope: whether `detect`'s whole-image fallback scan (tried when the read-only
data section yields no match) is still needed. It is untested and unmeasured; deciding it
needs evidence this spec does not have.

No ADR. The refusal stance follows from ADR 0006's exact-or-refuse posture rather than
extending it.

## 7. Further notes

**What the re-scope changed.** The original spec described the scanner as "the fallback …
the path with the least coverage", said "every matrix interpreter has a resolvable
`Py_Version` symbol, so the scanner is never the path under test", and asked in §7 whether
the fallback was reachable at all — proposing deletion as the better answer if not. All
three rest on the same error. `Py_Version` arrives in 3.11; the scanner is the only version
source below it, exercised by CI on nine matrix legs, and deleting it would drop support for
a third of the version range. That question is now answered and the section is gone.

The original §4 also proposed keeping `from_string` and delegating to it from a
`try_parse_version_at` helper that "finds the candidate's end using the existing
trailing-context rules". Finding a candidate's end *is* walking the grammar, so that shape
would have left the duplication in place and moved only the struct construction. Bounding the
candidate by the version-character run is what makes a single parser sufficient.
