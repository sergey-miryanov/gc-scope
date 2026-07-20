#!/usr/bin/env python3
"""Generate Rust bindings for _Py_DebugOffsets from a CPython checkout.

Usage:
    python scripts/gen-offsets.py X:/path/to/cpython/checkout

Requires bindgen on PATH (install with: cargo install bindgen-cli --version 0.72).
Requires LIBCLANG_PATH set on Windows.
"""

import argparse
import re
import subprocess
import sys
import tempfile
from pathlib import Path


def hex_already_registered(version_hex: int) -> bool:
    """True if `version_hex` already has a `LAYOUTS` row in offsets/mod.rs.

    When it does, a second checkout reporting the same hex is a *same-hex* build
    (a clean release vs a gc-instrumented `+inc` build sharing a PY_VERSION_HEX):
    it must NOT get its own nav variant, only a `GC_CANDIDATES` GC layout.
    """
    mod_rs = Path("src") / "remote_debugging" / "offsets" / "mod.rs"
    if not mod_rs.exists():
        return False
    # Match a LAYOUTS row: `(0x030f00b1, |p, a| ...)`.
    return bool(re.search(rf'\(\s*0x{version_hex:08x}\s*,\s*\|p,\s*a\|', mod_rs.read_text()))


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


def variant_name(major: int, minor: int, micro: int, level: int, serial: int) -> str:
    """Derive the `VersionedOffsets` enum variant name (no commit suffix).

    e.g. 3.14.4 final -> V3_14_4 ; 3.15.0a7 -> V3_15_0a7 ; 3.15.0b1 -> V3_15_0b1.
    """
    letters = {0xA: "a", 0xB: "b", 0xC: "rc", 0xF: ""}
    suffix = "" if level == 0xF else f"{letters.get(level, f'x{level:x}')}{serial}"
    return f"V{major}_{minor}_{micro}{suffix}"


def _brace_end(text: str, open_idx: int) -> int:
    """Index of the `}` matching the `{` at `open_idx`, or -1."""
    depth = 0
    for j in range(open_idx, len(text)):
        if text[j] == '{':
            depth += 1
        elif text[j] == '}':
            depth -= 1
            if depth == 0:
                return j
    return -1


def _extract_struct(text: str, anchor: str) -> str | None:
    """Extract a `struct NAME { ... };` block given its opening `anchor`."""
    s = text.find(anchor)
    if s < 0:
        return None
    o = text.index('{', s)
    e = _brace_end(text, o)
    if e < 0:
        return None
    end = e + 1
    m = re.match(r'\s*[A-Za-z_]\w*\s*;', text[end:])   # optional trailing `name;`
    if m:
        end += m.end()
    elif text[end:end + 1] == ';':
        end += 1
    return text[s:end]


def _extract_typedef_named(text: str, name: str) -> str | None:
    """Extract a `typedef struct { ... } NAME;` block by its `name`."""
    tail = f"}} {name};"
    ti = text.find(tail)
    if ti < 0:
        return None
    head = text.rfind("typedef struct", 0, ti)
    if head < 0:
        return None
    return text[head:ti + len(tail)]


def _find_header_with(inc: Path, anchor: str, names: tuple[str, ...]) -> str | None:
    """Text of the first header in `names` whose content contains `anchor`."""
    for n in names:
        h = inc / n
        if h.exists() and anchor in h.read_text():
            return h.read_text()
    return None


