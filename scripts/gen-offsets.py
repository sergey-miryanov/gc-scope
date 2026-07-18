#!/usr/bin/env python3
"""Generate Rust bindings for _Py_DebugOffsets from a CPython checkout.

Usage:
    python scripts/gen-offsets.py X:/path/to/cpython/checkout

Requires bindgen on PATH (install with: cargo install bindgen-cli).
Requires LIBCLANG_PATH set on Windows.
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path


def get_define(lines: list[str], name: str) -> str | None:
    for line in lines:
        m = re.match(rf'#define\s+{name}\s+(.+)', line)
        if m:
            return m.group(1).strip()
    return None


def resolve_release_level(lines: list[str], level_str: str) -> int:
    m = re.match(r'0x([0-9a-fA-F]+)', level_str)
    if m:
        return int(m.group(1), 16)
    m = re.match(r'^(\d+)$', level_str)
    if m:
        return int(m.group(1))
    macro_val = get_define(lines, level_str)
    if macro_val:
        return resolve_release_level(lines, macro_val)
    raise ValueError(f"Cannot resolve PY_RELEASE_LEVEL: {level_str}")


def main() -> None:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <cpython_path>", file=sys.stderr)
        sys.exit(1)

    cpython_path = Path(sys.argv[1])
    patchlevel = cpython_path / "Include" / "patchlevel.h"
    if not patchlevel.exists():
        print(f"Error: {patchlevel} not found", file=sys.stderr)
        sys.exit(1)

    lines = patchlevel.read_text().splitlines()
    major = int(get_define(lines, "PY_MAJOR_VERSION"))
    minor = int(get_define(lines, "PY_MINOR_VERSION"))
    micro = int(get_define(lines, "PY_MICRO_VERSION"))
    serial = int(get_define(lines, "PY_RELEASE_SERIAL"))
    level_str = get_define(lines, "PY_RELEASE_LEVEL")
    level = resolve_release_level(lines, level_str)

    version_hex = (major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial
    version_str = get_define(lines, "PY_VERSION").strip('"')

    commit_suffix = ""
    raw_version = version_str
    if version_str.endswith("+"):
        result = subprocess.run(
            ["git", "-C", str(cpython_path), "rev-parse", "--short", "HEAD"],
            capture_output=True, text=True
        )
        if result.returncode == 0:
            commit = result.stdout.strip()
            commit_suffix = f"_{commit}"
        version_str = version_str.rstrip("+")

    safe_tag = re.sub(r'[^a-zA-Z0-9_]', '', version_str.replace('.', '_')) + commit_suffix
    out_file = Path("src") / "remote_debugging" / "offsets" / f"v_{safe_tag}.rs"

    print(f"CPython {raw_version}  ->  0x{version_hex:08x}")
    print(f"Output: {out_file}")

    include_internal = cpython_path / "Include" / "internal"
    include_pc = cpython_path / "PC"

    # Find the header containing _Py_DebugOffsets
    offsets_header = include_internal / "pycore_debug_offsets.h"
    use_runtime_h = False
    if not offsets_header.exists():
        offsets_header = include_internal / "pycore_runtime.h"
        if not offsets_header.exists():
            print(f"Error: neither pycore_debug_offsets.h nor pycore_runtime.h found", file=sys.stderr)
            sys.exit(1)
        use_runtime_h = True

    # ── Detect gc_generation_stats support ────────────────────────────
    interp_structs = include_internal / "pycore_interp_structs.h"
    has_gc_stats = False
    if interp_structs.exists():
        has_gc_stats = "struct gc_generation_stats {" in interp_structs.read_text()

    # Write a wrapper header that supplies prerequisites for bindgen.
    # For 3.13.x (_Py_DebugOffsets is inside pycore_runtime.h), extract just
    # the struct definition so bindgen doesn't need to parse all dependencies.
    if use_runtime_h:
        hdr_text = offsets_header.read_text()
        start = hdr_text.find("typedef struct _Py_DebugOffsets {")
        if start == -1:
            # Try without typedef
            start = hdr_text.find("struct _Py_DebugOffsets {")
        end = hdr_text.find("} _Py_DebugOffsets;", start) + len("} _Py_DebugOffsets;")
        struct_text = hdr_text[start:end]
        # Extract _Py_Debug_Cookie from the header too
        cookie_match = re.search(r'#define\s+_Py_Debug_Cookie\s+"([^"]+)"', hdr_text)
        cookie_line = f'#define _Py_Debug_Cookie "{cookie_match.group(1)}"' if cookie_match else ''
        wrapper = f"""typedef unsigned long long uint64_t;
#define _Py_NONSTRING
#define _Py_Debug_Cookie "xdebugpy"
#pragma pack(push, 8)
{struct_text}
#pragma pack(pop)
"""
    else:
        wrapper = f"""typedef unsigned long long uint64_t;
