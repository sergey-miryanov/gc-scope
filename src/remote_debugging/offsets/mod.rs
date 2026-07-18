mod display;
pub mod offset_table;
pub mod pre_3_13;
pub mod tables;
pub mod validation;
mod v_3_13_1;
mod v_3_13_13_53e07256802;
mod v_3_14_4;
mod v_3_15_0a7;
mod v_3_15_0a8_33aa2bf477d;
mod v_3_15_0b1_6a660056998;
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

    if detected.major != version.major || detected.minor != version.minor {
        bail!(
            "Python version mismatch: process reports {}.{}.{}, but target is {}.{}.{}",
            detected.major, detected.minor, detected.micro,
            version.major, version.minor, version.micro,
        );
    }

    let offsets = match stored {
        0x030d01f0 => {
            let raw = read_struct::<v_3_13_1::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_13_1(raw)
        }
        0x030d0df0 => {
            let raw = read_struct::<v_3_13_13_53e07256802::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_13_13(raw)
        }
        0x030e04f0 => {
            let raw = read_struct::<v_3_14_4::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_14_4(raw)
        }
        0x030f00a8 => {
            let raw = read_struct::<v_3_15_0a8_33aa2bf477d::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_15_0a8(raw)
        }
        0x030f00b1 => {
            let raw = read_struct::<v_3_15_0b1_6a660056998::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_15_0b1(raw)
        }
        0x031000a0 => {
            let raw = read_struct::<v_3_16_0a0::_Py_DebugOffsets>(pid, addr)?;
            VersionedOffsets::V3_16_0a0(raw)
        }
        _ => bail!("Unsupported Python version {:#x}", stored),
    };
    Ok((addr, stored, offsets))
}

#[derive(Debug)]
pub enum VersionedOffsets {
    V3_13_1(v_3_13_1::_Py_DebugOffsets),
    V3_13_13(v_3_13_13_53e07256802::_Py_DebugOffsets),
    V3_14_4(v_3_14_4::_Py_DebugOffsets),
    V3_15_0a8(v_3_15_0a8_33aa2bf477d::_Py_DebugOffsets),
    V3_15_0b1(v_3_15_0b1_6a660056998::_Py_DebugOffsets),
    V3_16_0a0(v_3_16_0a0::_Py_DebugOffsets),
}

// ── Field accessors ───────────────────────────────────────────────

macro_rules! select {
    ($self:expr, [$( $pat:pat => $val:expr ),+ $(,)?]) => {
        match $self { $( $pat => $val, )+ }
    };
}

