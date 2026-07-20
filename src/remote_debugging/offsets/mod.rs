mod display;
pub mod offset_table;
pub mod pre_3_13;
pub mod validation;
mod v_3_13_1;
mod v_3_13_13_53e07256802;
mod v_3_14_4;
mod v_3_15_0a8;
mod v_3_15_0b1;
mod v_3_15_0b1_gcinc;
mod v_3_15_0b3;
mod v_3_15_0b4;
mod v_3_16_0a0;

use std::fmt;
use anyhow::{bail, Result};
use crate::memory::{process, reader};
use crate::remote_debugging::offsets::offset_table::GcItemLayout;
use crate::remote_debugging::version::PythonVersion;

fn read_struct<T>(pid: u32, addr: u64) -> Result<T> {
    let size = std::mem::size_of::<T>();
    let bytes = reader::read_memory(pid, addr, size)?;
    Ok(unsafe { std::ptr::read(bytes.as_ptr() as *const T) })
}

/// Minimum Python version that has `_Py_DebugOffsets` with cookie and version field.
const MIN_DEBUG_OFFSETS_MAJOR: u8 = 3;
const MIN_DEBUG_OFFSETS_MINOR: u8 = 13;

const MAJOR_MINOR_MASK: u64 = 0xffff_0000; // major (bits 24-31) | minor (bits 16-23)

/// The single registry of compiled `_Py_DebugOffsets` layouts: version hex → a
/// constructor that reads the struct from the target and wraps it in the matching
/// `VersionedOffsets` variant. This is the ONE place that lists supported layout hexes;
/// `build_variant`, the exact-match check, and the same-minor fallback all derive from
/// it. Adding a Python version adds one row here (plus its `VersionedOffsets` variant
/// and the `for_each_variant!` / `expected_version` / `validate` / display entries).
#[allow(clippy::type_complexity)]
const LAYOUTS: &[(u64, fn(u32, u64) -> Result<VersionedOffsets>)] = &[
    (0x030d01f0, |p, a| Ok(VersionedOffsets::V3_13_1(read_struct(p, a)?))),
    (0x030d0df0, |p, a| Ok(VersionedOffsets::V3_13_13(read_struct(p, a)?))),
    (0x030e04f0, |p, a| Ok(VersionedOffsets::V3_14_4(read_struct(p, a)?))),
    (0x030f00a8, |p, a| Ok(VersionedOffsets::V3_15_0a8(read_struct(p, a)?))),
    (0x030f00b1, |p, a| Ok(VersionedOffsets::V3_15_0b1(read_struct(p, a)?))),
    (0x030f00b3, |p, a| Ok(VersionedOffsets::V3_15_0b3(read_struct(p, a)?))),
    (0x030f00b4, |p, a| Ok(VersionedOffsets::V3_15_0b4(read_struct(p, a)?))),
    (0x031000a0, |p, a| Ok(VersionedOffsets::V3_16_0a0(read_struct(p, a)?))),
];

/// A candidate GC-stats layout for a version hex that has more than one compiled
/// `gc_generation_stats` shape — e.g. a clean release and a GC-instrumented `+inc`
/// build sharing a `PY_VERSION_HEX` and an identical `_Py_DebugOffsets` (so one nav
/// variant serves both), differing only in the per-slot stats struct. The correct one
/// is chosen at read-time by `select_gc_shape`.
struct GcCandidate {
    kind: offset_table::GcStatsKind,
    item_size: u64,
    layout: &'static GcItemLayout,
}

/// Hexes with MORE THAN ONE compiled `gc_generation_stats` layout. A hex absent here
/// has exactly one layout — the nav variant's own `gc_stats_shape()` — and skips
/// selection entirely (zero overhead, unchanged behavior). Invariant: within one entry
/// the candidates must have DISTINCT expected ring sizes (checked by the test below),
/// because the process-published ring size is the only out-of-process discriminator.
const GC_CANDIDATES: &[(u64, &[GcCandidate])] = &[
    // 3.15.0b1: clean release (64-byte stats) vs the gc-gen `+inc` build (208-byte
    // stats). Both report 0x030f00b1 with a byte-identical `_Py_DebugOffsets`; only the
    // `gc_generation_stats` struct differs (extended GC instrumentation in `+inc`).
    (0x030f00b1, &[
        GcCandidate {
            kind: offset_table::GcStatsKind::RingBuffer,
            item_size: v_3_15_0b1::GC_ITEM_SIZE as u64,
            layout: &v_3_15_0b1::GC_LAYOUT,
        },
        GcCandidate {
            kind: offset_table::GcStatsKind::RingBuffer,
            item_size: v_3_15_0b1_gcinc::GC_ITEM_SIZE as u64,
            layout: &v_3_15_0b1_gcinc::GC_LAYOUT,
        },
    ]),
];

/// Whether `stored` (a `PY_VERSION_HEX`) has an EXACT compiled layout in `LAYOUTS`.
///
/// `read_offsets` succeeds for both exact matches and validated same-minor
/// fallbacks; callers that need to distinguish the two tiers (e.g. `PySession`
/// tagging `Full` vs `LayoutOnly`) ask here.
pub fn has_exact_layout(stored: u64) -> bool {
    LAYOUTS.iter().any(|(h, _)| *h == stored)
}

/// Read the target's `_Py_DebugOffsets` through the compiled struct for `layout_hex`.
/// Returns `None` if gcscope has no layout for `layout_hex`.
fn build_variant(pid: u32, addr: u64, layout_hex: u64) -> Result<Option<VersionedOffsets>> {
    match LAYOUTS.iter().find(|(h, _)| *h == layout_hex) {
        Some((_, ctor)) => Ok(Some(ctor(pid, addr)?)),
        None => Ok(None),
    }
}

/// Resolved fallback layout hexes for (pid, stored-version) pairs that have no exact
/// compiled layout — so the resolution and its warning happen once per process, not on
/// every poll.
static FALLBACK_CACHE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(u32, u64), u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// `PY_RELEASE_LEVEL_FINAL` — the level nibble of a released (non-pre-release) build.
const RELEASE_LEVEL_FINAL: u64 = 0xF;

fn release_level(hex: u64) -> u64 {
    (hex >> 4) & 0xf
}