#define _Py_NONSTRING
#include "{offsets_header.resolve()}"
"""
    # Extract struct gc_generation_stats from pycore_interp_structs.h to
    # avoid pulling in the full header tree (which has clang-irresolvable deps).
    if has_gc_stats:
        text = interp_structs.read_text()
        start = text.find("struct gc_generation_stats {")
        if start >= 0:
            depth = 0
            for i in range(start, len(text)):
                if text[i] == '{':
                    depth += 1
                elif text[i] == '}':
                    depth -= 1
                    if depth == 0:
                        # include the closing } and any trailing semicolon
                        end = i + 1
                        if end < len(text) and text[end] == ';':
                            end += 1
                        break
            gc_struct_text = text[start:end]
            wrapper += f"""
// Extracted from pycore_interp_structs.h
typedef long long Py_ssize_t;
typedef long long PyTime_t;
{gc_struct_text}"""

    with tempfile.TemporaryDirectory(prefix="gcscope-bindgen-") as tmpdir:
        wrapper_path = Path(tmpdir) / "wrapper.h"
        wrapper_path.write_text(wrapper)

        bindgen_args = [
            "bindgen",
            "--allowlist-type", "_Py_DebugOffsets",
            "--allowlist-var", "_Py_Debug_Cookie",
        ]
        if has_gc_stats:
            bindgen_args += ["--allowlist-type", "gc_generation_stats"]
        bindgen_args += [
            "--raw-line", "#![allow(non_snake_case, non_camel_case_types, dead_code, non_upper_case_globals, unused_imports)]",
            "--raw-line", "",
            "--raw-line", "use crate::impl_display_debug_offsets;",
            "--raw-line", "use crate::impl_validate_debug_offsets;",
            "--output", str(out_file),
            str(wrapper_path),
            "--",
            "-DPy_BUILD_CORE",
            f"-I{cpython_path / 'Include'}",
            f"-I{include_internal}",
            f"-I{include_pc}",
        ]
        result = subprocess.run(bindgen_args, capture_output=True, text=True)

        if result.returncode != 0:
            print(result.stderr, file=sys.stderr)
            print(f"bindgen failed with exit code {result.returncode}", file=sys.stderr)
            sys.exit(1)

    # Read bindgen output to discover which sub-structs were generated
    generated = out_file.read_text()
    sub_structs = re.findall(r'pub struct (_Py_DebugOffsets__\w+)', generated)

    # The display/validate macros require ALL 21 sub-structs.
    # 3.15+ has all 21; earlier versions have fewer.
    # Only generate macros for versions that have the full set.
    if len(sub_structs) >= 21:
        indent = "    "
        macro_call = (
            "\nimpl_display_debug_offsets!(_Py_DebugOffsets,\n"
            + ",\n".join(f"{indent}{s}" for s in sub_structs)
            + "\n);\n"
        )
        validate_call = (
            "\nimpl_validate_debug_offsets!(_Py_DebugOffsets,\n"
            + ",\n".join(f"{indent}{s}" for s in sub_structs[:-1])
            + f",\n{indent}{sub_structs[-1]}"
            + "\n);\n"
        )
        with open(out_file, "a") as f:
            f.write(macro_call)
            f.write(validate_call)

    # ── Extract gc_generation_stats field layout from bindgen output ──
    # bindgen already generated the #[repr(C)] struct; we just need field names.
    if "pub struct gc_generation_stats {" in generated:
        # Extract field names from bindgen's Rust output
        brace = generated.index("pub struct gc_generation_stats {")
        brace = generated.index('{', brace)
        close = generated.index('}', brace)
        body = generated[brace+1:close]
        field_names = re.findall(r'^\s+pub (\w+):', body, re.MULTILINE)

        field_entries = "\n".join(
            f'        ("{name}", std::mem::offset_of!(gc_generation_stats, {name})),'
            for name in field_names
        )
        gc_block = f"""

// -- GC generation stats field layout --
// Computed from bindgen-generated #[repr(C)] struct via offset_of! at compile time.

pub use crate::remote_debugging::offsets::offset_table::GcItemLayout;

pub const GC_ITEM_SIZE: usize = std::mem::size_of::<gc_generation_stats>();

pub static GC_LAYOUT: GcItemLayout = GcItemLayout {{
    item_size: GC_ITEM_SIZE,
    fields: &[
{field_entries}
    ],
}};

pub fn gc_field_names() -> &'static [(&'static str, usize)] {{
    GC_LAYOUT.fields
}}
"""
        with open(out_file, "a") as f:
            f.write(gc_block)

        print(f"  gc_generation_stats: {len(field_names)} fields, via bindgen + offset_of!", file=sys.stderr)

    print(f"Generated {out_file}  (version hex: 0x{version_hex:08x})")


if __name__ == "__main__":
    main()