impl VersionedOffsets {
    pub fn expected_version(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(_) => 0x030d01f0,
            Self::V3_13_13(_) => 0x030d0df0,
            Self::V3_14_4(_) => 0x030e04f0,
            Self::V3_15_0a8(_) => 0x030f00a8,
            Self::V3_15_0b1(_) => 0x030f00b1,
            Self::V3_16_0a0(_) => 0x031000a0,
        ])
    }

    pub fn validate(&self) -> validation::ValidationReport {
        let expected = self.expected_version();
        select!(self, [
            // 3.13.x: no full validate macro, do basic check
            Self::V3_13_1(o) => validate_basic(o, expected),
            Self::V3_13_13(o) => validate_basic(o, expected),
            // 3.14.x: no full validate macro, do basic check
            Self::V3_14_4(o) => validate_basic(o, expected),
            // 3.15+ has the full validate macro
            Self::V3_15_0a8(o) => v_3_15_0a8_33aa2bf477d::validate_offsets(o, expected),
            Self::V3_15_0b1(o) => v_3_15_0b1_6a660056998::validate_offsets(o, expected),
            Self::V3_16_0a0(o) => v_3_16_0a0::validate_offsets(o, expected),
        ])
    }

    pub fn runtime_interpreters_head(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.runtime_state.interpreters_head,
            Self::V3_13_13(o) => o.runtime_state.interpreters_head,
            Self::V3_14_4(o) => o.runtime_state.interpreters_head,
            Self::V3_15_0a8(o) => o.runtime_state.interpreters_head,
            Self::V3_15_0b1(o) => o.runtime_state.interpreters_head,
            Self::V3_16_0a0(o) => o.runtime_state.interpreters_head,
        ])
    }

    pub fn runtime_state_finalizing(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.runtime_state.finalizing,
            Self::V3_13_13(o) => o.runtime_state.finalizing,
            Self::V3_14_4(o) => o.runtime_state.finalizing,
            Self::V3_15_0a8(o) => o.runtime_state.finalizing,
            Self::V3_15_0b1(o) => o.runtime_state.finalizing,
            Self::V3_16_0a0(o) => o.runtime_state.finalizing,
        ])
    }

    pub fn interpreter_state_gc(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.interpreter_state.gc,
            Self::V3_13_13(o) => o.interpreter_state.gc,
            Self::V3_14_4(o) => o.interpreter_state.gc,
            Self::V3_15_0a8(o) => o.interpreter_state.gc,
            Self::V3_15_0b1(o) => o.interpreter_state.gc,
            Self::V3_16_0a0(o) => o.interpreter_state.gc,
        ])
    }

    pub fn interpreter_state_next(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.interpreter_state.next,
            Self::V3_13_13(o) => o.interpreter_state.next,
            Self::V3_14_4(o) => o.interpreter_state.next,
            Self::V3_15_0a8(o) => o.interpreter_state.next,
            Self::V3_15_0b1(o) => o.interpreter_state.next,
            Self::V3_16_0a0(o) => o.interpreter_state.next,
        ])
    }

    pub fn interpreter_state_id(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.interpreter_state.id,
            Self::V3_13_13(o) => o.interpreter_state.id,
            Self::V3_14_4(o) => o.interpreter_state.id,
            Self::V3_15_0a8(o) => o.interpreter_state.id,
            Self::V3_15_0b1(o) => o.interpreter_state.id,
            Self::V3_16_0a0(o) => o.interpreter_state.id,
        ])
    }

    pub fn interpreter_state_threads_head(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.interpreter_state.threads_head,
            Self::V3_13_13(o) => o.interpreter_state.threads_head,
            Self::V3_14_4(o) => o.interpreter_state.threads_head,
            Self::V3_15_0a8(o) => o.interpreter_state.threads_head,
            Self::V3_15_0b1(o) => o.interpreter_state.threads_head,
            Self::V3_16_0a0(o) => o.interpreter_state.threads_head,
        ])
    }

    pub fn interpreter_state_threads_main(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(_o) => 0, // field doesn't exist in 3.13.x
            Self::V3_13_13(_o) => 0,
            Self::V3_14_4(o) => o.interpreter_state.threads_main,
            Self::V3_15_0a8(o) => o.interpreter_state.threads_main,
            Self::V3_15_0b1(o) => o.interpreter_state.threads_main,
            Self::V3_16_0a0(o) => o.interpreter_state.threads_main,
        ])
    }

    #[allow(dead_code)]
    pub fn thread_state_interp(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.thread_state.interp,
            Self::V3_13_13(o) => o.thread_state.interp,
            Self::V3_14_4(o) => o.thread_state.interp,
            Self::V3_15_0a8(o) => o.thread_state.interp,
            Self::V3_15_0b1(o) => o.thread_state.interp,
            Self::V3_16_0a0(o) => o.thread_state.interp,
        ])
    }

    #[allow(dead_code)]
    pub fn runtime_state_size(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.runtime_state.size,
            Self::V3_13_13(o) => o.runtime_state.size,
            Self::V3_14_4(o) => o.runtime_state.size,
            Self::V3_15_0a8(o) => o.runtime_state.size,
            Self::V3_15_0b1(o) => o.runtime_state.size,
            Self::V3_16_0a0(o) => o.runtime_state.size,
        ])
    }

    /// Returns 0 for versions that don't have `gc.generation_stats` (3.13.x, 3.14.x).
    pub fn gc_generation_stats(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(_) | Self::V3_13_13(_) | Self::V3_14_4(_) => 0,
            Self::V3_15_0a8(o) => o.gc.generation_stats,
            Self::V3_15_0b1(o) => o.gc.generation_stats,
            Self::V3_16_0a0(o) => o.gc.generation_stats,
        ])
    }

    /// Returns 0 for versions that don't have `gc.generation_stats_size` (3.13.x, 3.14.x).
    pub fn gc_generation_stats_size(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(_) | Self::V3_13_13(_) | Self::V3_14_4(_) => 0,
            Self::V3_15_0a8(o) => o.gc.generation_stats_size,
            Self::V3_15_0b1(o) => o.gc.generation_stats_size,
            Self::V3_16_0a0(o) => o.gc.generation_stats_size,
        ])
    }

    pub fn interpreter_state_size(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.interpreter_state.size,
            Self::V3_13_13(o) => o.interpreter_state.size,
            Self::V3_14_4(o) => o.interpreter_state.size,
            Self::V3_15_0a8(o) => o.interpreter_state.size,
            Self::V3_15_0b1(o) => o.interpreter_state.size,
            Self::V3_16_0a0(o) => o.interpreter_state.size,
        ])
    }

    pub fn gc_size(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.gc.size,
            Self::V3_13_13(o) => o.gc.size,
            Self::V3_14_4(o) => o.gc.size,
            Self::V3_15_0a8(o) => o.gc.size,
            Self::V3_15_0b1(o) => o.gc.size,
            Self::V3_16_0a0(o) => o.gc.size,
        ])
    }

    pub fn gc_collecting(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(o) => o.gc.collecting,
            Self::V3_13_13(o) => o.gc.collecting,
            Self::V3_14_4(o) => o.gc.collecting,
            Self::V3_15_0a8(o) => o.gc.collecting,
            Self::V3_15_0b1(o) => o.gc.collecting,
            Self::V3_16_0a0(o) => o.gc.collecting,
        ])
    }

    #[allow(dead_code)]
    pub fn gc_frame(&self) -> u64 {
        select!(self, [
            Self::V3_13_1(_) | Self::V3_13_13(_) | Self::V3_14_4(_) => 0,
            Self::V3_15_0a8(o) => o.gc.frame,
            Self::V3_15_0b1(o) => o.gc.frame,
            Self::V3_16_0a0(o) => o.gc.frame,
        ])
    }

    pub fn debug_offsets_highlight_regions(&self) -> Vec<(usize, u8, &'static str, usize)> {
        // Use any of the 3.15+ struct types for compile-time offset calculations;
        // the actual in-memory layout is the same for the struct fields we access.
        type DO = v_3_15_0b1_6a660056998::_Py_DebugOffsets;
        type RS = v_3_15_0b1_6a660056998::_Py_DebugOffsets__runtime_state;
        type IS = v_3_15_0b1_6a660056998::_Py_DebugOffsets__interpreter_state;
        type GC = v_3_15_0b1_6a660056998::_Py_DebugOffsets__gc;

        let head_off = std::mem::offset_of!(DO, runtime_state)
            + std::mem::offset_of!(RS, interpreters_head);
        let next_off = std::mem::offset_of!(DO, interpreter_state)
            + std::mem::offset_of!(IS, next);
        let gc_off = std::mem::offset_of!(DO, gc);
        let gc_sz = std::mem::size_of::<GC>();

        vec![
            (0, 8, "cookie[8]", 1),
            (head_off, 8, "interpreters_head", 2),
            (next_off, 8, "next", 2),
            (gc_off, gc_sz as u8, "gc", 2),
        ]
    }

    pub fn debug_offsets_total_size(&self) -> u64 {
        match self {
            Self::V3_13_1(o) => std::mem::size_of_val(o) as u64,
            Self::V3_13_13(o) => std::mem::size_of_val(o) as u64,
            Self::V3_14_4(o) => std::mem::size_of_val(o) as u64,
            Self::V3_15_0a8(o) => std::mem::size_of_val(o) as u64,
            Self::V3_15_0b1(o) => std::mem::size_of_val(o) as u64,
            Self::V3_16_0a0(o) => std::mem::size_of_val(o) as u64,
        }
    }
}

