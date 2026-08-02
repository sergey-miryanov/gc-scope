#!/usr/bin/env python3
"""Generate Rust bindings for _Py_DebugOffsets from a CPython checkout.

Usage:
    python scripts/gen-offsets.py X:/path/to/cpython/checkout

Requires bindgen on PATH (install with: cargo install bindgen-cli --version 0.72).
Requires LIBCLANG_PATH set on Windows.
"""

import argparse
import hashlib
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
    return bool(re.search(rf'\(\s*0x{version_hex:08x}\s*,\s*\|p,\s*a\|', mod_rs.read_text(encoding="utf-8")))


# A `LAYOUTS` row, as cargo fmt leaves it — the closure body is wrapped in braces and
# pushed to the next line, so the `{` and the newline are NOT optional whitespace:
#     (0x030e04f0, |p, a| {
#         Ok(VersionedOffsets::V3_14_4(read_struct(p, a)?))
#     }),
# `{hex}` is filled in by `.format()` with either a literal hex or a capture group.
LAYOUTS_ROW_RE = r'\(\s*0x{hex}\s*,\s*\|p,\s*a\|\s*\{{?\s*Ok\(VersionedOffsets::(\w+)\('


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
        if h.exists() and anchor in h.read_text(encoding="utf-8"):
            return h.read_text(encoding="utf-8")
    return None


def _gc_runtime_wrapper(cpython_path: Path) -> str | None:
    """A self-contained compile unit defining `_gc_runtime_state`, or None.

    `_gc_runtime_state` refers to its dependencies by *pointer*, so the referents can be
    forward-declared as opaque and the struct reconstructed without pulling in the rest
    of `Include/internal`. (This does not generalize: `PyInterpreterState` embeds dozens
    of types by value, and an embedded struct cannot be forward-declared.)
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
    return f"""typedef unsigned long long uintptr_t;
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


def compute_gc_runtime_facts(cpython_path: Path) -> tuple[int | None, int | None]:
    """`(offset of generation_stats, sizeof _gc_runtime_state)` for a GIL build.

    The **offset** is version-specific (3.13 = 128, 3.14 = 120, 3.15.0a7 = 104) — each
    release reshuffles the fields ahead of `generation_stats` — and is NOT published by
    `_Py_DebugOffsets` before 3.15, so it must be computed here and compiled in. That makes
    it the one GC quantity with no runtime check on 3.13/3.14.

    The **size** is what those builds do publish, as `gc.size`. It is a change detector,
    not a correctness oracle (ADR 0011), so the runtime tests membership of the swept set
    rather than equality.

    `(None, None)` if the struct can't be reconstructed or bindgen fails; the caller then
    treats the build as having no readable inline stats (`GcStatsKind::None`).
    """
    wrapper = _gc_runtime_wrapper(cpython_path)
    if wrapper is None:
        return None, None
    with tempfile.TemporaryDirectory(prefix="gcscope-inlineoff-") as tmpdir:
        wpath = Path(tmpdir) / "gcrt.h"
        wpath.write_text(wrapper, encoding="utf-8")
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
            return None, None
        text = opath.read_text(encoding="utf-8")
        off = re.search(
            r'offset_of!\(_gc_runtime_state, generation_stats\)\s*-\s*(\d+)usize', text)
        size = re.search(r'size_of::<_gc_runtime_state>\(\)\s*-\s*(\d+)usize', text)
        return (int(off.group(1)) if off else None,
                int(size.group(1)) if size else None)


def compute_inline_stats_off(cpython_path: Path) -> int | None:
    """Byte offset of the inline `generation_stats[]` array within `_gc_runtime_state`."""
    return compute_gc_runtime_facts(cpython_path)[0]


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
        text = f.read_text(encoding="utf-8")
        if "ONGOING dev build" in text:
            m = re.search(r'gcscope-source: \S+@([0-9a-f]{7,40})', text)
            out.append((f.name, m.group(1) if m else "?"))
    return out


def is_ongoing_module(offsets_dir: Path, mod_name: str) -> bool:
    """Whether a registered `v_*` module was generated off a release tag."""
    f = offsets_dir / f"{mod_name}.rs"
    return f.exists() and "ONGOING dev build" in f.read_text(encoding="utf-8")


