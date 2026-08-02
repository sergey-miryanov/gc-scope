# 0004 — Report `free_threaded` as information, not as a failed check

- **Status:** Not started, **pinned** (`a_free_threaded_flag_fails_only_that_check` in
  `offsets::mod`)
- **Kind:** bug — reporting
- **Effort:** S
- **Origin:** deferred plan of 2026-08-02, follow-up to
  [ADR 0006](../docs/adr/0006-layout-registration-integrity.md).
- **Respects:** [ADR 0006](../docs/adr/0006-layout-registration-integrity.md),
  [ADR 0011](../docs/adr/0011-layout-equivalence-sweep.md)

Not a correctness blocker — free-threaded builds decode correctly today and are covered by
a live CI leg. The damage is that `read-runtime` tells the operator a fully supported build
failed a check.

## 1. Problem

An operator running `read-runtime` against a free-threaded interpreter (`3.15t`) sees a `✗`
next to `free_threaded`, and the report withholds its `all checks passed ✓` summary. Both
are wrong: gcscope supports that build, decodes it correctly, and tests it on every OS. The
operator's reasonable conclusion — that their target is unsupported, or that a decode is
suspect — is the opposite of the truth.

## 2. Evidence

`impl_validate_debug_offsets!` in `offsets::validation` scores the flag as a check with an
expected value:

```rust
let ft_passed = off.free_threaded == 0;
// … Check::new("free_threaded", ft_passed, format!("{} (expected 0)", off.free_threaded))
```

The premise is recorded in the test that pins it — *"gcscope only decodes GIL builds
through these layouts; a free-threaded build reports `free_threaded = 1` and has a
different ABI, so it must be flagged."* That premise is wrong for `_Py_DebugOffsets`.

**CPython does not change the block's layout under `Py_GIL_DISABLED`.** In
`Include/internal/pycore_debug_offsets.h` (checked against the 3.15.0b4 tree) the macro
guards only the *values* assigned to fields, never the struct body:

```c
#ifdef Py_GIL_DISABLED
# define _Py_Debug_Free_Threaded 1
# define _Py_Debug_code_object_co_tlbc offsetof(PyCodeObject, co_tlbc)
# define _Py_Debug_interpreter_frame_tlbc_index offsetof(_PyInterpreterFrame, tlbc_index)
# define _Py_Debug_interpreter_state_tlbc_generation offsetof(PyInterpreterState, tlbc_indices.tlbc_generation)
#else
# define _Py_Debug_Free_Threaded 0
# define _Py_Debug_code_object_co_tlbc 0
# define _Py_Debug_interpreter_frame_tlbc_index 0
# define _Py_Debug_interpreter_state_tlbc_generation 0
#endif
```

The free-threading-only fields exist in a GIL build and hold `0`. A GIL-generated mirror
therefore reads a free-threaded block correctly, field for field.

**The rest of gcscope already treats free-threading as a build property to dispatch on:**

| Site | Behavior |
|---|---|
| `offsets::to_offset_table` | reads `free_threaded` out of the block and passes it on |
| `offsets::set_ring` / `expected_ring_size` | selects `[1,1,1]` entries per generation instead of `[11,3,3]` |
| `PySession::verify_ring_stats_size` | would hard-error if that selection were wrong |
| `.github/workflows/rust.yml` | `3.15t` is a live-smoke leg on all three OSes |

The entry counts match CPython (`Include/internal/pycore_interp_structs.h`):

```c
#ifdef Py_GIL_DISABLED
#define GC_YOUNG_STATS_SIZE 1
#define GC_OLD_STATS_SIZE 1
#else
#define GC_YOUNG_STATS_SIZE 11
#define GC_OLD_STATS_SIZE 3
#endif
```

So one site reports as a defect what four other sites treat as a supported configuration.

## 3. Scope

**Affected:** `read-runtime` only. `main` builds the report and prints it; nothing branches
on `checks[].passed`, so there is no exit-code or control-flow impact.

