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

/// Graceful degradation for a 3.13+ build with no exact compiled layout: pick the
/// closest same-(major,minor) layout. `_Py_DebugOffsets` is self-describing and stable
/// within a minor, so a same-minor layout normally matches the target's real offsets.
fn resolve_fallback_layout(stored: u64) -> Result<u64> {
    let tgt_mm = stored & MAJOR_MINOR_MASK;
    let tgt_micro = ((stored >> 8) & 0xff) as i64;

    LAYOUTS
        .iter()
        .map(|(h, _)| *h)
        .filter(|h| h & MAJOR_MINOR_MASK == tgt_mm)
        // nearest micro wins
        .min_by_key(|h| ((((*h >> 8) & 0xff) as i64) - tgt_micro).unsigned_abs())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unsupported Python version {:#010x}: no exact layout and no same-minor \
                 layout is compiled in. Generate offsets for this exact build with \
                 scripts/gen-offsets.py.",
                stored
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
            "warning: no exact offsets for Python {:#010x}; using the closest known \
             layout {:#010x} (same {}.{}). GC-stat offsets are approximate — a \
             buffer-size warning will appear if they don't match this build.",
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
    checks.push(validation::Check::new("version", version_ok, &format!("{:#x}", off.offsets_version())));
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