def _debug_offsets_structs(text: str) -> dict[str, str]:
    """{name: normalized-body} for every `_Py_DebugOffsets*` struct block in `text`.

    This is the ABI-relevant part of a generated module — the nav struct and its
    sub-structs. `gc_generation_stats` is intentionally excluded: it is exactly what a
    same-hex `+inc` candidate is *allowed* to differ in.
    """
    out = {}
    for m in re.finditer(r'pub struct (_Py_DebugOffsets\w*)\s*\{', text):
        o = text.index('{', m.start())
        e = _brace_end(text, o)
        if e < 0:
            continue
        out[m.group(1)] = re.sub(r'\s+', ' ', text[o:e + 1]).strip()
    return out


def registered_nav_module(version_hex: int, mod_rs_text: str) -> str | None:
    """The `v_*` module the LAYOUTS row for `version_hex` navigates through, if any."""
    m = re.search(LAYOUTS_ROW_RE.format(hex=f'{version_hex:08x}'), mod_rs_text)
    if not m:
        return None
    variant = m.group(1)
    m2 = re.search(rf'\b{variant}\((v_\w+)::_Py_DebugOffsets\)', mod_rs_text)
    return m2.group(1) if m2 else None


def _gc_stats_struct(generated: str) -> str | None:
    """The normalized `gc_generation_stats` body from a generated module, or None.

    Deliberately separate from `_debug_offsets_structs`: two builds may share a
    `_Py_DebugOffsets` and differ here (a clean release vs a gc-instrumented `+inc`),
    which is exactly the case that must NOT be aliased.
    """
    m = re.search(r'pub struct gc_generation_stats\s*\{', generated)
    if not m:
        return None
    o = generated.index('{', m.start())
    e = _brace_end(generated, o)
    return re.sub(r'\s+', ' ', generated[o:e + 1]).strip() if e > 0 else None


def registered_layout_hexes(mod_rs_text: str) -> dict[int, str]:
    """`{version_hex: module}` for every layout registered in `LAYOUTS`."""
    out = {}
    for hx, variant in re.findall(LAYOUTS_ROW_RE.format(hex=r'([0-9a-f]{8})'), mod_rs_text):
        m = re.search(rf'\b{variant}\((v_\w+)::_Py_DebugOffsets\)', mod_rs_text)
        out[int(hx, 16)] = m.group(1) if m else "?"
    return out


def layout_signature(cpython_path: Path, *, trust_tags: bool = False) -> dict | None:
    """Everything the sweep needs to decide whether two builds share a layout.

    An alias requires ALL THREE of `block`, `stats` and `inline_off` to match; block
    identity alone is not sufficient (see `_gc_stats_struct`). The block is compared as
    *generated Rust*, not raw C — bindgen has already resolved which header the struct
    lives in (it moved at 3.14) and normalized away comments and no-op attributes like
    `_Py_NONSTRING`, so neither can masquerade as a layout change.
    """
    try:
        v = read_version(cpython_path)
    except (OSError, TypeError, ValueError):
        return None
    if (v["major"], v["minor"]) < (3, 13):
        return None                      # no _Py_DebugOffsets before 3.13
    built = build_wrapper(cpython_path, quiet=True)
    if built is None:
        return None
    wrapper, has_gc_stats = built
    with tempfile.TemporaryDirectory(prefix="gcscope-sweep-") as tmpdir:
        out = Path(tmpdir) / "m.rs"
        if run_bindgen(wrapper, cpython_path, out, has_gc_stats) is not None:
            return None
        generated = out.read_text(encoding="utf-8")
    blocks = _debug_offsets_structs(generated)
    if not blocks:
        return None
    inline_off, grt_size = compute_gc_runtime_facts(cpython_path)
    prov = git_provenance(cpython_path)
    return {
        "hex": v["hex"],
        "version": v["version_str"],
        # Only a release tag is immutable, and only immutable trees may be aliased: a
        # branch tip's equivalence to another layout is true at one commit and rots
        # silently thereafter. `main` currently matches 3.15.0b4 exactly, which is
        # precisely the tempting-but-wrong alias this flag exists to refuse.
        #
        # `trust_tags` is for callers that extracted the sources themselves and so have
        # no `.git` to interrogate — CI enumerates release tags and exports each one, so
        # tag-ness is guaranteed by construction rather than discoverable after the fact.
        "at_tag": trust_tags or bool(prov and prov["at_tag"]),
        "block": hashlib.sha256(
            repr(sorted(blocks.items())).encode()).hexdigest()[:12],
        "stats": hashlib.sha256(
            (_gc_stats_struct(generated) or "").encode()).hexdigest()[:12],
        "inline_off": inline_off,
        "grt_size": grt_size,
    }


