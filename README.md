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
| 3.15.0a7 | `0x030f00a7` | generated layout |
| 3.15.0a8 | `0x030f00a8` | bindgen |
| 3.15.0b1+ | `0x030f00b1` | bindgen |
| 3.16.0a0 | `0x031000a0` | bindgen |

All pre-3.13 versions (3.8–3.12) use hardcoded tables in `pre_3_13.rs`.

## How offset tables work

gcscope needs to know the byte offsets of fields within `_PyRuntime`,
`PyInterpreterState`, `PyThreadState`, and the GC state. These offsets
change between every Python minor version (3.8 → 3.9 → … → 3.16).
There are three ways to obtain them:

### 1. Hardcoded tables (3.8–3.12)

For versions before `_Py_DebugOffsets` existed, the offsets are
extracted from CPython headers by hand and stored in
`src/remote_debugging/offsets/pre_3_13.rs`. Each version needs ~7
field offsets. These versions do not support GC stats reading.

### 2. Generated layout (any 3.13+, verify-only)

`scripts/gen-offsets-table.py` parses the `_Py_DebugOffsets` C struct
definition using pure Python (no compiler needed). It walks the
`uint64_t` fields sequentially, recording each field's byte position
within the struct, and emits a compact `DebugOffsetsLayout` constant.

At runtime, gcscope reads the raw `_Py_DebugOffsets` bytes from the
target process, extracts the actual offset values at the recorded
positions, and builds an `OffsetTable`. This supports `--verify` and
pointer walking for any 3.13+ version with zero toolchain dependencies.

### 3. Bindgen-generated struct (3.13+, full support)

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

Then register in `src/remote_debugging/offsets/mod.rs`:

```diff
+ mod v_3_15_0;
+ use v_3_15_0::_Py_DebugOffsets; // in read_offsets match
+ VersionedOffsets::V3_15_0(raw)
```

**Quick verify-only** (generated layout, no bindgen needed):

```powershell
python scripts/gen-offsets-table.py X:/path/to/cpython
```

Then register in `src/remote_debugging/offsets/tables/mod.rs`:

```diff
+ mod v_3_15_0;
  0x030f00f0 => Some(&v_3_15_0::LAYOUT),
```

### Version hex reference

The version hex in `patchlevel.h` encodes:
`(major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial`

Level: `0xA` = alpha, `0xB` = beta, `0xC` = release candidate, `0xF` = final

To find the hex for any checkout:
```powershell
python scripts/gen-offsets.py X:/path/to/cpython --stdout | grep version_hex
```