/// Graceful degradation for a 3.13+ build with no exact compiled layout — **only
/// between patch releases of the same minor**.
///
/// The boundary is CPython's own stability rule, not a heuristic. Within a released
/// line (3.15.0, 3.15.1, 3.15.2 …) the structs `_Py_DebugOffsets` describes are
/// ABI-frozen, so any final same-minor layout describes the target correctly. During a
/// pre-release cycle they are not: 3.15.0b1 shrank `gc_generation_stats` from 96 bytes
/// to 64, and 3.15.0b4 inserted `last_profiled_frame_seq` into `_thread_state`, shifting
/// every later field by 8 bytes. Substituting *any* other layout for a pre-release is
/// therefore guesswork that fails open — it reads mapped memory and returns plausible
/// nonsense — so an alpha/beta/rc with no exact layout is refused outright.
///
/// This also means a final build never borrows a pre-release layout: 3.15.0 final may
/// differ from 3.15.0rc1, and only an exact entry can say otherwise.
fn resolve_fallback_layout(stored: u64) -> Result<u64> {
    if release_level(stored) != RELEASE_LEVEL_FINAL {
        bail!(
            "Unsupported Python version {:#010x}: this is a pre-release (alpha/beta/rc), \
             and `_Py_DebugOffsets` changes between pre-releases — no other {}.{} layout \
             can stand in for it. Generate offsets for this exact build with \
             scripts/gen-offsets.py.",
            stored,
            (stored >> 24) & 0xff,
            (stored >> 16) & 0xff
        );
    }

    let tgt_mm = stored & MAJOR_MINOR_MASK;
    LAYOUTS
        .iter()
        .map(|(h, _)| *h)
        .filter(|h| h & MAJOR_MINOR_MASK == tgt_mm)
        .filter(|h| release_level(*h) == RELEASE_LEVEL_FINAL)
        // Any final same-minor layout is equally correct; pick deterministically —
        // nearest, preferring the lower one on a tie.
        .min_by_key(|h| (*h > stored, h.abs_diff(stored)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unsupported Python version {:#010x}: no exact layout, and no *released* \
                 {}.{}.x layout is compiled in to fall back to. Generate offsets for this \
                 build with scripts/gen-offsets.py.",
                stored,
                (stored >> 24) & 0xff,
                (stored >> 16) & 0xff
            )
        })
}

pub fn read_offsets(pid: u32, version: &PythonVersion) -> Result<(u64, u64, VersionedOffsets)> {
    if version.major < MIN_DEBUG_OFFSETS_MAJOR
        || (version.major == MIN_DEBUG_OFFSETS_MAJOR && version.minor < MIN_DEBUG_OFFSETS_MINOR)
    {
        bail!(
            "Python {}.{} does not support _Py_DebugOffsets",
            version.major, version.minor
        );
    }

    let addr = process::find_runtime(pid)?;
    let version_bytes = reader::read_memory(pid, addr + 8, 8)?;
    let stored = u64::from_le_bytes(version_bytes[..].try_into()?);

    let detected = PythonVersion::from_hex(stored)
        .ok_or_else(|| anyhow::anyhow!("Invalid version value in _Py_DebugOffsets: {:#x}", stored))?;

    // For >=3.13 the in-memory `_Py_DebugOffsets` version (`stored`) is authoritative
    // and drives the dispatch below. `version` comes from `version::detect`, which
    // reads the on-disk binary — that can legitimately disagree (in-place upgrade of
    // a running process, or a best-effort version-string scan that mis-hit). So a
    // mismatch is advisory, not fatal: warn and trust the live value.
    if detected.major != version.major || detected.minor != version.minor {
        eprintln!(
            "warning: on-disk version {}.{}.{} differs from the live _Py_DebugOffsets \
             {}.{}.{}; trusting the live value.",
            version.major, version.minor, version.micro,
            detected.major, detected.minor, detected.micro,
        );
    }

    // Resolve which compiled layout to read the struct through: exact hex match,
    // else the closest same-minor fallback (cached per pid+version so the warning
    // happens once, not on every poll).
    let layout_hex = if LAYOUTS.iter().any(|(h, _)| *h == stored) {
        stored
    } else if let Some(&h) = FALLBACK_CACHE.lock().unwrap().get(&(pid, stored)) {
        h
    } else {
        let h = resolve_fallback_layout(stored)?;
        eprintln!(
            "warning: no exact offsets for Python {:#010x}; using released layout \
             {:#010x} (same {}.{} line, where `_Py_DebugOffsets` is ABI-frozen).",
            stored, h, (stored >> 24) & 0xff, (stored >> 16) & 0xff
        );
        FALLBACK_CACHE.lock().unwrap().insert((pid, stored), h);
        h
    };

    let offsets = build_variant(pid, addr, layout_hex)?
        .expect("layout_hex is an exact KNOWN_LAYOUT_HEXES entry or a validated fallback");
    Ok((addr, stored, offsets))
}

#[derive(Debug)]
pub enum VersionedOffsets {
    V3_13_1(v_3_13_1::_Py_DebugOffsets),
    V3_13_13(v_3_13_13_53e07256802::_Py_DebugOffsets),
    V3_14_4(v_3_14_4::_Py_DebugOffsets),
    V3_15_0a8(v_3_15_0a8::_Py_DebugOffsets),
    V3_15_0b1(v_3_15_0b1::_Py_DebugOffsets),
    V3_15_0b3(v_3_15_0b3::_Py_DebugOffsets),
    V3_15_0b4(v_3_15_0b4::_Py_DebugOffsets),
    V3_16_0a0(v_3_16_0a0::_Py_DebugOffsets),
}

/// Shape of a version's GC generation-stats region, as data.
pub struct GcStatsShape {
    pub kind: offset_table::GcStatsKind,
    /// Per-slot size in bytes; 0 when `kind` is `None`.
    pub item_size: u64,
    /// Per-slot field layout; `None` when unavailable (no readable stats).
    pub layout: Option<&'static GcItemLayout>,
}