def read_version(cpython_path: Path) -> dict:
    """Version fields from `Include/patchlevel.h`, plus the packed `PY_VERSION_HEX`."""
    patchlevel = cpython_path / "Include" / "patchlevel.h"
    lines = patchlevel.read_text(encoding="utf-8").splitlines()
    major = int(get_define(lines, "PY_MAJOR_VERSION"))
    minor = int(get_define(lines, "PY_MINOR_VERSION"))
    micro = int(get_define(lines, "PY_MICRO_VERSION"))
    serial = int(get_define(lines, "PY_RELEASE_SERIAL"))
    level = resolve_release_level(lines, get_define(lines, "PY_RELEASE_LEVEL"))
    return {
        "major": major, "minor": minor, "micro": micro, "level": level, "serial": serial,
        "hex": (major << 24) | (minor << 16) | (micro << 8) | (level << 4) | serial,
        "version_str": get_define(lines, "PY_VERSION").strip('"'),
    }


def build_wrapper(cpython_path: Path, *, quiet: bool = False) -> tuple[str, bool] | None:
    """The bindgen compile unit for this tree's `_Py_DebugOffsets`, and whether
    `gc_generation_stats` was found. None if no offsets header exists (pre-3.13).

    `_Py_DebugOffsets` moved headers: it lives inside `pycore_runtime.h` on 3.13 and in
    its own `pycore_debug_offsets.h` from 3.14. `gc_generation_stats` moved too
    (`pycore_gc.h` on 3.13, `pycore_interp_structs.h` after), so both are searched for
    rather than assumed.
    """
    include_internal = cpython_path / "Include" / "internal"
    include_pc = cpython_path / "PC"

    offsets_header = include_internal / "pycore_debug_offsets.h"
    use_runtime_h = False
    if not offsets_header.exists():
        offsets_header = include_internal / "pycore_runtime.h"
        if not offsets_header.exists():
            return None
        use_runtime_h = True

    gc_stats_header = None
    for _name in ("pycore_interp_structs.h", "pycore_gc.h"):
        _h = include_internal / _name
        if _h.exists() and "struct gc_generation_stats {" in _h.read_text(encoding="utf-8"):
            gc_stats_header = _h
            break
    has_gc_stats = gc_stats_header is not None
    if not has_gc_stats and not quiet:
        print("Note: `struct gc_generation_stats` not found in pycore_interp_structs.h "
              "or pycore_gc.h — GC_LAYOUT will NOT be emitted for this build.",
              file=sys.stderr)

    # For 3.13.x the struct is buried in pycore_runtime.h; extract just its definition so
    # bindgen doesn't have to parse that header's whole dependency tree. `_Py_NONSTRING`
    # is defined away in both branches: 3.13 later added it to `char cookie[8]`, and it is
    # an attribute with no layout effect, so neutralizing it keeps the block comparable
    # across the patch line.
    if use_runtime_h:
        hdr_text = offsets_header.read_text(encoding="utf-8")
        start = hdr_text.find("typedef struct _Py_DebugOffsets {")
        if start == -1:
            start = hdr_text.find("struct _Py_DebugOffsets {")
        end = hdr_text.find("} _Py_DebugOffsets;", start) + len("} _Py_DebugOffsets;")
        struct_text = hdr_text[start:end]
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

    if has_gc_stats:
        text = gc_stats_header.read_text(encoding="utf-8")
        start = text.find("struct gc_generation_stats {")
        if start >= 0:
            end = _brace_end(text, text.index('{', start)) + 1
            if end < len(text) and text[end] == ';':
                end += 1
            wrapper += f"""
// Extracted from {gc_stats_header.name}
typedef long long Py_ssize_t;
typedef long long PyTime_t;
{text[start:end]}"""
    return wrapper, has_gc_stats