/// Mirrors the C `_Py_GC_GenerationStats` struct layout.
/// All fields are pub so external code can use `offset_of!`.
#[repr(C)]
pub struct GcGenerationStatsSlot {
    pub ts_start: u64,
    pub ts_stop: u64,
    pub collections: u64,
    pub collected: u64,
    pub uncollectable: u64,
    pub candidates: u64,
    pub duration: f64,
    pub heap_size: u64,
}

impl VersionedOffsets {
    /// Build an `OffsetTable` from this `VersionedOffsets` with GC stats constants.
    /// The caller resolves `gc_stats_addr` per-interpreter using the returned constants.
    pub fn to_offset_table(&self, _pid: u32, _runtime_addr: u64) -> offset_table::OffsetTable {

        let free_threaded: u64 = select!(self, [
            Self::V3_13_1(o) => o.free_threaded,
            Self::V3_13_13(o) => o.free_threaded,
            Self::V3_14_4(o) => o.free_threaded,
            Self::V3_15_0a8(o) => o.free_threaded,
            Self::V3_15_0b1(o) => o.free_threaded,
            Self::V3_16_0a0(o) => o.free_threaded,
        ]);

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
            gc_stats_addr: None,
            gc_item_size: None,
            gc_slots_per_gen: None,
            gc_gen_base_offsets: None,
            gc_stats_addr_is_per_interp: false,
        };