def compute_inline_stats_off(cpython_path: Path) -> int | None:
    """Byte offset of the inline `generation_stats[]` array within `_gc_runtime_state`.

    This offset is version-specific (3.13 = 128, 3.14 = 120, 3.15.0a7 = 104) because
    each release reshuffles the fields preceding `generation_stats`: 3.14 dropped the
    `generation0` pointer, 3.15.0a7 dropped `trash_delete_later`, and so on. Crucially
    it is NOT exposed by `_Py_DebugOffsets` for these versions (their `gc` sub-struct
    carries only `size`/`collecting`), so gcscope cannot read it from the target — it
    must be computed at generation time. We reconstruct `_gc_runtime_state` from the
    real headers and let bindgen compute the offset via `offset_of!`.

    Returns None if the struct can't be reconstructed or bindgen fails; the caller then
    treats this build as having no readable inline stats (`GcStatsKind::None`).
    """
    inc = cpython_path / "Include" / "internal"
    grt = _extract_struct(
        _find_header_with(inc, "struct _gc_runtime_state {",
                          ("pycore_interp_structs.h", "pycore_gc.h")) or "",
        "struct _gc_runtime_state {")
    pygc = _extract_typedef_named(
        _find_header_with(inc, "} PyGC_Head;",
                          ("pycore_gc.h", "pycore_interp_structs.h")) or "",
        "PyGC_Head")
    ggen = _extract_struct(
        _find_header_with(inc, "struct gc_generation {",
                          ("pycore_gc.h", "pycore_interp_structs.h")) or "",
        "struct gc_generation {")
    gstat = _extract_struct(
        _find_header_with(inc, "struct gc_generation_stats {",
                          ("pycore_interp_structs.h", "pycore_gc.h")) or "",
        "struct gc_generation_stats {")
    if not (grt and "generation_stats[" in grt and pygc and ggen and gstat):
        return None

    ng_src = _find_header_with(inc, "#define NUM_GENERATIONS",
                               ("pycore_gc.h", "pycore_interp_structs.h")) or ""
    ng_m = re.search(r'#define\s+NUM_GENERATIONS\s+(\d+)', ng_src)
    num_gen = ng_m.group(1) if ng_m else "3"

    # Prerequisites: forward-declare opaque referents (PyObject, _PyInterpreterFrame)
    # and neutralize the alignment macro. Dropping `_Py_ALIGNED_DEF`'s alignment is
    # layout-safe here because `_PyObject_MIN_ALIGNMENT` is 4 <= natural pointer
    # alignment, so it never inserts padding. The `#ifdef Py_GIL_DISABLED` tail
    # (PyMutex etc.) is excluded because we don't define that macro.
    wrapper = f"""typedef unsigned long long uintptr_t;
typedef long long Py_ssize_t;
typedef struct _object PyObject;
typedef struct _PyInterpreterFrame _PyInterpreterFrame;
#define _Py_ALIGNED_DEF(N, T) T
#define NUM_GENERATIONS {num_gen}
{pygc}
{ggen}
{gstat}
{grt}
"""
    with tempfile.TemporaryDirectory(prefix="gcscope-inlineoff-") as tmpdir:
        wpath = Path(tmpdir) / "gcrt.h"
        wpath.write_text(wrapper)
        opath = Path(tmpdir) / "gcrt.rs"
        r = subprocess.run(
            ["bindgen", "--allowlist-type", "_gc_runtime_state",
             "--output", str(opath), str(wpath), "--", "-DPy_BUILD_CORE"],
            capture_output=True, text=True,
        )
        if r.returncode != 0:
            print("Note: could not compute the inline generation_stats offset "
                  "(bindgen failed on _gc_runtime_state) — GC stats will be "
                  "unavailable for this build.", file=sys.stderr)
            return None
        m = re.search(
            r'offset_of!\(_gc_runtime_state, generation_stats\)\s*-\s*(\d+)usize',
            opath.read_text())
        return int(m.group(1)) if m else None