/// Per-version behavior that varies by build, implemented **once per generated
/// `_Py_DebugOffsets` struct** (emitted by `scripts/gen-offsets.py`). `VersionedOffsets`
/// delegates to this via `for_each_variant!`, so these version-specific offsets and the
/// GC-stats shape need no per-version arms in `mod.rs` — the impl lives in the generated
/// `v_*.rs` file. `0` / `None` returns encode a field that this version's struct lacks.
pub trait DebugOffsetsView {
    /// The registered layout version hex this struct was generated for. (For a
    /// same-minor fallback this is the *layout's* version, not the target's.)
    fn layout_version(&self) -> u64;
    /// `interpreter_state.threads_main`, or 0 if absent (3.13.x).
    fn threads_main(&self) -> u64;
    /// `gc.frame`, or 0 if absent (3.13.x, 3.14.x).
    fn gc_frame(&self) -> u64;
    /// `gc.generation_stats` pointer offset (ring-buffer versions), else 0.
    fn gc_generation_stats(&self) -> u64;
    /// `gc.generation_stats_size` value offset (ring-buffer versions), else 0.
    fn gc_generation_stats_size(&self) -> u64;
    /// The GC generation-stats region shape (kind + per-slot size + field layout).
    fn gc_stats_shape(&self) -> GcStatsShape;
    /// For `InlineArray` versions: byte offset of `generation_stats[]` within
    /// `_gc_runtime_state`. Version-specific and computed by `gen-offsets.py`
    /// (3.13 = 0x80, 3.14 = 0x78). Defaults to 0 for versions
    /// with no inline array (ring-buffer or None), which never read it.
    fn gc_inline_off(&self) -> u64 { 0 }
}

// ── Field accessors ───────────────────────────────────────────────

/// Fan a single expression out across every `VersionedOffsets` variant.
///
/// The variant list lives here, in ONE place. Adding a Python version means
/// adding one arm to this macro (plus the enum variant and the `read_offsets`
/// arm) — uniform accessors below pick it up automatically. Use this only for
/// fields present on *every* variant; fields that some versions lack keep an
/// explicit grouped `match` (see the divergent accessors further down).
macro_rules! for_each_variant {
    ($self:expr, $o:ident => $body:expr) => {
        match $self {
            Self::V3_13_1($o) => $body,
            Self::V3_13_13($o) => $body,
            Self::V3_14_4($o) => $body,
            Self::V3_15_0a8($o) => $body,
            Self::V3_15_0b1($o) => $body,
            Self::V3_15_0b3($o) => $body,
            Self::V3_15_0b4($o) => $body,
            Self::V3_16_0a0($o) => $body,
        }
    };
}

impl VersionedOffsets {
    pub fn expected_version(&self) -> u64 {
        for_each_variant!(self, o => o.layout_version())
    }

    pub fn validate(&self) -> validation::ValidationReport {
        let expected = self.expected_version();
        match self {
            // 3.13.x / 3.14.x: no full validate macro, do basic check
            Self::V3_13_1(o) => validate_basic(o, expected),
            Self::V3_13_13(o) => validate_basic(o, expected),
            Self::V3_14_4(o) => validate_basic(o, expected),
            // clean 3.15.0a8 is basic tier (20 sub-structs, no full validate macro)
            Self::V3_15_0a8(o) => validate_basic(o, expected),
            // 3.15.0b1+ has the full validate macro
            Self::V3_15_0b1(o) => v_3_15_0b1::validate_offsets(o, expected),
            Self::V3_15_0b3(o) => v_3_15_0b3::validate_offsets(o, expected),
            Self::V3_15_0b4(o) => v_3_15_0b4::validate_offsets(o, expected),
            Self::V3_16_0a0(o) => v_3_16_0a0::validate_offsets(o, expected),
        }
    }

    // ── Uniform accessors: field present on every version ───────────────
    // The variant list is enumerated once, in `for_each_variant!` above.

    pub fn runtime_interpreters_head(&self) -> u64 {
        for_each_variant!(self, o => o.runtime_state.interpreters_head)
    }

    pub fn runtime_state_finalizing(&self) -> u64 {
        for_each_variant!(self, o => o.runtime_state.finalizing)
    }

    pub fn interpreter_state_gc(&self) -> u64 {
        for_each_variant!(self, o => o.interpreter_state.gc)
    }

    pub fn interpreter_state_next(&self) -> u64 {
        for_each_variant!(self, o => o.interpreter_state.next)
    }

    pub fn interpreter_state_id(&self) -> u64 {
        for_each_variant!(self, o => o.interpreter_state.id)
    }

    pub fn interpreter_state_threads_head(&self) -> u64 {
        for_each_variant!(self, o => o.interpreter_state.threads_head)
    }

    #[allow(dead_code)]
    pub fn thread_state_interp(&self) -> u64 {
        for_each_variant!(self, o => o.thread_state.interp)
    }

    #[allow(dead_code)]
    pub fn runtime_state_size(&self) -> u64 {
        for_each_variant!(self, o => o.runtime_state.size)
    }

    pub fn interpreter_state_size(&self) -> u64 {
        for_each_variant!(self, o => o.interpreter_state.size)
    }

    pub fn gc_size(&self) -> u64 {
        for_each_variant!(self, o => o.gc.size)
    }

    pub fn gc_collecting(&self) -> u64 {
        for_each_variant!(self, o => o.gc.collecting)
    }

    // ── Divergent accessors: delegate to each struct's `DebugOffsetsView` impl ──
    // The per-version logic (which versions have the field) lives in the generated
    // `v_*.rs` files, not here — see the trait above.

    pub fn interpreter_state_threads_main(&self) -> u64 {
        for_each_variant!(self, o => o.threads_main())
    }

    pub fn gc_generation_stats(&self) -> u64 {
        for_each_variant!(self, o => o.gc_generation_stats())
    }

    pub fn gc_generation_stats_size(&self) -> u64 {
        for_each_variant!(self, o => o.gc_generation_stats_size())
    }

    #[allow(dead_code)]
    pub fn gc_frame(&self) -> u64 {
        for_each_variant!(self, o => o.gc_frame())
    }

    /// Byte offset of the inline `generation_stats[]` array within `_gc_runtime_state`
    /// (InlineArray versions only; 0 otherwise). See `DebugOffsetsView::gc_inline_off`.
    pub fn gc_inline_off(&self) -> u64 {
        for_each_variant!(self, o => o.gc_inline_off())
    }