**Full tier only.** The check lives in `impl_validate_debug_offsets!`, invoked by
`v_3_15_0b1`, `v_3_15_0b1_gcinc`, `v_3_15_0b4` and `v_3_16_0a0`. The basic tier
(`validate_basic`, used by `V3_13_0` and `V3_14_0`) checks cookie and version only. That is
a second inconsistency worth resolving in the same change: a free-threaded 3.14 build
validates clean while a free-threaded 3.15 build does not, for reasons that have nothing to
do with either build.

**Not affected:** `gc-stats`, `monitor`, `run`, `tui`, `list-pids` — none of them call
`validate()`.

**Why CI misses it:** the `3.15t` leg asserts decoded shape through `gc-stats`, which never
builds a validation report. The one command that shows the wrong verdict is not on the
matrix path.

## 4. Proposed change

Report the flag as information rather than as a verdict.

1. **`offsets::validation`** — extend `Check` with a non-judgemental variant, or add an
   `info: Vec<(String, String)>` alongside `checks` on `ValidationReport`. Move
   `free_threaded` there, keeping its value visible.
2. **`ValidationReport`'s `Display`** — print info rows without `✓`/`✗` and exclude them
   from the `all_passed` computation, so a free-threaded build can reach
   `all checks passed ✓`.
3. **Rewrite the pinning test** — `a_free_threaded_flag_fails_only_that_check` becomes
   `a_free_threaded_flag_is_reported_without_failing`: assert the value appears in the
   report and that every check passes with `free_threaded = 1`. Delete the "different ABI"
   comment, which is the claim §2 disproves.
4. **Emit the same info row from `validate_basic`**, so both tiers report the flag
   identically and the 3.14-vs-3.15 inconsistency closes with it.

**Minimal alternative**, if the report structure is not worth touching: set `ft_passed`
unconditionally true and change the detail string to `"{} (0 = GIL, 1 = free-threaded)"`.
Cheaper, but it leaves a "check" that cannot fail — which is its own kind of noise, and
invites the next reader to delete it as dead.

## 5. Seams and testing decisions

- **Seam:** `VersionedOffsets::validate()` returning a `ValidationReport` — the existing
  public seam, already used by the pinning test. The report is a value, so the assertion is
  on data rather than on printed output.
- **New seam needed:** none. This is the ideal case the seam rule aims at: the behavior
  being changed is already observable at a level someone chose deliberately.
- **What makes a good test here:** assert the *report's* content — which rows exist, which
  are verdicts, and that `all_passed` holds with the flag set. Do not assert the rendered
  string; the `Display` formatting is presentation and will change again.
- **Prior art:** the existing validation tests in `offsets::mod` — `validate_basic_checks`
  and the per-check assertions around cookie and version, which already build a synthetic
  `_Py_DebugOffsets` and inspect the resulting checks.
- **Cases:**
  1. `free_threaded = 1` on a Full-tier layout: every check passes, the value is present in
     the report. Fails today — this is the pinned behavior being deliberately changed.
  2. `free_threaded = 0`: report content unchanged apart from the row's category.
  3. Basic tier reports the flag too, so 3.14 and 3.15 agree.
  4. Manual: `read-runtime` against a `3.15t` interpreter ends in `all checks passed ✓` and
     still shows the flag's value; against a GIL 3.15 build, output is unchanged apart from
     the row moving.

## 6. Out of scope

- **The ring geometry itself.** `[11,3,3]` and `[1,1,1]` stay hardcoded because CPython
  publishes no entry counts; only `gc.generation_stats_size` is published, and
  `verify_ring_stats_size` already cross-checks against it. The TUI's separate,
  *un*-cross-checked copy of that geometry is [spec 0005](0005-tui-ring-geometry-from-layout.md).
- **Whether free-threaded builds need their own registry entries.** They do not; §4 of
  [`docs/version-support.md`](../docs/version-support.md) records why the block's layout is
  shared, and the CI evidence is the `3.15t` leg.

## 7. Further notes

`Display for _Py_DebugOffsets` already prints `free_threaded`, and `read-runtime` prints
that dump immediately before the validation report — so the value is not lost under any of
the options above, including the minimal one.