def print_same_hex_checklist(
    *, version_hex: int, mod_name: str, gc_kind: str,
) -> None:
    """Checklist for a second build sharing an already-registered PY_VERSION_HEX.

    The nav struct (`_Py_DebugOffsets`) is identical to the registered build, so this
    module is NOT a new `VersionedOffsets` variant — only its `gc_generation_stats`
    layout differs. It is wired in as a `GC_CANDIDATES` entry, selected at read-time by
    the process-published ring size. This is the ONLY registration needed.
    """
    bar = "═" * 70
    kind_path = f"offset_table::GcStatsKind::{gc_kind}"
    print(f"\n{bar}", file=sys.stderr)
    print(f"SAME-HEX BUILD (0x{version_hex:08x} already registered) — GC layout only",
          file=sys.stderr)
    print(bar, file=sys.stderr)
    print(f"""\
  This build shares its PY_VERSION_HEX and `_Py_DebugOffsets` with an already-registered
  version; only its `gc_generation_stats` differs. Do NOT add a `LAYOUTS` row, a
  `VersionedOffsets` variant, a `for_each_variant!` / validate / Display arm, or any
  `impl_basic_*` entry. Two edits in src/remote_debugging/offsets/mod.rs:

  1. Module decl (with the other `mod v_*;`):
       mod {mod_name};

  2. `GC_CANDIDATES` — add this build's layout to the entry for 0x{version_hex:08x}
     (create the `(0x{version_hex:08x}, &[ ... ])` entry if it's the first pair, and
     include the ALREADY-registered nav variant's own layout as the other candidate):
       GcCandidate {{
           kind: {kind_path},
           item_size: {mod_name}::GC_ITEM_SIZE as u64,
           layout: &{mod_name}::GC_LAYOUT,
       }},

  Then `cargo test` — `gc_candidates_have_distinct_ring_sizes` must pass. If it fails,
  the two builds have the SAME ring size and cannot be told apart out-of-process; one
  must be dropped (see the test's message). Live-check: `cargo run -- gc-stats <PID>`
  against a process of THIS build decodes with the extended columns.
{bar}""", file=sys.stderr)


def print_registration_checklist(
    *, version_hex: int, mod_name: str, variant: str,
    sub_struct_count: int, has_gc_stats: bool,
) -> None:
    """Print the exact edits needed to wire a generated module into dispatch.

    Registration is manual (the enum + `read_offsets` are hand-written), and a
    module that is generated but only half-registered fails at runtime for that
    version. This checklist makes every required site explicit so none is missed.
    """
    full_macros = sub_struct_count >= 21
    if full_macros:
        display_line = "uses the FULL macros: v_...::validate_offsets / fmt::Display::fmt"
    else:
        display_line = ("uses BASIC: validate_basic + fmt_debug_offsets_basic; also add to "
                        "the impl_basic_display! and impl_basic_offsets! lists")
    if has_gc_stats:
        gc_note = "(this module exports GC_LAYOUT — reference it in the arm above)"
    else:
        gc_note = "(no GC_LAYOUT emitted for this build)"

    bar = "═" * 70
    print(f"\n{bar}", file=sys.stderr)
    print("REGISTER THIS VERSION in src/remote_debugging/offsets/mod.rs", file=sys.stderr)
    print(bar, file=sys.stderr)
    print(f"""\
  The generated file already carries its `impl DebugOffsetsView` (version-varying
  offsets + GC-stats shape), so almost no per-version accessor edits are needed — the
  one exception is the `gc_debug_fields` arm (#5), a hand-written `match` rather than a
  `for_each_variant!` accessor. Add these {mod_name} entries — every one is
  compiler-enforced except #2:

  1. Module decl (with the other `mod v_*;`):
       mod {mod_name};

  2. `LAYOUTS` registry row (hex → struct reader):
       (0x{version_hex:08x}, |p, a| Ok(VersionedOffsets::{variant}(read_struct(p, a)?))),

  3. `VersionedOffsets` enum variant:
       {variant}({mod_name}::_Py_DebugOffsets),

  4. `for_each_variant!` macro arm (drives most accessors + the trait delegation):
       Self::{variant}($o) => $body,

  5. `gc_debug_fields` match arm (NOT for_each_variant!-driven — computes the gc
     sub-struct field offsets from this build's own types via offset_of!/size_of!):
       Self::{variant}(_) => build(
           offset_of!({mod_name}::_Py_DebugOffsets, gc),
           size_of::<{mod_name}::_Py_DebugOffsets__gc>()),

  6. `validate` arm:
       {display_line}
       (this build has {sub_struct_count} sub-structs; full macros need >= 21)

  7. `Display` arm (same basic/full split as validate).

  {"8. impl_basic_display! + impl_basic_offsets! lists (basic tier only)." if not full_macros else "(full tier: no impl_basic_* entries needed.)"}
       {gc_note}
{bar}""", file=sys.stderr)


