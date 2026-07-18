#!/usr/bin/env python3
"""Generate an OffsetTable for a CPython version by parsing _Py_DebugOffsets.

Usage:
    python scripts/gen-offsets-table.py X:/path/to/cpython/checkout
    python scripts/gen-offsets-table.py X:/path/to/cpython/checkout --stdout

Parses the _Py_DebugOffsets C struct (either from pycore_debug_offsets.h
in 3.14+ or from pycore_runtime.h in 3.13.x) and emits a Rust OffsetTable
constant with the fields gcscope uses.
"""

import re
import subprocess
import sys
from pathlib import Path


# ── C struct field walker ──────────────────────────────────────────

def walk_fields(lines: list[str]) -> dict[str, int]:
    """Walk uint64_t fields in _Py_DebugOffsets, return {full_name: byte_offset}.

    Handles:
        char cookie[8];
        uint64_t version;
        uint64_t free_threaded;
        struct _xxx { uint64_t a; uint64_t b; } xxx;
    Ignores non-uint64_t fields, comments, preprocessor lines.
    """
    offset = 0
    substruct_prefix = None
    fields: dict[str, int] = {}

    i = 0
    while i < len(lines):
        line = lines[i].strip()

        # Skip preprocessor, comments, empty lines
        if line.startswith('#') or line.startswith('//') or line.startswith('/*') or not line:
            # Handle block comments
            if line.startswith('/*') and '*/' not in line:
                while i < len(lines) and '*/' not in lines[i]:
                    i += 1
            i += 1
            continue

        # char cookie[8]; -> skip 8 bytes
        m = re.match(r'char\s+\w+\[\d+\]', line)
        if m:
            m2 = re.search(r'\[(\d+)\]', line)
            if m2:
                offset += int(m2.group(1))
            i += 1
            continue

        # struct _xxx {  (begin sub-struct)
        m = re.match(r'struct\s+_(\w+)\s*\{', line)
        if m:
            substruct_prefix = m.group(1)
            # sub-struct starts at current offset (its size field)
            # continue to walk its fields
            i += 1
            continue

        # } xxx;  (end sub-struct)
        if re.match(r'\}\s*\w+\s*;', line):
            # sub-struct ends: offset stays at end
            substruct_prefix = None
            i += 1
            continue

        # } _Py_DebugOffsets;  (end main struct)
        if re.match(r'\}\s*_Py_DebugOffsets\s*;', line):
            break

        # uint64_t name;
        m = re.match(r'uint64_t\s+(\w+)\s*;', line)
        if m:
            if substruct_prefix:
                name = m.group(1)
                full_name = f"{substruct_prefix}.{name}"
                fields[full_name] = offset
            offset += 8

        i += 1

    return fields


# ── Version info from patchlevel.h ─────────────────────────────────