    /// The `gc` sub-struct fields as `(name, absolute byte offset within
    /// `_Py_DebugOffsets`)`, version-correct. Used to drive the diagram's GC-state
    /// subtree from actual layout instead of hardcoded offsets.
    ///
    /// The `gc` sub-struct is append-only across CPython versions (`size`@0,
    /// `collecting`@8, `frame`@16, `generation_stats_size`@24, `generation_stats`@32);
    /// older builds simply have a shorter struct (3.13/3.14 = `size`, `collecting`
    /// only). So a version has a field iff its `gc` sub-struct is large enough to
    /// contain it. Both `gc_off` and `gc_size` are compile-time constants of that
    /// variant's own generated types, so the match is exhaustive (a new version fails
    /// to compile until it adds an arm).
    pub fn gc_debug_fields(&self) -> Vec<(&'static str, u64)> {
        use std::mem::{offset_of, size_of};
        fn build(gc_off: usize, gc_size: usize) -> Vec<(&'static str, u64)> {
            const CANON: &[(&str, usize)] = &[
                ("size", 0),
                ("collecting", 8),
                ("frame", 16),
                ("generation_stats_size", 24),
                ("generation_stats", 32),
            ];
            CANON
                .iter()
                .filter(|(_, off)| *off < gc_size)
                .map(|(name, off)| (*name, (gc_off + off) as u64))
                .collect()
        }
        match self {
            Self::V3_13_1(_) => build(
                offset_of!(v_3_13_1::_Py_DebugOffsets, gc),
                size_of::<v_3_13_1::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_13_13(_) => build(
                offset_of!(v_3_13_13_53e07256802::_Py_DebugOffsets, gc),
                size_of::<v_3_13_13_53e07256802::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_14_4(_) => build(
                offset_of!(v_3_14_4::_Py_DebugOffsets, gc),
                size_of::<v_3_14_4::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_15_0a8(_) => build(
                offset_of!(v_3_15_0a8::_Py_DebugOffsets, gc),
                size_of::<v_3_15_0a8::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_15_0b1(_) => build(
                offset_of!(v_3_15_0b1::_Py_DebugOffsets, gc),
                size_of::<v_3_15_0b1::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_15_0b3(_) => build(
                offset_of!(v_3_15_0b3::_Py_DebugOffsets, gc),
                size_of::<v_3_15_0b3::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_15_0b4(_) => build(
                offset_of!(v_3_15_0b4::_Py_DebugOffsets, gc),
                size_of::<v_3_15_0b4::_Py_DebugOffsets__gc>(),
            ),
            Self::V3_16_0a0(_) => build(
                offset_of!(v_3_16_0a0::_Py_DebugOffsets, gc),
                size_of::<v_3_16_0a0::_Py_DebugOffsets__gc>(),
            ),
        }
    }

    pub fn debug_offsets_highlight_regions(&self) -> Vec<(usize, u8, &'static str, usize)> {
        // `cookie`/`interpreters_head`/`next` sit in early sub-structs whose positions
        // are stable across every supported bindgen version (3.13→3.16), so a pinned
        // 3.15 type computes them correctly.
        type DO = v_3_15_0b1::_Py_DebugOffsets;
        type RS = v_3_15_0b1::_Py_DebugOffsets__runtime_state;
        type IS = v_3_15_0b1::_Py_DebugOffsets__interpreter_state;

        let head_off = std::mem::offset_of!(DO, runtime_state)
            + std::mem::offset_of!(RS, interpreters_head);
        let next_off = std::mem::offset_of!(DO, interpreter_state)
            + std::mem::offset_of!(IS, next);

        // The `gc` sub-struct moves every version (568/648/704/744) and is shorter on
        // 3.13/3.14, so derive its span from the version-correct `gc_debug_fields()`
        // rather than a pinned type: first field (`size`) to end of the last present one.
        let gc_fields = self.gc_debug_fields();
        let gc_off = gc_fields.first().map(|&(_, o)| o as usize).unwrap_or(0);
        let gc_end = gc_fields.last().map(|&(_, o)| o as usize + 8).unwrap_or(gc_off);
        let gc_sz = (gc_end - gc_off) as u8; // 16 on 3.13/3.14, 40 on 3.15+

        vec![
            (0, 8, "cookie[8]", 1),
            (head_off, 8, "interpreters_head", 2),
            (next_off, 8, "next", 2),
            (gc_off, gc_sz, "gc", 2),
        ]
    }

    pub fn debug_offsets_total_size(&self) -> u64 {
        for_each_variant!(self, o => std::mem::size_of_val(o) as u64)
    }
}

/// Configure an `OffsetTable` for an inline-array GC stats layout (3.13.x, 3.14.4):
/// one slot per generation, laid out contiguously at a fixed offset from the gc state.
fn set_inline(
    table: &mut offset_table::OffsetTable,
    item_size: u64,
    layout: &'static GcItemLayout,
    inline_off: u64,
) {
    table.gc_stats_kind = offset_table::GcStatsKind::InlineArray;
    table.gc_layout = Some(layout);
    table.gc_item_size = Some(item_size);
    table.gc_slots_per_gen = Some([1, 1, 1]);
    table.gc_gen_base_offsets = Some([0, item_size, 2 * item_size]);
    table.gc_stats_inline_off = inline_off;
    table.gc_stats_addr_is_per_interp = true;
}

/// Expected total `generation_stats_size` (ring byte-count) for a ring layout with the
/// given per-slot `item_size` and free-threaded flag. Mirrors the geometry in `set_ring`
/// and the size guard in `gc_stats.rs`.
fn expected_ring_size(item_size: u64, free_threaded: u64) -> u64 {
    let (young, old) = if free_threaded != 0 { (1u64, 1u64) } else { (11, 3) };
    let slots = [young, old, old];
    let bases = offset_table::compute_ring_base_offsets(item_size, &slots);
    bases[2] + slots[2] * item_size + 8
}

/// Pick the GC-stats shape for `hex` when more than one layout is compiled for it (a
/// clean vs `+inc` build sharing a `PY_VERSION_HEX`). Selection is by the process-
/// published `reported` ring size. Returns `default` — the nav variant's own shape —
/// when the hex has a single layout, when `reported` is 0 (inline/older versions, no
/// ambiguity), or when no candidate matches (a genuinely unregistered build; the
/// `gc_stats.rs` size guard then emits the regenerate warning).
fn select_gc_shape(hex: u64, reported: u64, free_threaded: u64, default: GcStatsShape) -> GcStatsShape {
    if reported == 0 {
        return default;
    }
    let Some((_, cands)) = GC_CANDIDATES.iter().find(|(h, _)| *h == hex) else {
        return default;
    };
    for c in *cands {
        if expected_ring_size(c.item_size, free_threaded) == reported {
            return GcStatsShape { kind: c.kind, item_size: c.item_size, layout: Some(c.layout) };
        }
    }
    default
}

/// Configure an `OffsetTable` for a ring-buffer GC stats layout (3.15.0a8+): per-generation
/// rings whose slot counts depend on whether this is a free-threaded build.
fn set_ring(
    table: &mut offset_table::OffsetTable,
    item_size: u64,
    layout: &'static GcItemLayout,
    free_threaded: u64,
) {
    let (young, old) = if free_threaded != 0 { (1u64, 1u64) } else { (11, 3) };
    let slots = [young, old, old];
    let bases = offset_table::compute_ring_base_offsets(item_size, &slots);
    table.gc_stats_kind = offset_table::GcStatsKind::RingBuffer;
    table.gc_layout = Some(layout);
    table.gc_item_size = Some(item_size);
    table.gc_slots_per_gen = Some(slots);
    table.gc_gen_base_offsets = Some(bases);
    table.gc_stats_addr_is_per_interp = true;
}

impl VersionedOffsets {
    /// Build an `OffsetTable` from this `VersionedOffsets` with GC stats constants.
    /// The caller resolves `gc_stats_addr` per-interpreter using the returned constants.
    pub fn to_offset_table(&self, _pid: u32, _runtime_addr: u64) -> offset_table::OffsetTable {

        let free_threaded: u64 = for_each_variant!(self, o => o.free_threaded);

        // Build base table with navigation fields
        let mut table = offset_table::OffsetTable {
            version_hex: self.expected_version(),
            runtime_interpreters_head: self.runtime_interpreters_head(),
            runtime_gc: None,
            interp_next: self.interpreter_state_next(),
            interp_id: self.interpreter_state_id(),
            interp_threads_head: self.interpreter_state_threads_head(),
            interp_gc: Some(self.interpreter_state_gc()),
            thread_interp: self.thread_state_interp(),
            gc_generations: 0x18,
            gc_collecting: self.gc_collecting(),
            gc_frame: Some(self.gc_frame()),
            gc_stats_kind: offset_table::GcStatsKind::None,
            gc_layout: None,
            gc_stats_addr: None,
            gc_item_size: None,
            gc_slots_per_gen: None,
            gc_gen_base_offsets: None,
            gc_stats_inline_off: 0,
            gc_stats_addr_is_per_interp: false,
        };

        // GC generation-stats geometry comes from each struct's `DebugOffsetsView`
        // impl (generated per version) — kind, per-slot size, and field layout are
        // data, keyed by version rather than struct byte-size. Only the ring slot
        // counts (free-threaded vs GIL) are resolved here. A new version that forgets
        // its impl / `for_each_variant!` arm fails to COMPILE — it cannot panic here.
        // The nav variant's own shape is the default; when this hex has multiple
        // compiled GC layouts (clean vs +inc build), pick the one whose expected ring
        // size matches what the process publishes. Single-layout hexes return the
        // default unchanged.
        let default_shape = for_each_variant!(self, o => o.gc_stats_shape());
        let shape = select_gc_shape(
            self.expected_version(),
            self.gc_generation_stats_size(),
            free_threaded,
            default_shape,
        );
        match (shape.kind, shape.layout) {
            (offset_table::GcStatsKind::InlineArray, Some(layout)) => {
                set_inline(&mut table, shape.item_size, layout, self.gc_inline_off());
            }
            (offset_table::GcStatsKind::RingBuffer, Some(layout)) => {
                set_ring(&mut table, shape.item_size, layout, free_threaded);
            }
            // `None`, or a kind with no layout (e.g. a build whose GC_LAYOUT wasn't
            // generated) → leave the table's GC fields empty (no stats).
            _ => {}
        }

        table
    }
}

fn fmt_debug_offsets_basic(o: &dyn BasicDisplay, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    fn _fmt64(val: u64) -> String {
        if val == 0 { "0".to_string() } else { format!("{}", val) }
    }
    writeln!(f, "cookie:             \"xdebugpy\" ✓")?;
    writeln!(f, "version:            {}", o.offsets_version())?;
    writeln!(f, "free_threaded:      {}", o.free_threaded())?;
    Ok(())
}

trait BasicDisplay {
    fn offsets_version(&self) -> u64;
    fn free_threaded(&self) -> u64;
}

macro_rules! impl_basic_display {
    ($ty:ty) => {
        impl BasicDisplay for $ty {
            fn offsets_version(&self) -> u64 { self.version }
            fn free_threaded(&self) -> u64 { self.free_threaded }
        }
    };
}

impl_basic_display!(v_3_13_1::_Py_DebugOffsets);
impl_basic_display!(v_3_13_13_53e07256802::_Py_DebugOffsets);
impl_basic_display!(v_3_14_4::_Py_DebugOffsets);
impl_basic_display!(v_3_15_0a8::_Py_DebugOffsets);
impl_basic_display!(v_3_15_0b1::_Py_DebugOffsets);
impl_basic_display!(v_3_16_0a0::_Py_DebugOffsets);

fn validate_basic<T>(off: &T, expected_version: u64) -> validation::ValidationReport
where T: BasicOffsets {
    let cookie_str = off.cookie_str();
    let cookie_ok = cookie_str == "xdebugpy";
    let version_ok = off.offsets_version() == expected_version;
    let mut checks = Vec::new();
    checks.push(validation::Check::new("cookie", cookie_ok, if cookie_ok { "\"xdebugpy\"" } else { &cookie_str }));
    checks.push(validation::Check::new("version", version_ok, format!("{:#x}", off.offsets_version())));
    validation::ValidationReport { checks }
}

trait BasicOffsets {
    fn cookie_str(&self) -> String;
    fn offsets_version(&self) -> u64;
}

macro_rules! impl_basic_offsets {
    ($ty:ty) => {
        impl BasicOffsets for $ty {
            fn cookie_str(&self) -> String {
                let bytes: &[u8] = unsafe { ::std::slice::from_raw_parts(self.cookie.as_ptr() as *const u8, self.cookie.len()) };
                ::std::string::String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
            }
            fn offsets_version(&self) -> u64 { self.version }
        }
    };
}

impl_basic_offsets!(v_3_13_1::_Py_DebugOffsets);
impl_basic_offsets!(v_3_13_13_53e07256802::_Py_DebugOffsets);
impl_basic_offsets!(v_3_14_4::_Py_DebugOffsets);
impl_basic_offsets!(v_3_15_0a8::_Py_DebugOffsets);
impl_basic_offsets!(v_3_15_0b1::_Py_DebugOffsets);
impl_basic_offsets!(v_3_16_0a0::_Py_DebugOffsets);

impl fmt::Display for VersionedOffsets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V3_13_1(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_13_13(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_14_4(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_15_0a8(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_15_0b1(o) => fmt::Display::fmt(o, f),
            Self::V3_15_0b3(o) => fmt::Display::fmt(o, f),
            Self::V3_15_0b4(o) => fmt::Display::fmt(o, f),
            Self::V3_16_0a0(o) => fmt::Display::fmt(o, f),
        }
    }
}

// GC generation-stats item layout is now carried by version on `OffsetTable`
// (`gc_layout`, set in `to_offset_table`), keyed by version hex rather than by
// struct byte-size — see the note there. The old size-keyed `resolve_gc_item_layout`
// was removed because same-sized 3.15+ structs can have different field layouts.

#[cfg(test)]
mod gc_candidate_tests {
    use super::*;

    /// The ring size is the only out-of-process discriminator between candidates that
    /// share a hex, so two candidates with the same expected ring size cannot be told
    /// apart — that would silently mis-decode. Enforce distinctness at test time for
    /// both the GIL and free-threaded geometries.
    #[test]
    fn gc_candidates_have_distinct_ring_sizes() {
        for (hex, cands) in GC_CANDIDATES {
            for ft in [0u64, 1] {
                let n = cands.len();
                let mut sizes: Vec<u64> =
                    cands.iter().map(|c| expected_ring_size(c.item_size, ft)).collect();
                sizes.sort_unstable();
                sizes.dedup();
                assert_eq!(
                    sizes.len(), n,
                    "GC_CANDIDATES for {hex:#010x} have colliding ring sizes \
                     (free_threaded={ft}); they cannot be distinguished out-of-process — \
                     one must be dropped."
                );
            }
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// Build every `LAYOUTS` variant from a zeroed buffer in our own address space.
    ///
    /// The generated `layout_version()` / `gc_stats_shape()` impls are hardcoded
    /// constants that never read `self` (e.g. `v_3_15_0b1.rs`), so a zeroed instance
    /// answers them correctly — which is what makes the registry checkable without a
    /// live interpreter. Self-read is the same capability `-1` targeting relies on.
    fn build_all() -> Vec<(u64, VersionedOffsets)> {
        // Comfortably larger than any `_Py_DebugOffsets`; `read_struct` reads
        // exactly `size_of::<T>()` bytes from it.
        let buf = vec![0u8; 64 * 1024];
        LAYOUTS
            .iter()
            .map(|(hex, ctor)| {
                let vo = ctor(std::process::id(), buf.as_ptr() as u64)
                    .unwrap_or_else(|e| panic!("self-read failed for {hex:#010x}: {e}"));
                assert!(
                    vo.debug_offsets_total_size() as usize <= buf.len(),
                    "{hex:#010x} struct outgrew the test buffer"
                );
                (*hex, vo)
            })
            .collect()
    }

    /// Each `LAYOUTS` row must construct the variant whose own `layout_version()`
    /// equals the row's key. This is the ONE registration step the compiler does not
    /// enforce (see CLAUDE.md): a copy-pasted row that keeps the previous variant
    /// builds fine and then decodes a live process through the wrong struct.
    #[test]
    fn every_layouts_row_builds_its_own_version() {
        for (hex, vo) in build_all() {
            assert_eq!(
                vo.expected_version(),
                hex,
                "LAYOUTS row {hex:#010x} builds a variant that reports {:#010x} — \
                 the row and the variant disagree",
                vo.expected_version()
            );
        }
    }

    #[test]
    fn layouts_hexes_are_unique_sorted_and_in_range() {
        let hexes: Vec<u64> = LAYOUTS.iter().map(|(h, _)| *h).collect();

        let mut deduped = hexes.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), hexes.len(), "duplicate hex in LAYOUTS");
        assert_eq!(hexes, deduped, "LAYOUTS must stay sorted by hex");

        for hex in hexes {
            let v = PythonVersion::from_hex(hex)
                .unwrap_or_else(|| panic!("{hex:#010x} is not a decodable PY_VERSION_HEX"));
            // `_Py_DebugOffsets` only exists from 3.13; anything older belongs in
            // `pre_3_13.rs`, not here.
            assert!(
                (v.major, v.minor) >= (MIN_DEBUG_OFFSETS_MAJOR, MIN_DEBUG_OFFSETS_MINOR),
                "{hex:#010x} ({v}) predates _Py_DebugOffsets"
            );
            assert!(has_exact_layout(hex), "{hex:#010x} is in LAYOUTS but not exact-matched");
        }
    }

    #[test]
    fn has_exact_layout_rejects_an_unregistered_build() {
        // A plausible 3.13 micro we have never generated offsets for.
        assert!(!has_exact_layout(0x030d02f0));
    }

    /// The permitted case: an unregistered **patch** release of a shipped line.
    /// `_Py_DebugOffsets` is ABI-frozen across 3.13.0/3.13.1/3.13.2…, so any released
    /// same-minor layout describes the target correctly.
    #[test]
    fn fallback_substitutes_between_released_patch_versions() {
        assert_eq!(resolve_fallback_layout(0x030d02f0).unwrap(), 0x030d01f0, "3.13.2");
        assert_eq!(resolve_fallback_layout(0x030d0cf0).unwrap(), 0x030d01f0, "3.13.12");
        assert_eq!(resolve_fallback_layout(0x030e09f0).unwrap(), 0x030e04f0, "3.14.9");
    }

    /// Regression test for a shipped bug, and for the rule that replaced the fix.
    ///
    /// The metric was once micro ONLY, so every 3.15 pre-release tied at micro 0 and the
    /// first row won — a8, whose `gc_generation_stats` is 96 bytes against b1+'s 64. A
    /// 3.15.0b4 target decoded through it reported pure garbage while CI stayed green.
    /// Ordering the candidates was not enough either: b4 inserted
    /// `last_profiled_frame_seq` into `_thread_state`, shifting every later field by 8
    /// bytes, so *no* neighbouring layout fits. Pre-releases must be registered exactly.
    #[test]
    fn fallback_refuses_every_prerelease() {
        for hex in [
            0x030f00b5u64, // beta after our newest beta
            0x030f00a9,    // alpha
            0x030f00c1,    // release candidate
            0x031000a1,    // 3.16 alpha
        ] {
            assert!(
                resolve_fallback_layout(hex).is_err(),
                "{hex:#010x} is a pre-release and must not borrow another layout"
            );
        }
    }

    /// A released build must not borrow a pre-release layout either: 3.15.0 final may
    /// differ from any of the 3.15 betas, and only an exact entry could say otherwise.
    /// Every compiled 3.15 layout is a pre-release today, so 3.15.0 final has nothing
    /// legitimate to fall back to.
    #[test]
    fn a_released_build_never_borrows_a_prerelease_layout() {
        assert!(LAYOUTS
            .iter()
            .any(|(h, _)| h & MAJOR_MINOR_MASK == 0x030f_0000),
            "test assumes some 3.15 layout is registered");
        assert!(resolve_fallback_layout(0x030f00f0).is_err(), "3.15.0 final");
        assert!(resolve_fallback_layout(0x030f01f0).is_err(), "3.15.1 final");
    }

    #[test]
    fn fallback_refuses_an_unknown_minor() {
        // 3.11 has no bindgen layout at all (it is a `pre_3_13.rs` version), and
        // silently borrowing a 3.13 layout for it would mis-decode every field.
        assert!(resolve_fallback_layout(0x030b00f0).is_err());
        assert!(resolve_fallback_layout(0x031100a0).is_err(), "a future 3.17");
    }
}

#[cfg(test)]
mod gc_shape_tests {
    use super::*;

    /// A stand-in for "the nav variant's own shape" that no candidate can equal, so a
    /// test can tell "fell through to the default" apart from "selected a candidate".
    const SENTINEL: GcStatsShape = GcStatsShape {
        kind: offset_table::GcStatsKind::None,
        item_size: 0xDEAD,
        layout: None,
    };

    fn is_sentinel(s: &GcStatsShape) -> bool {
        s.kind == SENTINEL.kind && s.item_size == SENTINEL.item_size && s.layout.is_none()
    }

    /// The whole point of `select_gc_shape`: for a hex with several compiled GC
    /// layouts, the size the process publishes must select that layout and no other.
    /// This is what keeps a clean 3.15.0b1 from being decoded through the `+inc`
    /// build's stats struct, and vice versa.
    #[test]
    fn reported_ring_size_selects_its_own_candidate() {
        for (hex, cands) in GC_CANDIDATES {
            for ft in [0u64, 1] {
                for c in *cands {
                    let reported = expected_ring_size(c.item_size, ft);
                    let picked = select_gc_shape(*hex, reported, ft, SENTINEL);
                    assert_eq!(
                        picked.item_size, c.item_size,
                        "{hex:#010x} (free_threaded={ft}): reported {reported} bytes \
                         should select the {}-byte layout",
                        c.item_size
                    );
                    assert_eq!(picked.kind, c.kind);
                    assert!(picked.layout.is_some());
                }
            }
        }
    }

    #[test]
    fn zero_reported_size_keeps_the_nav_variants_own_shape() {
        // Inline and pre-ring builds publish no ring size; there is nothing to
        // disambiguate and selection must not touch the default.
        for (hex, _) in GC_CANDIDATES {
            assert!(is_sentinel(&select_gc_shape(*hex, 0, 0, SENTINEL)));
        }
    }

    #[test]
    fn unknown_hex_or_unmatched_size_falls_through_to_the_default() {
        // A hex with a single compiled layout skips selection entirely.
        assert!(is_sentinel(&select_gc_shape(0x030e04f0, 4096, 0, SENTINEL)));
        // A registered hex whose reported size matches no candidate: fall through, so
        // the size guard in `gc_stats.rs` emits the regenerate hint rather than a
        // silently wrong layout being used.
        for (hex, _) in GC_CANDIDATES {
            assert!(is_sentinel(&select_gc_shape(*hex, 12345, 0, SENTINEL)));
        }
    }

    #[test]
    fn every_gc_candidate_hex_is_a_registered_layout() {
        for (hex, _) in GC_CANDIDATES {
            assert!(
                has_exact_layout(*hex),
                "{hex:#010x} has GC candidates but no _Py_DebugOffsets layout to reach them"
            );
        }
    }

    /// Ring geometry is written twice — `expected_ring_size` (used for selection) and
    /// `set_ring` (used for decoding). If they drift, selection picks a layout the
    /// decoder then reads with different bases, and every stat is silently wrong.
    #[test]
    fn expected_ring_size_agrees_with_set_ring() {
        for (_, cands) in GC_CANDIDATES {
            for c in *cands {
                for ft in [0u64, 1] {
                    let mut table = pre_3_13::table_for_version(3, 12).unwrap();
                    set_ring(&mut table, c.item_size, c.layout, ft);
                    let slots = table.gc_slots_per_gen.unwrap();
                    let bases = table.gc_gen_base_offsets.unwrap();
                    assert_eq!(
                        bases[2] + slots[2] * c.item_size + 8,
                        expected_ring_size(c.item_size, ft),
                        "ring geometry drifted (item_size={}, free_threaded={ft})",
                        c.item_size
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use crate::remote_debugging::offsets::validation::Check;

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check named {name:?}"))
    }

    fn all_pass(checks: &[Check]) -> bool {
        checks.iter().all(|c| c.passed)
    }

    /// A synthetic, fully-valid full-tier (`_Py_DebugOffsets` with the validate
    /// macro) layout, built in our own address space. `zeroed()` is sound: every
    /// field is a `u64` or a `[c_char; 8]`, for which all-zero is a valid value.
    /// Field offsets stay 0 so every `field + 8 <= size` bounds check passes; only
    /// the sub-struct sizes and the cookie/version need setting.
    fn valid_full() -> v_3_15_0b1::_Py_DebugOffsets {
        let mut off: v_3_15_0b1::_Py_DebugOffsets = unsafe { std::mem::zeroed() };
        off.cookie = (*b"xdebugpy").map(|b| b as std::os::raw::c_char);
        off.version = 0x030f00b1; // must equal V3_15_0b1's layout_version()
        off.free_threaded = 0;
        // Every sub-struct the macro size-checks must report a non-zero size, and
        // must be large enough that the zero-valued field offsets stay in bounds.
        off.runtime_state.size = 1024;
        off.interpreter_state.size = 1024;
        off.thread_state.size = 1024;
        off.interpreter_frame.size = 1024;
        off.code_object.size = 1024;
        off.pyobject.size = 1024;
        off.type_object.size = 1024;
        off.heap_type_object.size = 1024;
        off.tuple_object.size = 1024;
        off.list_object.size = 1024;
        off.set_object.size = 1024;
        off.dict_object.size = 1024;
        off.float_object.size = 1024;
        off.long_object.size = 1024;
        off.bytes_object.size = 1024;
        off.unicode_object.size = 1024;
        off.gc.size = 1024;
        off.gen_object.size = 1024;
        off
    }

    fn validate_full(off: v_3_15_0b1::_Py_DebugOffsets) -> Vec<Check> {
        VersionedOffsets::V3_15_0b1(off).validate().checks
    }

    /// The all-valid baseline must pass every check. If this fails, the failure
    /// tests below can't be trusted to isolate the field they flip.
    #[test]
    fn a_fully_valid_full_tier_layout_passes_every_check() {
        assert!(all_pass(&validate_full(valid_full())));
    }

    /// A cookie that isn't `"xdebugpy"` fails the cookie check and only that check —
    /// the cookie is the one signal that we're even looking at a `_Py_DebugOffsets`.
    #[test]
    fn a_bad_cookie_fails_only_the_cookie_check() {
        let mut off = valid_full();
        off.cookie[0] = b'Z' as std::os::raw::c_char;
        let checks = validate_full(off);
        assert!(!find(&checks, "cookie").passed);
        assert!(checks.iter().filter(|c| c.name != "cookie").all(|c| c.passed));
    }

    /// A version field that disagrees with the layout the bytes were decoded through
    /// means we picked the wrong struct — the version check must catch it.
    #[test]
    fn a_version_mismatch_fails_only_the_version_check() {
        let mut off = valid_full();
        off.version = 0x030f00b3; // a different released layout
        let checks = validate_full(off);
        assert!(!find(&checks, "version").passed);
        assert!(checks.iter().filter(|c| c.name != "version").all(|c| c.passed));
    }

    /// gcscope only decodes GIL builds through these layouts; a free-threaded build
    /// reports `free_threaded = 1` and has a different ABI, so it must be flagged.
    #[test]
    fn a_free_threaded_flag_fails_only_that_check() {
        let mut off = valid_full();
        off.free_threaded = 1;
        let checks = validate_full(off);
        assert!(!find(&checks, "free_threaded").passed);
        assert!(checks.iter().filter(|c| c.name != "free_threaded").all(|c| c.passed));
    }

    /// A zero sub-struct size means that section was never populated — a decode that
    /// read past the published struct. The size check for that section must fail.
    #[test]
    fn a_zero_section_size_fails_its_size_check() {
        let mut off = valid_full();
        off.gc.size = 0;
        let checks = validate_full(off);
        assert!(!find(&checks, "gc.size").passed);
        // Zeroing the size also drops every gc field out of bounds, so the gc.*
        // bounds checks fail too; no *other* section is affected.
        assert!(checks
            .iter()
            .filter(|c| !c.name.starts_with("gc."))
            .all(|c| c.passed));
    }

    /// A field whose offset lands past the end of its sub-struct would read another
    /// section's bytes. The `field + 8 <= size` bounds check must reject it.
    #[test]
    fn an_out_of_bounds_field_fails_only_its_bounds_check() {
        let mut off = valid_full();
        off.gc.generation_stats = 2000; // > gc.size (1024)
        let checks = validate_full(off);
        assert!(!find(&checks, "gc.generation_stats").passed);
        assert!(checks
            .iter()
            .filter(|c| c.name != "gc.generation_stats")
            .all(|c| c.passed));
    }

    /// A field exactly at the boundary — `offset + 8 == size` — is the last legal
    /// position and must pass; one byte further must not.
    #[test]
    fn a_field_at_the_exact_boundary_is_in_bounds() {
        let mut off = valid_full();
        off.gc.generation_stats = off.gc.size - 8; // 1016 + 8 == 1024
        assert!(find(&validate_full(off), "gc.generation_stats").passed);

        let mut off = valid_full();
        off.gc.generation_stats = off.gc.size - 7; // 1017 + 8 == 1025 > 1024
        assert!(!find(&validate_full(off), "gc.generation_stats").passed);
    }

    // ── Basic tier (validate_basic: cookie + version only) ──────────────────

    fn valid_basic() -> v_3_13_1::_Py_DebugOffsets {
        let mut off: v_3_13_1::_Py_DebugOffsets = unsafe { std::mem::zeroed() };
        off.cookie = (*b"xdebugpy").map(|b| b as std::os::raw::c_char);
        off.version = 0x030d01f0; // V3_13_1's layout_version()
        off
    }

    fn validate_basic_checks(off: v_3_13_1::_Py_DebugOffsets) -> Vec<Check> {
        VersionedOffsets::V3_13_1(off).validate().checks
    }

    /// The basic tier runs exactly two checks — cookie and version. A valid layout
    /// passes both, and nothing else is inspected.
    #[test]
    fn a_valid_basic_tier_layout_passes_its_two_checks() {
        let checks = validate_basic_checks(valid_basic());
        assert_eq!(checks.len(), 2);
        assert!(all_pass(&checks));
    }

    #[test]
    fn basic_tier_catches_a_bad_cookie() {
        let mut off = valid_basic();
        off.cookie[0] = b'Z' as std::os::raw::c_char;
        assert!(!find(&validate_basic_checks(off), "cookie").passed);
    }

    #[test]
    fn basic_tier_catches_a_version_mismatch() {
        let mut off = valid_basic();
        off.version = 0x030d0df0; // 3.13.13's hex, not 3.13.1's
        assert!(!find(&validate_basic_checks(off), "version").passed);
    }
}