def _short_remote(remote: str | None) -> str:
    """`https://github.com/python/cpython.git` / `git@…:python/cpython.git` → `python/cpython`."""
    if not remote:
        return "cpython"
    remote = remote.strip()
    m = re.search(r'[:/]([^/:]+/[^/]+?)(?:\.git)?$', remote)
    return m.group(1) if m else remote


def git_provenance(cpython_path: Path) -> dict | None:
    """Source provenance for the checkout, or None if it is not a git repo.

    `at_tag` is True only when HEAD is exactly a release tag. A build off a tag is an
    in-development ("ongoing") layout that keeps drifting; that is what the single-ongoing
    guard and the "must come from git" rule key on. `commit` is the FULL 40-char SHA so
    the provenance line greps unambiguously.
    """
    def _git(*a: str) -> str | None:
        r = subprocess.run(["git", "-C", str(cpython_path), *a],
                           capture_output=True, text=True)
        return r.stdout.strip() if r.returncode == 0 else None

    commit = _git("rev-parse", "HEAD")
    if not commit:
        return None
    return {
        "commit": commit,
        "remote": _short_remote(_git("remote", "get-url", "origin")),
        "describe": _git("describe", "--tags", "HEAD"),
        "at_tag": _git("describe", "--exact-match", "--tags", "HEAD") is not None,
    }


def provenance_comment(prov: dict, *, is_ongoing: bool, version_str: str,
                       version_hex: int) -> str:
    """The `// gcscope-source:` block embedded at the top of a generated module.

    The `owner/repo@<40-hex>` shape is a contract: .github/workflows/rust.yml greps the
    commit out of it to pin the from-source CI build of an ongoing version, so the whole
    thing is a single source of truth — regenerate and the CI pin moves with it.
    """
    desc = f" — describe {prov['describe']}" if prov.get("describe") else ""
    lines = [
        f"// gcscope-source: {prov['remote']}@{prov['commit']}",
        f"//   CPython {version_str} (0x{version_hex:08x}){desc}",
    ]
    if is_ongoing:
        lines += [
            "//   ONGOING dev build (HEAD is not a release tag): only one such layout may be",
            "//   registered at a time, and CI pins this exact commit (read from this line by",
            "//   .github/workflows/rust.yml). Regenerate and the pin moves with it.",
        ]
    return "\n".join(lines) + "\n"


