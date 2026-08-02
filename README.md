# gcscope — CPython process memory analysis

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/sergey-miryanov/gc-scope?utm_source=badge)
[![codecov](https://codecov.io/gh/sergey-miryanov/gc-scope/graph/badge.svg)](https://codecov.io/gh/sergey-miryanov/gc-scope)

Cross-platform CLI tool for reading and analyzing CPython process memory.

## Commands

| Command | Description |
|---|---|
| `gcscope list <PID>` | List memory regions |
| `gcscope read <PID> <ADDR> <SIZE>` | Hex dump remote memory |
| `gcscope find-runtime <PID>` | Locate the `_PyRuntime` global |
| `gcscope read-runtime <PID>` | Read and display `_Py_DebugOffsets` |

`-1` can be used as PID to target the current process.

## Attaching: per-platform permissions

gcscope reads another process's memory, which every OS gates differently.

| Platform | Requirement |
|---|---|
| **Windows** | None for a process you own. |
| **Linux** | Same-uid works when the target is a descendant. Otherwise loosen Yama: `sudo sysctl -w kernel.yama.ptrace_scope=0`. |
| **macOS** | Depends on the target: **Python 3.13+ attaches unprivileged**; **3.8–3.12 needs `sudo gcscope …`**. |

On macOS the split is the target's doing: framework builds are signed with a
hardened runtime, and only 3.13+ ships `com.apple.security.get-task-allow` (with
PEP 768), which is what admits a same-user caller. Granting
`system.privilege.taskport` does not help. If you don't know the target's version,
use `sudo`. The entitlement arrived with the same change that added the runtime
marker and the self-published offsets — see
[PEP 768](docs/version-support.md#pep-768-the-interpreter-starts-describing-itself)
in the version-support notes.

To avoid `sudo` there, sign gcscope with `com.apple.security.cs.debugger` (the
`gcscope.entitlements` file in the repo root grants it) — the mechanism LLDB's
`debugserver` uses:

```bash
codesign -s "<your-cert>" --entitlements gcscope.entitlements -f target/release/gcscope
```

The certificate must be trusted for code signing — a **self-signed** one in the
System keychain works, no Apple Developer account needed — and ad-hoc
`codesign -s -` will **not** do, since restricted entitlements are ignored on
ad-hoc signatures. Re-sign after every build; `cargo build` replaces the binary.

## Testing

The `.gc-gen-3.15+inc` venv provides a custom 3.15+rc Python build
for testing. Since venv launchers are child processes,
`find-runtime` and `read-runtime` use `remoteprocess` to
recursively search children of the target PID.

## Supported Python versions

For *why* support works this way — what changes across CPython versions, and what
each change forces — see [`docs/version-support.md`](docs/version-support.md).

Generated offset structs are in `src/remote_debugging/offsets/`. One module per distinct
**layout**, not per version — several releases usually share one:

| Module | Hex | Also describes |
|---|---|---|
| `v_3_13_0` | `0x030d00f0` | 3.13.1 – 3.13.14 (verified); later 3.13.x by fallback |
| `v_3_14_0` | `0x030e00f0` | 3.14.1 – 3.14.6 (verified); later 3.14.x by fallback |
| `v_3_15_0b1` | `0x030f00b1` | 3.15.0b2, 3.15.0b3 (verified) |
| `v_3_15_0b1_gcinc` | `0x030f00b1` | the `+inc` GC layout for that hex (see below) |
| `v_3_15_0b4` | `0x030f00b4` | — |
| `v_3_16_0a0` | `0x031000a0` | — (ongoing dev build, provenance-pinned) |

A build resolves on one of three tiers, strongest evidence first:

1. **Exact** — its hex has a module.
2. **Verified alias** — `gen-offsets.py --sweep` proved its layout identical to a
   registered one. Proof, so it reports as a full match, and the only way a **pre-release**
   without its own module resolves. Every shipped 3.13.x/3.14.x release is on this tier.
3. **Same-minor fallback** — an unregistered *final* borrows its minor's anchor, on
   CPython's patch-freeze convention. An assumption, so it warns.

Anything else is refused rather than approximated — including 3.15.0a7 and 3.15.0a8, both
superseded alphas with layouts of their own.

All pre-3.13 versions (3.8–3.12) use hardcoded tables in `pre_3_13.rs`; see
[ADR 0010](docs/adr/0010-pre-3-13-offsets-stay-hand-maintained.md) for why those are not
generated.

### Builds that share a version hex (multi-candidate GC layout)

A clean release and a GC-instrumented `+inc` build can share a `PY_VERSION_HEX` and an
identical `_Py_DebugOffsets`, differing only in the per-entry `gc_generation_stats` struct.
For those, `GC_CANDIDATES` (in `offsets/mod.rs`) registers each candidate GC layout for the
hex, and `select_gc_shape` picks the right one at read-time by the process-published
`generation_stats_size` (total ring bytes). Candidates for one hex must have distinct ring
sizes — the only out-of-process discriminator — enforced by a test. Example:
`0x030f00b1` serves both clean 3.15.0b1 (64-byte stats) and the `gc-gen-3.15+inc` build
(208-byte stats).

Why the version hex is an incomplete key, and why a colliding pair has to be refused
rather than guessed between, is in
[Build configuration varies independently of version](docs/version-support.md#build-configuration-varies-independently-of-version).

## How offset tables work

gcscope needs to know the byte offsets of fields within `_PyRuntime`,
`PyInterpreterState`, `PyThreadState`, and the GC state. These offsets
change between every Python minor version (3.8 → 3.9 → … → 3.16).
There are two ways to obtain them — what each mechanism compiles in versus reads
from the process is laid out in
[Deciding which layout describes the build](docs/version-support.md#4-deciding-which-layout-describes-the-build):

### 1. Hardcoded tables (3.8–3.12)

For versions before `_Py_DebugOffsets` existed, the offsets are
extracted from CPython headers by hand and stored in
`src/remote_debugging/offsets/pre_3_13.rs`. Each version needs ~7
field offsets. GC generation stats are read for all of 3.8–3.12
(the inline-array layout is identical to 3.13/3.14); 3.9–3.12 have
per-interpreter GC state, while 3.8 keeps its GC state global in
`_PyRuntime` and is decoded through the stats loop's global-GC
branch. The diagram/TUI is unavailable for these versions (it
visualizes the `_Py_DebugOffsets` struct, which they lack).

### 2. Bindgen-generated struct (3.13+, full support)

`scripts/gen-offsets.py` uses `bindgen` (Rust FFI bindings) to
generate a complete `#[repr(C)]` Rust struct that mirrors the C
`_Py_DebugOffsets` type. The process:

```
CPython checkout
      │
      ▼
gen-offsets.py ──► reads patchlevel.h (version hex)
      │
      ├─ 3.14+: includes pycore_debug_offsets.h
      │          in a wrapper header
      │
      └─ 3.13.x: extracts the _Py_DebugOffsets struct
                 text from pycore_runtime.h and wraps
                 it with uint64_t typedef and _Py_NONSTRING
      │
      ▼
    wrapper.h  ──► bindgen parses it, resolves struct
    (temp)          layout, and emits a Rust struct with
                    offset_of! compile-time checks
      │
      ▼
    v_{version}.rs   ──► #[repr(C)] struct + field accessors
      │
      ├─ 3.15+: also appends impl_display_debug_offsets!
      │          and impl_validate_debug_offsets! macros
      │          for hex-dump and validation support
      │
      └─ 3.13–3.14: skips the macros (struct has fewer
                     sub-structs); basic display/validate
                     are provided by mod.rs
```

The generated struct is then read **from the target process at
runtime** — gcscope reads `_Py_DebugOffsets` bytes from the running
Python process and casts them through the generated Rust struct to
get the actual offset values for that specific build.

#### How the wrapper header works

For 3.14+, the wrapper is trivial:

```c
typedef unsigned long long uint64_t;
#define _Py_NONSTRING
#include "path/to/pycore_debug_offsets.h"
```

For 3.13.x, `pycore_debug_offsets.h` doesn't exist — the struct is
inside `pycore_runtime.h`. Parsing the full header with bindgen would
require resolving dozens of internal dependencies, so the script
**extracts just the `_Py_DebugOffsets` struct text** (lines 97–101)
and wraps it as a standalone unit:

```c
typedef unsigned long long uint64_t;
#define _Py_NONSTRING
#define _Py_Debug_Cookie "xdebugpy"
#pragma pack(push, 8)
// pasted struct definition
#pragma pack(pop)
```

## Adding a new Python version

**Full support** (bindgen — recommended for production releases):

This is a maintainer-only step — building, testing, and running gcscope need none of it,
since the generated `v_*.rs` are checked in. `gen-offsets.py` shells out to a `bindgen`
binary on PATH; it is not a crate dependency.

**Check first whether a new module is needed at all.** Most releases share a layout with one
already registered and need only an alias row. Ask the sweep:

```powershell
python scripts/gen-offsets.py --sweep X:/path/to/cpython-trees --tags-only --emit-aliases
```

It groups every tree by layout and prints the `ALIASES` and `VERIFIED_GC_SIZES` tables for
`offsets/mod.rs` (**merge** them — the rows describe only the trees you swept). Generate a
module only for a build the sweep reports as a genuinely new layout. Background:
[What the ABI freeze does and does not promise](docs/version-support.md#what-the-abi-freeze-does-and-does-not-promise)
and [ADR 0011](docs/adr/0011-layout-equivalence-sweep.md).

```powershell
# One-time: install the bindgen CLI (puts `bindgen` on PATH) and point LIBCLANG_PATH
# at the VS-bundled LLVM.
cargo install bindgen-cli --version 0.72
$env:LIBCLANG_PATH = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin"

# Generate bindgen struct
python scripts/gen-offsets.py X:/path/to/cpython
```

The generator prints an **exact registration checklist to stderr** — follow it. It refuses
to overwrite an existing `v_*.rs` (use `--force` to regenerate a build in place). The
generated `v_*.rs` is self-contained (it carries its own `impl DebugOffsetsView` with all
version-varying offsets and the GC-stats shape), so registering it in
`src/remote_debugging/offsets/mod.rs` is ~9 lines and every site except the `LAYOUTS` row
is compiler-enforced (a forgotten site fails to build):

```diff
  // 1. module decl, with the other `mod v_*;`
+ mod v_3_15_0;
  // 2. LAYOUTS row (hex → struct reader) — the ONLY non-compiler-enforced site
+ (0x030f00f0, |p, a| Ok(VersionedOffsets::V3_15_0(read_struct(p, a)?))),
  // 3. VersionedOffsets enum variant
+ V3_15_0(v_3_15_0::_Py_DebugOffsets),
  // 4. for_each_variant! arm — drives most accessors + the trait delegation automatically
+ Self::V3_15_0($o) => $body,
  // 5. gc_debug_fields arm — the one accessor NOT driven by for_each_variant!
+ Self::V3_15_0(_) => build(offset_of!(v_3_15_0::_Py_DebugOffsets, gc),
+                           size_of::<v_3_15_0::_Py_DebugOffsets__gc>()),
  // 6. validate() arm   — validate_basic(o, expected)  OR  v_3_15_0::validate_offsets(o, expected)
  // 7. Display arm       — fmt_debug_offsets_basic(o, f) OR  fmt::Display::fmt(o, f)
  // 8. basic tier only  — add to the impl_basic_display! and impl_basic_offsets! lists
```

Step 5 (`gc_debug_fields`) is a hand-written `match` — not a `for_each_variant!` accessor —
because it computes the `gc` sub-struct's field offsets from each build's own struct types
via `offset_of!`/`size_of!`; it drives the diagram's GC-state subtree. It's still
compiler-enforced (the `match` is exhaustive). Basic vs full tier (steps 6–8) is decided by
the sub-struct count (`>= 21` → full, with generated `validate_offsets`/`Display`); the
generator's checklist tells you which applies.

**Same-hex second build** (a clean release vs a gc-instrumented `+inc` build sharing a
`PY_VERSION_HEX` — see "Builds that share a version hex" above). If the `+inc` build doesn't
bump `patchlevel.h`, its version alone can't distinguish it, so pass an explicit name:

```powershell
python scripts/gen-offsets.py X:/path/to/cpython-+inc --suffix gcinc
```

This writes `v_<version>_gcinc.rs` instead of clobbering the clean file, and (because the
hex is already registered) the checklist prints the *same-hex* path: just a `mod` decl plus
one `GC_CANDIDATES` row — no new enum variant, `LAYOUTS` row, or accessor arms. `cargo test`
then enforces that the candidates have distinct ring sizes (the only out-of-process
discriminator).

### Version hex reference

`PY_VERSION_HEX` comes from CPython's `patchlevel.h`. Its bit layout, and why the
release-level nibble matters for layout selection, are in
[`docs/version-support.md`](docs/version-support.md#2-knowing-which-version-you-are-looking-at).

To find the hex for any checkout:
```powershell
python scripts/gen-offsets.py X:/path/to/cpython --stdout | grep version_hex
```