def run_bindgen(wrapper: str, cpython_path: Path, out_file: Path,
                has_gc_stats: bool) -> str | None:
    """Generate `out_file` from `wrapper`. Returns bindgen's stderr on failure."""
    include_internal = cpython_path / "Include" / "internal"
    with tempfile.TemporaryDirectory(prefix="gcscope-bindgen-") as tmpdir:
        wrapper_path = Path(tmpdir) / "wrapper.h"
        wrapper_path.write_text(wrapper, encoding="utf-8")
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
            f"-I{cpython_path / 'PC'}",
        ]
        result = subprocess.run(bindgen_args, capture_output=True, text=True)
        return None if result.returncode == 0 else result.stderr


def run_sweep(trees_dir: Path, *, emit: bool, tags_only: bool = False,
              trust_tags: bool = False) -> int:
    """Group CPython trees by layout and report aliases, duplicates and gaps.

    The generation-time half of the fail-closed story (ADR 0011): it replaces the
    patch-freeze *assumption* with a comparison of every tree it is given, and emits the
    two tables `offsets/mod.rs` needs — `ALIASES` (hexes that provably share a registered
    layout) and `VERIFIED_GC_SIZES` (the sizes the inline runtime check accepts).

    Returns a process exit code: non-zero if any tree is uncovered by the registry.
    """
    trees = sorted(d for d in trees_dir.iterdir()
                   if (d / "Include" / "patchlevel.h").exists())
    if tags_only and not trust_tags:
        trees = [t for t in trees
                 if (p := git_provenance(t)) is not None and p["at_tag"]]
    if not trees:
        print(f"No CPython trees under {trees_dir} "
              f"(expected subdirectories with Include/patchlevel.h)", file=sys.stderr)
        return 1

    offsets_dir = Path("src") / "remote_debugging" / "offsets"
    mod_rs = offsets_dir / "mod.rs"
    registered = registered_layout_hexes(mod_rs.read_text(encoding="utf-8")) \
        if mod_rs.exists() else {}

    sigs = []
    for t in trees:
        print(f"  probing {t.name} …", file=sys.stderr)
        s = layout_signature(t, trust_tags=trust_tags)
        if s is not None:
            s["tree"] = t.name
            sigs.append(s)

    # A layout is the (block, stats struct, inline offset) triple; anything sharing it
    # decodes identically, and nothing else may be aliased onto it.
    groups: dict[tuple, list[dict]] = {}
    for s in sigs:
        groups.setdefault((s["block"], s["stats"], s["inline_off"]), []).append(s)

    print(f"\n{len(sigs)} trees with a _Py_DebugOffsets block -> "
          f"{len(groups)} distinct layouts\n")
    header = f"{'tree':24} {'hex':>10} {'block':12} {'stats':12} {'stats@':>7} {'sizeof':>7}"
    aliases: list[tuple[int, int]] = []
    verified_sizes: list[tuple[int, list[int]]] = []
    redundant: list[tuple[str, str]] = []
    uncovered: list[dict] = []

    for i, (key, members) in enumerate(sorted(groups.items(),
                                              key=lambda kv: min(m["hex"] for m in kv[1])), 1):
        anchors = [m for m in members if m["hex"] in registered]
        mods = sorted({registered[m["hex"]] for m in anchors})
        label = f"layout {i}: " + (f"registered as {', '.join(mods)}" if mods
                                   else "NOT REGISTERED")
        print(label)
        print("  " + header)
        for m in sorted(members, key=lambda m: m["hex"]):
            mark = "*" if m["hex"] in registered else " "
            dev = "" if m["at_tag"] else "  (dev, not a tag)"
            print(f" {mark}{m['tree']:24} 0x{m['hex']:08x} {m['block']:12} {m['stats']:12} "
                  f"{str(m['inline_off']):>7} {str(m['grt_size']):>7}{dev}")
        # Only tagged trees contribute to the verified set: a dev tree's sizeof describes
        # a commit nobody can attach to twice.
        sizes = sorted({m["grt_size"] for m in members
                        if m["at_tag"] and m["grt_size"] is not None})
        if key[2] is not None and sizes:
            print(f"   verified gc.size set: {{{', '.join(str(s) for s in sizes)}}} "
                  f"-> generation_stats@{key[2]}")

        # Redundancy is about the *modules*, not the trees: two registered modules
        # encoding one layout means one can go. An ONGOING module is exempt — it must
        # keep its own file even when it currently matches a release (3.16 dev matches
        # 3.15.0b4 today and will stop without warning).
        settled = sorted({mod for m in anchors
                          if not is_ongoing_module(offsets_dir, (mod := registered[m["hex"]]))})
        if len(settled) > 1:
            redundant.append((", ".join(settled), label))
        tagged_anchors = [a for a in anchors if a["at_tag"]]
        if tagged_anchors:
            anchor_hex = min(a["hex"] for a in tagged_anchors)
            aliases += [(m["hex"], anchor_hex) for m in members
                        if m["hex"] not in registered and m["at_tag"]]
            # Only inline builds need this: a ring build publishes its own
            # `generation_stats` offset and is already guarded by the ring-size check.
            if key[2] is not None and sizes:
                verified_sizes.append((anchor_hex, sizes))
        uncovered += [m for m in members
                      if m["hex"] not in registered and not tagged_anchors]
        print()

    if redundant:
        print("REDUNDANT — these modules encode the same layout; keep one:")
        for mods, _ in redundant:
            print(f"  {mods}")
        print()
    if uncovered:
        print("UNCOVERED — no registered layout describes these builds:")
        for m in uncovered:
            print(f"  {m['tree']:24} 0x{m['hex']:08x}  (CPython {m['version']})")
        print("\n  A *final* release is still served by the same-minor fallback; a "
              "pre-release\n  is refused outright. Generate a module for it:\n"
              f"      python {Path(sys.argv[0]).name} <tree>\n")
    if emit and (aliases or verified_sizes):
        print(f"// Generated by --sweep over {len(sigs)} trees under {trees_dir}; "
              f"do not hand-edit.")
        print("// SCOPE: these rows describe ONLY the trees swept in this run. A layout "
              "not\n// represented above has no row here and MERGING is required — "
              "replacing the\n// tables wholesale would silently drop it. To regenerate "
              "them in full, sweep a\n// directory holding every supported tree, "
              "pre-releases included.")
        print("const ALIASES: &[(u64, u64)] = &[")
        for a, anchor in sorted(aliases):
            print(f"    (0x{a:08x}, 0x{anchor:08x}),")
        print("];\n")
        print("const VERIFIED_GC_SIZES: &[(u64, &[u64])] = &[")
        for anchor, sizes in sorted(verified_sizes):
            print(f"    (0x{anchor:08x}, &{sizes}),".replace("[", "[").replace("'", ""))
        print("];")

    return 1 if uncovered else 0


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Generate Rust _Py_DebugOffsets bindings from a CPython checkout.")
    parser.add_argument("cpython_path", type=Path, nargs="?",
                        help="CPython source checkout (must have Include/patchlevel.h).")
    parser.add_argument(
        "--sweep", type=Path, metavar="DIR",
        help="Instead of generating, group every CPython tree under DIR by layout and "
             "report which hexes can be aliased onto a registered module, which modules "
             "are redundant, and which builds nothing covers. Needs only headers — no "
             "build, no interpreter — so it can check every patch release of a minor, "
             "not just the one CI happens to install.")
    parser.add_argument(
        "--emit-aliases", action="store_true",
        help="With --sweep, also print the ALIASES table for offsets/mod.rs.")
    parser.add_argument(
        "--tags-only", action="store_true",
        help="With --sweep, skip trees whose HEAD is not a release tag. What CI wants: "
             "a feature branch can share a PY_VERSION_HEX with a release and would "
             "otherwise be mistaken for it.")
    parser.add_argument(
        "--trust-tags", action="store_true",
        help="With --sweep, treat every tree as a release tag even without a .git to "
             "check. For callers that exported the sources themselves and so know it by "
             "construction. Do NOT use on a directory that may contain a branch tip: "
             "only immutable tags may be aliased.")
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

    if args.sweep:
        sys.exit(run_sweep(args.sweep, emit=args.emit_aliases,
                           tags_only=args.tags_only, trust_tags=args.trust_tags))
    if args.cpython_path is None:
        parser.error("give a CPython checkout to generate from, or --sweep DIR")

    cpython_path = args.cpython_path
    patchlevel = cpython_path / "Include" / "patchlevel.h"
    if not patchlevel.exists():
        print(f"Error: {patchlevel} not found", file=sys.stderr)
        sys.exit(1)

    v = read_version(cpython_path)
    major, minor, micro = v["major"], v["minor"], v["micro"]
    level, serial = v["level"], v["serial"]
    version_hex = v["hex"]
    version_str = v["version_str"]

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

    built = build_wrapper(cpython_path)
    if built is None:
        print("Error: neither pycore_debug_offsets.h nor pycore_runtime.h found",
              file=sys.stderr)
        sys.exit(1)
    wrapper, has_gc_stats = built

    err = run_bindgen(wrapper, cpython_path, out_file, has_gc_stats)
    if err is not None:
        print(err, file=sys.stderr)
        print("bindgen failed", file=sys.stderr)
        sys.exit(1)

    # Embed source provenance right after bindgen's autogen header (line 1), so every
    # module records the exact commit it came from; for an ongoing build CI reads the pin
    # from here.
    if prov is not None:
        text = out_file.read_text(encoding="utf-8")
        nl = text.index("\n") + 1
        # newline="" suppresses the write-side \n -> \r\n translation that would otherwise
        # rewrite every line of a bindgen (LF) module on Windows, churning the whole file.
        out_file.write_text(
            text[:nl]
            + provenance_comment(prov, is_ongoing=is_ongoing,
                                 version_str=version_str, version_hex=version_hex)
            + text[nl:], encoding="utf-8", newline="")

    # Read bindgen output to discover which sub-structs were generated
    generated = out_file.read_text(encoding="utf-8")

    # Same-hex nav-struct guard. A --suffix build shares the registered hex's nav variant
    # (only its GC layout is new), so its `_Py_DebugOffsets` MUST be byte-identical to that
    # variant's. That holds automatically for a frozen base (a tagged 3.15.0b1) but NOT for
    # an ongoing one (3.16 dev): a +inc built on a drifted commit would silently navigate
    # with the wrong offsets. Verify rather than assume.
    if suffix and hex_already_registered(version_hex):
        mod_rs = Path("src") / "remote_debugging" / "offsets" / "mod.rs"
        nav_mod = registered_nav_module(version_hex, mod_rs.read_text(encoding="utf-8"))
        nav_file = (out_file.parent / f"{nav_mod}.rs") if nav_mod else None
        if nav_file and nav_file.exists():
            nav = _debug_offsets_structs(nav_file.read_text(encoding="utf-8"))
            new = _debug_offsets_structs(generated)
            if nav != new:
                differing = sorted(n for n in set(nav) | set(new)
                                   if nav.get(n) != new.get(n))
                print(f"\nError: this build's _Py_DebugOffsets does not match the "
                      f"registered nav variant {nav_mod}.\n"
                      f"  Differing structs: {', '.join(differing) or '(struct set differs)'}\n\n"
                      f"  A same-hex (--suffix) build contributes only a GC layout and "
                      f"navigates through\n  {nav_mod}'s offsets, so the two must share a "
                      f"byte-identical _Py_DebugOffsets — they\n  don't, so the builds are "
                      f"on different base commits. Rebuild this one on the\n  SAME commit as "
                      f"{nav_mod}; for an ongoing base, regenerate {nav_mod} and this build\n"
                      f"  together from one commit. ({out_file.name} was left for inspection.)",
                      file=sys.stderr)
                sys.exit(1)
            print(f"  nav-struct check: _Py_DebugOffsets matches {nav_mod} ✓",
                  file=sys.stderr)
        else:
            print(f"  Warning: could not locate the registered nav module for "
                  f"0x{version_hex:08x}; skipped the nav-struct match check.",
                  file=sys.stderr)

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
        with open(out_file, "a", newline="\n") as f:
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
        with open(out_file, "a", newline="\n") as f:
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
    # `gc.size` is `sizeof(struct _gc_runtime_state)`, published by every 3.13+ build.
    # It is the only quantity an inline build exposes that moves when that struct is
    # restructured, which is what the VERIFIED_GC_SIZES membership check keys on.
    grs = "self.gc.size" if "size:" in gc_body else "0"
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
        with open(out_file, "a", newline="\n") as f:
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
    fn gc_runtime_size(&self) -> u64 {{ {grs} }}
    fn gc_stats_shape(&self) -> crate::remote_debugging::offsets::GcStatsShape {{
        crate::remote_debugging::offsets::GcStatsShape {{
            kind: crate::remote_debugging::offsets::offset_table::GcStatsKind::{kind},
            item_size: {item},
            layout: {layout},
        }}
    }}{inline_off_fn}
}}
"""
    with open(out_file, "a", newline="\n") as f:
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