def existing_ongoing(offsets_dir: Path, exclude: Path) -> list[tuple[str, str]]:
    """(filename, commit) for every OTHER registered module marked ONGOING."""
    out = []
    for f in sorted(offsets_dir.glob("v_*.rs")):
        if f.resolve() == exclude.resolve():
            continue
        text = f.read_text()
        if "ONGOING dev build" in text:
            m = re.search(r'gcscope-source: \S+@([0-9a-f]{7,40})', text)
            out.append((f.name, m.group(1) if m else "?"))
    return out


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate Rust _Py_DebugOffsets bindings from a CPython checkout.")
    parser.add_argument("cpython_path", type=Path,
                        help="CPython source checkout (must have Include/patchlevel.h).")
    parser.add_argument(
        "--suffix", "-s", default="",
        help="Disambiguating tag for the output filename: --suffix gcinc writes "
             "v_<version>_gcinc.rs. Use for a second build that shares a PY_VERSION_HEX "
             "with an already-registered one (e.g. a clean release vs a `+inc` build "
             "whose patchlevel.h is unchanged, so the version alone can't tell them apart).")
    parser.add_argument(
        "--force", "-f", action="store_true",
        help="Overwrite an existing v_<version>.rs in place (regenerate the same build). "
             "Without it, an existing output file is a hard error, not a silent clobber.")
    args = parser.parse_args()

    cpython_path = args.cpython_path
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

    ver_tag = re.sub(r'[^a-zA-Z0-9_]', '', version_str.replace('.', '_'))
    suffix = re.sub(r'[^a-zA-Z0-9_]', '', args.suffix)
    # An explicit --suffix names a distinct file (a same-hex second build); otherwise the
    # tag is the version plus, for `+`-tagged dev builds, the git commit. `+inc`-style
    # builds that DON'T bump patchlevel.h land on the bare version tag and would collide
    # with the clean release — that collision is caught by the overwrite guard below.
    safe_tag = f"{ver_tag}_{suffix}" if suffix else ver_tag + commit_suffix
    out_file = Path("src") / "remote_debugging" / "offsets" / f"v_{safe_tag}.rs"

    print(f"CPython {raw_version}  ->  0x{version_hex:08x}")
    print(f"Output: {out_file}")

    # Provenance + "ongoing" status. An ongoing (in-development) layout is one built off a
    # release tag; a --suffix same-hex candidate (e.g. +inc) is never a version of its own,
    # so it is excluded regardless of tag state.
    prov = git_provenance(cpython_path)
    is_ongoing = prov is not None and not prov["at_tag"] and not suffix
    # A dev-head build with no git repo cannot be pinned, so its layout would be one no CI
    # leg could reproduce. Refuse rather than record it.
    looks_dev = (level == 0xA and serial == 0) or raw_version.endswith("+")
    if prov is None and not suffix and looks_dev:
        print(f"\nError: {raw_version} looks like an in-development build, but "
              f"{cpython_path}\n  is not a git checkout. An ongoing layout must be "
              f"generated from git so its exact\n  commit can be recorded and pinned by "
              f"CI — generate from a clone, not a tarball.", file=sys.stderr)
        sys.exit(1)

    # Never silently clobber an existing layout. A clean release and a gc-instrumented
    # `+inc` build can share a PY_VERSION_HEX and thus derive the SAME filename, so a bare
    # regen of the second would overwrite the first. Require an explicit choice.
    if out_file.exists() and not args.force and not suffix:
        print(f"\nError: {out_file} already exists — refusing to overwrite.", file=sys.stderr)
        if hex_already_registered(version_hex):
            print(
                f"  0x{version_hex:08x} is already registered. If this is a DIFFERENT build\n"
                f"  sharing this PY_VERSION_HEX (e.g. clean release vs a gc-instrumented\n"
                f"  `+inc` build), give it a distinct name — only its GC layout is new:\n\n"
                f"      python {Path(sys.argv[0]).name} {cpython_path} --suffix gcinc\n\n"
                f"  → writes v_{ver_tag}_gcinc.rs; the checklist then shows the one\n"
                f"  GC_CANDIDATES entry to add (no new enum variant).\n"
                f"  To instead regenerate THIS same build in place, re-run with --force.",
                file=sys.stderr)
        else:
            print(f"  Re-run with --force to regenerate it in place, or --suffix <tag> to\n"
                  f"  write a distinct v_{ver_tag}_<tag>.rs.", file=sys.stderr)
        sys.exit(1)

    # At most one in-development layout may be registered: two dev snapshots drift and
    # there is no oracle for which is current. Regenerating THIS build in place is fine
    # (its own file is excluded); a DIFFERENT ongoing version must wait until the current
    # one is retired. --force overrides for the rare deliberate overlap.
    if is_ongoing and not args.force:
        others = existing_ongoing(out_file.parent, out_file)
        if others:
            listed = "\n".join(f"      {name}  (commit {c})" for name, c in others)
            print(f"\nError: an ongoing dev layout is already registered:\n{listed}\n"
                  f"  Only one in-development (off-tag) version may be registered at a "
                  f"time. Retire it\n  (delete the module + its mod.rs rows) before adding "
                  f"{raw_version}, or pass --force.", file=sys.stderr)
            sys.exit(1)

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
    # The struct lives in different headers across versions: pycore_interp_structs.h
    # (3.14+) or pycore_gc.h (3.13.x). Search both so GC_LAYOUT is emitted for every
    # version that actually defines the struct.
    gc_stats_header = None
    for _name in ("pycore_interp_structs.h", "pycore_gc.h"):
        _h = include_internal / _name
        if _h.exists() and "struct gc_generation_stats {" in _h.read_text():
            gc_stats_header = _h
            break
    has_gc_stats = gc_stats_header is not None
    if not has_gc_stats:
        print("Note: `struct gc_generation_stats` not found in pycore_interp_structs.h "
              "or pycore_gc.h — GC_LAYOUT will NOT be emitted for this build.", file=sys.stderr)

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
    # Extract struct gc_generation_stats from its header to avoid pulling in the
    # full header tree (which has clang-irresolvable deps).
    if has_gc_stats:
        text = gc_stats_header.read_text()
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
// Extracted from {gc_stats_header.name}
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

    # Embed source provenance right after bindgen's autogen header (line 1), so every
    # module records the exact commit it came from; for an ongoing build CI reads the pin
    # from here.
    if prov is not None:
        text = out_file.read_text()
        nl = text.index("\n") + 1
        out_file.write_text(
            text[:nl]
            + provenance_comment(prov, is_ongoing=is_ongoing,
                                 version_str=version_str, version_hex=version_hex)
            + text[nl:])

    # Read bindgen output to discover which sub-structs were generated
    generated = out_file.read_text()
    sub_structs = re.findall(r'pub struct (_Py_DebugOffsets__\w+)', generated)

    # Emit the full Display/validation macros only when this build has the full set
    # of 21 nested sub-structs those macros expect (one positional type arg each, in
    # declaration order). Counting the generated sub-struct types is the reliable
    # gate: several fields (e.g. `err_stackitem`) are ANONYMOUS structs that bindgen
    # names `_Py_DebugOffsets__bindgen_ty_N`, so matching by field/type NAME is not
    # safe — the count is. Earlier versions with fewer sub-structs fall back to basic.
    if len(sub_structs) >= 21:
        indent = "    "
        args = ",\n".join(f"{indent}{s}" for s in sub_structs)
        macro_call = f"\nimpl_display_debug_offsets!(_Py_DebugOffsets,\n{args}\n);\n"
        validate_call = f"\nimpl_validate_debug_offsets!(_Py_DebugOffsets,\n{args}\n);\n"
        with open(out_file, "a") as f:
            f.write(macro_call)
            f.write(validate_call)

    # ── Extract gc_generation_stats field layout from bindgen output ──
    # bindgen already generated the #[repr(C)] struct; we just need field names.
    wrote_gc_layout = False
    if "pub struct gc_generation_stats {" in generated:
        # Extract field names from bindgen's Rust output
        brace = generated.index("pub struct gc_generation_stats {")
        brace = generated.index('{', brace)
        close = generated.index('}', brace)
        body = generated[brace+1:close]
        # Real gc_generation_stats fields never start with `_`; drop bindgen
        # artifacts (__bindgen_padding_*, _bitfield_*) so they don't pollute the layout.
        field_names = [
            n for n in re.findall(r'^\s+pub (\w+):', body, re.MULTILINE)
            if not n.startswith('_')
        ]

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
        wrote_gc_layout = True

        print(f"  gc_generation_stats: {len(field_names)} fields, via bindgen + offset_of!", file=sys.stderr)

    # ── Emit the DebugOffsetsView impl (per-version dispatch) ──
    # This is what lets `VersionedOffsets` delegate the version-varying offsets and the
    # GC-stats shape uniformly, so mod.rs needs no per-version accessor arms.
    def _substruct_body(name: str) -> str:
        key = f"pub struct _Py_DebugOffsets__{name} {{"
        if key not in generated:
            return ""
        i = generated.index("{", generated.index(key))
        return generated[i:generated.index("}", i)]

    gc_body = _substruct_body("gc")
    has_threads_main = "threads_main:" in _substruct_body("interpreter_state")
    has_frame = "frame:" in gc_body
    has_gen_stats = "generation_stats:" in gc_body           # ring-buffer pointer
    has_gen_stats_size = "generation_stats_size:" in gc_body

    # The stats are readable two ways: a ring-buffer pointer in _Py_DebugOffsets.gc
    # (3.15.0a8+), OR an inline `generation_stats[]` array in `_gc_runtime_state`
    # (3.8–3.15.0a7). The inline array's offset moves every release (3.13 = 0x80,
    # 3.14 = 0x78, 3.15.0a7 = 0x68) and is NOT in _Py_DebugOffsets, so we compute it
    # from the headers. `is_inline` is true exactly when that computation succeeds and
    # this build has no ring pointer; the ring pointer always wins when both exist.
    inline_off = None
    if wrote_gc_layout and not has_gen_stats:
        inline_off = compute_inline_stats_off(cpython_path)

    is_ring = has_gen_stats and wrote_gc_layout
    is_inline = inline_off is not None

    tm = "self.interpreter_state.threads_main" if has_threads_main else "0"
    fr = "self.gc.frame" if has_frame else "0"
    gs = "self.gc.generation_stats" if has_gen_stats else "0"
    gss = "self.gc.generation_stats_size" if has_gen_stats_size else "0"
    if is_ring:
        kind, item, layout = "RingBuffer", "GC_ITEM_SIZE as u64", "Some(&GC_LAYOUT)"
    elif is_inline:
        kind, item, layout = "InlineArray", "GC_ITEM_SIZE as u64", "Some(&GC_LAYOUT)"
    else:
        kind, item, layout = "None", "0", "None"

    # Inline versions carry the per-build offset of `generation_stats[]` within
    # `_gc_runtime_state` (computed above) and override `gc_inline_off`; every other
    # version inherits the trait default of 0.
    if is_inline:
        inline_const = (
            f"\n/// Byte offset of the inline `generation_stats[]` array within "
            f"`_gc_runtime_state`,\n/// computed by scripts/gen-offsets.py from this "
            f"build's headers (version-specific).\n"
            f"pub const GC_STATS_INLINE_OFF: u64 = 0x{inline_off:x};\n"
        )
        with open(out_file, "a") as f:
            f.write(inline_const)
        inline_off_fn = "\n    fn gc_inline_off(&self) -> u64 { GC_STATS_INLINE_OFF }"
        print(f"  inline generation_stats at 0x{inline_off:x} "
              f"({inline_off}) within _gc_runtime_state", file=sys.stderr)
    else:
        inline_off_fn = ""

    view_impl = f"""
// -- DebugOffsetsView: per-version dispatch (see offsets/mod.rs) --
impl crate::remote_debugging::offsets::DebugOffsetsView for _Py_DebugOffsets {{
    fn layout_version(&self) -> u64 {{ 0x{version_hex:08x} }}
    fn threads_main(&self) -> u64 {{ {tm} }}
    fn gc_frame(&self) -> u64 {{ {fr} }}
    fn gc_generation_stats(&self) -> u64 {{ {gs} }}
    fn gc_generation_stats_size(&self) -> u64 {{ {gss} }}
    fn gc_stats_shape(&self) -> crate::remote_debugging::offsets::GcStatsShape {{
        crate::remote_debugging::offsets::GcStatsShape {{
            kind: crate::remote_debugging::offsets::offset_table::GcStatsKind::{kind},
            item_size: {item},
            layout: {layout},
        }}
    }}{inline_off_fn}
}}
"""
    with open(out_file, "a") as f:
        f.write(view_impl)

    print(f"Generated {out_file}  (version hex: 0x{version_hex:08x})")

    # A same-hex second build (this hex already has a nav variant, and we wrote a distinct
    # suffixed/commit-tagged file rather than the bare version file) only contributes a GC
    # layout — print the focused GcCandidate checklist. Otherwise it's a new nav variant.
    same_hex = hex_already_registered(version_hex) and safe_tag != ver_tag
    if same_hex:
        print_same_hex_checklist(
            version_hex=version_hex,
            mod_name=f"v_{safe_tag}",
            gc_kind=kind,
        )
    else:
        print_registration_checklist(
            version_hex=version_hex,
            mod_name=f"v_{safe_tag}",
            variant=variant_name(major, minor, micro, level, serial),
            sub_struct_count=len(sub_structs),
            has_gc_stats=has_gc_stats,
        )


if __name__ == "__main__":
    main()
