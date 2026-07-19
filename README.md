# gcscope — CPython process memory analysis

Cross-platform CLI tool for reading and analyzing CPython process memory.

## Commands

| Command | Description |
|---|---|
| `gcscope list <PID>` | List memory regions |
| `gcscope read <PID> <ADDR> <SIZE>` | Hex dump remote memory |
| `gcscope find-runtime <PID>` | Locate the `_PyRuntime` global |
| `gcscope read-runtime <PID>` | Read and display `_Py_DebugOffsets` |

`-1` can be used as PID to target the current process.

## Testing

The `.gc-gen-3.15+inc` venv provides a custom 3.15+rc Python build
for testing. Since venv launchers are child processes,
`find-runtime` and `read-runtime` use `remoteprocess` to
recursively search children of the target PID.

## Supported Python versions

Generated offset structs are in `src/remote_debugging/offsets/`. Currently supported:

| Version | Hex | Method |
|---|---|---|
| 3.13.1 | `0x030d01f0` | bindgen |
| 3.13.13+ | `0x030d0df0` | bindgen |
| 3.14.4 | `0x030e04f0` | bindgen |
| 3.15.0a8 | `0x030f00a8` | bindgen |
| 3.15.0b1 | `0x030f00b1` | bindgen |
| 3.15.0b3 | `0x030f00b3` | bindgen |
| 3.16.0a0 | `0x031000a0` | bindgen |

All pre-3.13 versions (3.8–3.12) use hardcoded tables in `pre_3_13.rs`.

### Builds that share a version hex (multi-candidate GC layout)

A clean release and a GC-instrumented `+inc` build can share a `PY_VERSION_HEX` and an
identical `_Py_DebugOffsets`, differing only in the per-slot `gc_generation_stats` struct.
For those, `GC_CANDIDATES` (in `offsets/mod.rs`) registers each candidate GC layout for the
hex, and `select_gc_shape` picks the right one at read-time by the process-published
`generation_stats_size` (total ring bytes). Candidates for one hex must have distinct ring
sizes — the only out-of-process discriminator — enforced by a test. Example:
`0x030f00b1` serves both clean 3.15.0b1 (64-byte stats) and the `gc-gen-3.15+inc` build
(208-byte stats).

## How offset tables work

gcscope needs to know the byte offsets of fields within `_PyRuntime`,
`PyInterpreterState`, `PyThreadState`, and the GC state. These offsets
change between every Python minor version (3.8 → 3.9 → … → 3.16).
There are two ways to obtain them:

### 1. Hardcoded tables (3.8–3.12)

For versions before `_Py_DebugOffsets` existed, the offsets are
extracted from CPython headers by hand and stored in
`src/remote_debugging/offsets/pre_3_13.rs`. Each version needs ~7
field offsets. These versions do not support GC stats reading.

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

```powershell
# One-time: set LIBCLANG_PATH to VS-bundled LLVM
$env:LIBCLANG_PATH = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin"

# Generate bindgen struct
python scripts/gen-offsets.py X:/path/to/cpython
```

The generator prints an **exact registration checklist to stderr** — follow it. It refuses
to overwrite an existing `v_*.rs` (use `--force` to regenerate a build in place). The
generated `v_*.rs` is self-contained (it carries its own `impl DebugOffsetsView` with all
version-varying offsets and the GC-stats shape), so registering it in
`src/remote_debugging/offsets/mod.rs` is ~8 lines and every site except the `LAYOUTS` row
is compiler-enforced (a forgotten site fails to build):

```diff
  // 1. module decl, with the other `mod v_*;`
+ mod v_3_15_0;
  // 2. LAYOUTS row (hex → struct reader) — the ONLY non-compiler-enforced site
+ (0x030f00f0, |p, a| Ok(VersionedOffsets::V3_15_0(read_struct(p, a)?))),
  // 3. VersionedOffsets enum variant
+ V3_15_0(v_3_15_0::_Py_DebugOffsets),
  // 4. for_each_variant! arm — drives ~20 accessors + the trait delegation automatically
+ Self::V3_15_0($o) => $body,
  // 5. validate() arm   — validate_basic(o, expected)  OR  v_3_15_0::validate_offsets(o, expected)
  // 6. Display arm       — fmt_debug_offsets_basic(o, f) OR  fmt::Display::fmt(o, f)
  // 7. basic tier only  — add to the impl_basic_display! and impl_basic_offsets! lists
```

Basic vs full tier (steps 5–7) is decided by the sub-struct count (`>= 21` → full, with
generated `validate_offsets`/`Display`); the generator's checklist tells you which applies.

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

The version hex in `patchlevel.h` encodes:
`(major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial`

Level: `0xA` = alpha, `0xB` = beta, `0xC` = release candidate, `0xF` = final

To find the hex for any checkout:
```powershell
python scripts/gen-offsets.py X:/path/to/cpython --stdout | grep version_hex
```
