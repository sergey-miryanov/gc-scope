# 0006 — One version grammar: make the byte scanner delegate to `from_string`

**Status:** Not started
**Kind:** bug — drift
**Effort:** S
**Origin:** 2026-07-18 review (finding R2). The related C5 defect — the scan aborting on
its first bad candidate — has since landed; the duplication it rode on has not.
**Respects:** [ADR 0002](../docs/adr/0002-version-split-runtime-finding.md),
[ADR 0006](../docs/adr/0006-layout-registration-integrity.md)

## 1. Problem

gcscope parses a CPython version string two different ways, and the two have already
diverged: one clamps the pre-release serial into the nibble the version hex has room for,
the other does not. Which one runs depends on whether the `Py_Version` symbol resolved — so
the same interpreter can produce two different versions through two paths, and the version
is what selects the layout everything else decodes through. On a stripped or unusual build,
the operator gets *"Unsupported Python version"* for a version gcscope supports.

## 2. Evidence

`PythonVersion::from_string` clamps:

```rust
let serial = parse_digits(&mut chars)?;
(level, serial.min(0xF))
```

`scan_for_version_string` re-implements the same grammar over raw bytes and constructs the
struct inline with no clamp — its `serial` comes straight from `parse_micro_digits`, a
`u8`. A `3.15.0b17` string therefore yields `serial = 17`, which does not fit the 4-bit
serial field; the encoded hex matches no `LAYOUTS` row and no `ALIASES` row, so resolution
refuses (correctly, per ADR 0006's exact-or-refuse posture) a version that `from_string`
would have encoded as `0xF` and resolved.

Beyond the clamp, the scanner duplicates the whole suffix grammar (`a` / `b` / `rc`) and
the trailing-context rules. Any future grammar change has to be made twice or diverges
further.

The scanner is the **fallback** in `version::detect`, used when the `Py_Version` symbol
cannot be resolved — stripped binaries, unusual embedder builds. It is the path with the
least coverage and the most exposure to arbitrary bytes.

## 3. Scope

**Affected:** `version::detect`'s no-symbol fallback, on every command. The primary symbol
path reads the live `PY_VERSION_HEX` and is untouched.

**Not affected in practice, yet.** The divergence bites only on double-digit pre-release
serials, which mainstream CPython has not shipped. This is drift-prevention, and it is
cheap *because* nothing depends on the difference — which is also the argument for doing it
before something does.

**Why CI misses it:** every matrix interpreter has a resolvable `Py_Version` symbol, so the
scanner is never the path under test.

## 4. Proposed change

Keep the scanner's job as *locating candidates*; give the parsing to `from_string`.

1. Factor the per-candidate parse out of the scan loop into
   `fn try_parse_version_at(bytes: &[u8], i: usize) -> Option<PythonVersion>`. It finds the
   candidate's end using the existing trailing-context rules, builds a `&str` over that byte
   range, and returns `PythonVersion::from_string(s)`.
2. Reduce the loop to: find the next `3.`, try to parse it, and on failure advance by one
   and keep scanning. That preserves the property C5 established — a bad candidate advances
   the scan rather than aborting it — with one parser behind it instead of two.
3. Drop `#[allow(dead_code)]` from `from_string`; after this it has a non-test caller.
4. Delete `parse_micro_digits` if `from_string` subsumes every use; keep it if the
   candidate-boundary scan still needs it.

**The delimiting rules must not move.** The scanner requires a fully-qualified `X.Y.Z` (a
bare `3.1` is a false positive that would otherwise shadow the real `3.10.x`) and validates
the trailing byte against `\0`, space, `(`, newline, carriage return, tab, `"`. Those stay
in the *scanner*; `from_string` is deliberately permissive about trailing content and must
stay that way for its other callers.

## 5. Seams and testing decisions

- **Seam:** the existing in-crate unit tests over `scan_for_version_string` and
  `from_string`. Both are pure functions over bytes, which is the highest useful seam — the
  live seam above them (`version::detect`) cannot reach the scanner without a stripped
  interpreter, which CI cannot produce.
- **New seam needed:** none. `try_parse_version_at` is an internal helper; the assertions
  belong on the scanner's public behavior, not on the helper, so the refactor introduces no
  new test surface.
- **What makes a good test here:** assert the parsed `PythonVersion`, not the intermediate
  parse steps — the whole point is that two implementations must agree on the *result*.
  Table-drive it over the shapes `detect` actually sees, so the tests document the grammar
  rather than the code.
- **Prior art:** `from_string_parses_the_shapes_detect_actually_sees`,
  `from_string_rejects_overflowing_component`, and the existing scanner tests in
  `remote_debugging::version` — this spec makes the two suites assertions about one grammar
  instead of two.
- **Cases:**
  1. Double-digit serial (`"3.15.0b17\0"`): scanner and `from_string` agree on
     `serial == 0xF`. Fails today — this is the divergence.
  2. Malformed candidate preceding the real version (`"3.999.0 … 3.13.1\0"`) still returns
     `3.13.1` — pins the C5 property through the refactor.
  3. A bare `"3.1 "` does not shadow a later `"3.10.4\0"` — pins the fully-qualified rule.
  4. Every existing case in both suites passes unchanged; they encode the shapes `detect`
     actually sees and are the regression guard.
  5. Live matrix green — confirms the symbol path was not disturbed.

## 6. Out of scope

Widening what the scanner accepts: Python 4, `X.Y` without a micro, non-ASCII encodings.
The goal is one grammar, not a more permissive one — and a scanner over arbitrary binary
data should get *less* permissive, not more, since every extra accepted shape is another
way to match garbage before the real version string.

## 7. Further notes

Worth checking while in here whether `detect`'s fallback is reachable at all on the
platforms gcscope ships to, or whether it is dead weight kept for embedder builds nobody
has tested. If it is genuinely unreachable, deleting it is a better answer than unifying
it — but that needs evidence this spec does not have.