def get_version_info(cpython_path: Path) -> tuple[int, str, str]:
    patchlevel = cpython_path / "Include" / "patchlevel.h"
    text = patchlevel.read_text()

    def get_define(name: str) -> str:
        m = re.search(rf'#define\s+{name}\s+(.+)', text)
        return m.group(1).strip() if m else ""

    def resolve_release_level(level_str: str) -> int:
        m = re.match(r'0x([0-9a-fA-F]+)', level_str)
        if m:
            return int(m.group(1), 16)
        m = re.match(r'^(\d+)$', level_str)
        if m:
            return int(m.group(1))
        macro_val = get_define(level_str)
        if macro_val:
            return resolve_release_level(macro_val)
        raise ValueError(f"Cannot resolve PY_RELEASE_LEVEL: {level_str}")

    major = int(get_define("PY_MAJOR_VERSION"))
    minor = int(get_define("PY_MINOR_VERSION"))
    micro = int(get_define("PY_MICRO_VERSION"))
    serial = int(get_define("PY_RELEASE_SERIAL"))
    level = resolve_release_level(get_define("PY_RELEASE_LEVEL"))
    version_hex = (major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial
    version_str = get_define("PY_VERSION").strip('"')

    commit_suffix = ""
    if version_str.endswith("+"):
        result = subprocess.run(
            ["git", "-C", str(cpython_path), "rev-parse", "--short", "HEAD"],
            capture_output=True, text=True, timeout=10
        )
        if result.returncode == 0:
            commit_suffix = f"_{result.stdout.strip()}"

    safe_tag = re.sub(r'[^a-zA-Z0-9_]', '', version_str.replace('.', '_')) + commit_suffix
    return version_hex, safe_tag, version_str


# ── Header location ────────────────────────────────────────────────

def find_offsets_header(cpython_path: Path) -> Path:
    """Return path to the header containing _Py_DebugOffsets."""
    h1 = cpython_path / "Include" / "internal" / "pycore_debug_offsets.h"
    if h1.exists():
        return h1
    h2 = cpython_path / "Include" / "internal" / "pycore_runtime.h"
    if h2.exists():
        return h2
    raise FileNotFoundError(
        "Could not find pycore_debug_offsets.h or pycore_runtime.h"
    )


def extract_struct_text(header: Path) -> list[str]:
    """Extract lines of the _Py_DebugOffsets struct definition."""
    text = header.read_text()
    start = text.find("typedef struct _Py_DebugOffsets {")
    if start == -1:
        raise ValueError("_Py_DebugOffsets not found in header")
    # Find the closing brace + typedef
    end = text.find("} _Py_DebugOffsets;", start)
    if end == -1:
        raise ValueError("_Py_DebugOffsets closing brace not found")
    block = text[start:end + len("} _Py_DebugOffsets;")]
    return block.splitlines()


# ── Field names gcscope needs ──────────────────────────────────────

NEEDED = [
    # (output_field_name, field_path_in_struct)
    ("runtime_interpreters_head", "runtime_state.interpreters_head"),
    ("interp_id", "interpreter_state.id"),
    ("interp_next", "interpreter_state.next"),
    ("interp_threads_head", "interpreter_state.threads_head"),
    ("interp_gc", "interpreter_state.gc"),
    ("thread_interp", "thread_state.interp"),
    ("gc_size", "gc.size"),
    ("gc_collecting", "gc.collecting"),
]

OPTIONAL = [
    # (output_field_name, field_path_in_struct, min_gc_size)
    ("gc_frame", "gc.frame", 24),
    ("gc_generation_stats_size", "gc.generation_stats_size", 32),
    ("gc_generation_stats", "gc.generation_stats", 40),
]


# ── Output ─────────────────────────────────────────────────────────

def emit_rust(version_hex: int, tag: str, version_str: str, fields: dict[str, int]) -> str:
    lines = [f"""// Generated by gen-offsets-table.py for CPython {version_str}
// Version hex: 0x{version_hex:08x}

use crate::remote_debugging::offsets::offset_table::DebugOffsetsLayout;

pub const LAYOUT: DebugOffsetsLayout = DebugOffsetsLayout {{"""]
    lines.append(f"    version_hex: 0x{version_hex:08x},")

    for rust_name, c_path in NEEDED:
        if c_path not in fields:
            print(f"WARNING: {c_path} not found in struct", file=sys.stderr)
            lines.append(f"    {rust_name}: 0, // NOT FOUND")
            continue
        lines.append(f"    {rust_name}: {fields[c_path]},")

    for rust_name, c_path, _ in OPTIONAL:
        pos = fields.get(c_path)
        if pos is not None:
            lines.append(f"    {rust_name}: Some({pos}),")
        else:
            lines.append(f"    {rust_name}: None,")

    lines.append("};")
    return "\n".join(lines)


def main() -> None:
    if len(sys.argv) not in (2, 3):
        print(f"Usage: {sys.argv[0]} <cpython_path> [--stdout]", file=sys.stderr)
        sys.exit(1)

    cpython_path = Path(sys.argv[1])
    to_stdout = "--stdout" in sys.argv

    version_hex, tag, version_str = get_version_info(cpython_path)

    print(f"CPython {version_str}  ->  0x{version_hex:08x}", file=sys.stderr)

    header_path = find_offsets_header(cpython_path)
    print(f"Header: {header_path}", file=sys.stderr)

    struct_lines = extract_struct_text(header_path)
    fields = walk_fields(struct_lines)

    print(f"Found {len(fields)} fields in _Py_DebugOffsets", file=sys.stderr)

    rust_code = emit_rust(version_hex, tag, version_str, fields)

    if to_stdout:
        print(rust_code)
    else:
        out_dir = Path("src") / "remote_debugging" / "offsets" / "tables"
        out_dir.mkdir(parents=True, exist_ok=True)
        out_file = out_dir / f"v_{tag}.rs"
        out_file.write_text(rust_code + "\n")
        print(f"Generated {out_file}", file=sys.stderr)


if __name__ == "__main__":
    main()