        // Fill GC stats constants based on version (address resolved per-interpreter by caller)
        let version_hex = self.expected_version();
        match version_hex {
            0x030d01f0 | 0x030d0df0 => {}  // 3.13.x — no generation_stats
            0x030e04f0 | 0x030f00a7 => {
                // Inline array (3.14, 3.15.0a7)
                let item_size = if version_hex == 0x030f00a7 { 40u64 } else { 24u64 };
                table.gc_item_size = Some(item_size);
                table.gc_slots_per_gen = Some([1, 1, 1]);
                table.gc_gen_base_offsets = Some([0, item_size, 2 * item_size]);
                table.gc_stats_addr_is_per_interp = true;
            }
            _ => {
                // Ring buffer (3.15.0b1+, 3.16.0a0, gc-gen-3.15+inc)
                let gc_item_size = match version_hex {
                    0x030f00a8 => v_3_15_0a8_33aa2bf477d::GC_ITEM_SIZE as u64,
                    0x030f00b1 => 64u64,
                    0x031000a0 => v_3_16_0a0::GC_ITEM_SIZE as u64,
                    _ => 64u64,
                };
                let (young, old) = if free_threaded != 0 { (1, 1) } else { (11, 3) };
                let slots = [young as u64, old as u64, old as u64];
                let bases = offset_table::compute_ring_base_offsets(gc_item_size, &slots);
                table.gc_item_size = Some(gc_item_size);
                table.gc_slots_per_gen = Some(slots);
                table.gc_gen_base_offsets = Some(bases);
                table.gc_stats_addr_is_per_interp = true;
            }
        }

        table
    }

    /// Returns slot-relative (offset, size, label) for highlighted GC generation stats fields.
    /// Skips `uncollectable` and `candidates` per user preference.
    pub fn gc_slot_highlight_regions(&self) -> Vec<(usize, u8, &'static str)> {
        use std::mem::{offset_of, size_of};
        type S = GcGenerationStatsSlot;
        vec![
            (offset_of!(S, ts_start), size_of::<u64>() as u8, "ts_start"),
            (offset_of!(S, ts_stop), size_of::<u64>() as u8, "ts_stop"),
            (offset_of!(S, collections), size_of::<u64>() as u8, "collections"),
            (offset_of!(S, collected), size_of::<u64>() as u8, "collected"),
            (offset_of!(S, duration), size_of::<f64>() as u8, "duration"),
            (offset_of!(S, heap_size), size_of::<u64>() as u8, "heap_size"),
        ]
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
impl_basic_display!(v_3_15_0a8_33aa2bf477d::_Py_DebugOffsets);
impl_basic_display!(v_3_15_0b1_6a660056998::_Py_DebugOffsets);
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
impl_basic_offsets!(v_3_15_0a8_33aa2bf477d::_Py_DebugOffsets);
impl_basic_offsets!(v_3_15_0b1_6a660056998::_Py_DebugOffsets);
impl_basic_offsets!(v_3_16_0a0::_Py_DebugOffsets);

impl fmt::Display for VersionedOffsets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V3_13_1(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_13_13(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_14_4(o) => fmt_debug_offsets_basic(o, f),
            Self::V3_15_0a8(o) => fmt::Display::fmt(o, f),
            Self::V3_15_0b1(o) => fmt::Display::fmt(o, f),
            Self::V3_16_0a0(o) => fmt::Display::fmt(o, f),
        }
    }
}

// ── GC item layout resolver ────────────────────────────────────

/// Resolve the GC generation stats item layout by matching `item_size`
/// against all known generated structs. Returns `None` for unknown sizes.
pub fn resolve_gc_item_layout(item_size: usize) -> Option<&'static GcItemLayout> {
    macro_rules! try_layout {
        ($mod:ident) => {
            if item_size == $mod::GC_ITEM_SIZE {
                return Some(&$mod::GC_LAYOUT);
            }
        };
    }
    try_layout!(v_3_15_0a8_33aa2bf477d);
    try_layout!(v_3_15_0a7);
    try_layout!(v_3_16_0a0);
    None
}
