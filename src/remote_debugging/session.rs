//! Per-PID attached session.
//!
//! A `PySession` resolves an attached process's immutable facts ONCE — the
//! process handle, the `_PyRuntime` address, the Python version, and the offset
//! layout — and hands out cheap reads through the single held handle. It is the
//! one "resolve offsets for this PID" facade: every consumer attaches, then
//! matches on [`Resolved`]/[`Tier`] and degrades uniformly.
//!
//! See `docs/pysession-plan.md`. `attach` + reads, the `(exe_path, mtime)`-keyed
//! layout cache and `revalidate` (§6), plus `gc_stats`/`collect`/`verify`, are
//! consumed by gc-stats, monitor, the diagram stack, and list-pids.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use read_process_memory::ProcessHandle;

use crate::memory::{process, reader};
use crate::remote_debugging::gc_stats::GcStat;
use crate::remote_debugging::offsets::{
    self,
    offset_table::{GcStatsKind, OffsetTable},
    VersionedOffsets,
};
use crate::remote_debugging::version::{self, PythonVersion};

/// Identity of the Python binary a layout was resolved from: its on-disk path
/// plus mtime. A rebuilt binary (in-place upgrade) gets a new mtime and so a new
/// cache entry. See `docs/pysession-plan.md` §6.
type ExeKey = (PathBuf, SystemTime);

/// A resolved layout plus the metadata needed to reuse it safely across PIDs and
/// process restarts. Cheap to clone (an `Arc` plus `Copy` fields).
#[derive(Clone)]
struct CachedLayout {
    resolved: Arc<Resolved>,
    version: PythonVersion,
    /// The `_Py_DebugOffsets` version word for 3.13+ (`None` for pre-3.13, which
    /// has no self-describing word). Used as the version backstop on a cache hit.
    stored_hex: Option<u64>,
}

/// Process-wide layout cache, keyed by binary identity. The resolved offsets are
/// a pure function of the binary (`to_offset_table` reads only the already-read
/// struct, never live memory), so one entry is shared across every PID running
/// that binary and survives a PID's death — making a reused-PID or
/// same-interpreter re-attach cheap (E1/E2). Keying on `(path, mtime)` also keeps
/// the clean and `+inc` builds that share a version hex in separate entries.
static LAYOUT_CACHE: LazyLock<Mutex<HashMap<ExeKey, CachedLayout>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Outcome of re-checking a session whose read failed (§6.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Revalidated {
    /// Same program, soft re-attached (fresh handle + runtime address) — retry.
    Fresh,
    /// A different program now holds this PID — the session is stale, drop it.
    Changed,
    /// The process is gone / unreadable.
    Dead,
}

/// The resolved offset layout for an attached process.
///
/// The three variants are the support tiers. `Full` and `LayoutOnly` both carry the
/// bindgen `VersionedOffsets` (the fallback path still reads a real struct) plus the
/// flat `OffsetTable` derived from it; they differ only in confidence (exact hex vs.
/// same-minor fallback). `Legacy` (3.8–3.12) has no self-describing struct, only a
/// hardcoded table: it supports interpreter navigation and — for 3.9–3.12 — GC
/// generation stats (see [`PySession::supports_gc_stats`]), but not the
/// `_Py_DebugOffsets` struct panels of the diagram.
#[derive(Debug)]
pub enum Resolved {
    Full { offsets: VersionedOffsets, table: OffsetTable },
    LayoutOnly { offsets: VersionedOffsets, table: OffsetTable },
    Legacy { table: OffsetTable },
}

impl Resolved {
    /// The flat offset table, available for every tier.
    pub fn table(&self) -> &OffsetTable {
        match self {
            Resolved::Full { table, .. }
            | Resolved::LayoutOnly { table, .. }
            | Resolved::Legacy { table } => table,
        }
    }

    /// The bindgen offsets, if this is a 3.13+ tier (`None` for `Legacy`).
    pub fn offsets(&self) -> Option<&VersionedOffsets> {
        match self {
            Resolved::Full { offsets, .. } | Resolved::LayoutOnly { offsets, .. } => Some(offsets),
            Resolved::Legacy { .. } => None,
        }
    }
}

/// An attached CPython process: resolve once, read many.
///
/// No `Debug` derive: `ProcessHandle` is not `Debug`.
pub struct PySession {
    pid: u32,
    handle: ProcessHandle,
    /// Load address of `_PyRuntime` (== the `_Py_DebugOffsets` base for 3.13+).
    runtime_addr: u64,
    /// Authoritative version: the live `_Py_DebugOffsets` word for 3.13+, else
    /// the detected on-disk version.
    version: PythonVersion,
    /// Shared layout, from the process-wide cache (§6).
    resolved: Arc<Resolved>,
    /// Version word for the 3.13+ backstop (`None` for pre-3.13).
    stored_hex: Option<u64>,
    /// Binary identity, when its mtime is readable — the layout cache key and the
    /// `revalidate` exe-change check. `None` disables caching for this session.
    exe_key: Option<ExeKey>,
    /// Command line at attach time — the `revalidate` change-detector (§6.3).
    cmdline: Option<String>,
}

impl PySession {
    /// Resolve `pid` once: open the handle, find `_PyRuntime` (+ its module path),
    /// then obtain the offset layout — reusing the process-wide cache when the
    /// binary's `(path, mtime)` is already known and the live version word still
    /// matches, else running the full cascade and caching the result (§6).
    pub fn attach(pid: u32) -> Result<Self> {
        let handle = reader::open_handle(pid)?;
        let (runtime_addr, module_path, version) = find_runtime_versioned(pid)?;
        let exe_key = exe_key_for(&module_path);
        let cmdline = process::read_cmdline(pid);

        // Layout-cache fast path: reuse a cached layout for this exact binary when
        // the live version word still matches (the argv/mtime proxy backstop).
        let cached = match &exe_key {
            Some(key) => {
                let hit = LAYOUT_CACHE.lock().unwrap().get(key).cloned();
                match hit {
                    Some(entry) if layout_still_valid(&handle, runtime_addr, &entry)? => {
                        Some(entry)
                    }
                    _ => None,
                }
            }
            None => None,
        };

        let cached = match cached {
            Some(entry) => entry,
            None => {
                let entry = resolve_layout(pid, runtime_addr, version)?;
                if let Some(key) = &exe_key {
                    LAYOUT_CACHE.lock().unwrap().insert(key.clone(), entry.clone());
                }
                entry
            }
        };

        Ok(PySession {
            pid,
            handle,
            runtime_addr,
            version: cached.version,
            resolved: cached.resolved,
            stored_hex: cached.stored_hex,
            exe_key,
            cmdline,
        })
    }

    /// Re-check a session whose read just failed (§6.3). Never retries on its own
    /// — that decision belongs to the caller's `WaitPolicy`. On `Fresh` the
    /// session has been soft re-attached (fresh handle + runtime address) and the
    /// caller should retry the read; on `Changed`/`Dead` the caller gives up.
    pub fn revalidate(&mut self) -> Revalidated {
        // Different command line ⇒ definitely a different program on this PID.
        match (self.cmdline.as_deref(), process::read_cmdline(self.pid)) {
            (_, None) => return Revalidated::Dead,
            (Some(old), Some(new)) if old != new => return Revalidated::Changed,
            _ => {}
        }
        // Same argv ⇒ same-program relaunch (reused PID) or a transient glitch.
        // Soft re-attach, backstopped by the exe (path, mtime) and version word so
        // a same-argv in-place upgrade is still caught.
        match self.soft_reattach() {
            Ok(true) => Revalidated::Fresh,
            Ok(false) => Revalidated::Changed,
            Err(_) => Revalidated::Dead,
        }
    }

    /// Refresh instance-specific state (handle + runtime address) in place,
    /// reusing the cached [`Resolved`]. Returns `Ok(false)` if the binary or its
    /// version word drifted (a same-argv upgrade) — the caller treats that as a
    /// changed program.
    fn soft_reattach(&mut self) -> Result<bool> {
        let handle = reader::open_handle(self.pid)?;
        let (runtime_addr, module_path, _version) = find_runtime_versioned(self.pid)?;

        if exe_key_for(&module_path) != self.exe_key {
            return Ok(false);
        }
        if let Some(hex) = self.stored_hex
            && reader::read_u64_h(&handle, runtime_addr + 8)? != hex
        {
            return Ok(false);
        }

        self.handle = handle;
        self.runtime_addr = runtime_addr;
        Ok(true)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn runtime_addr(&self) -> u64 {
        self.runtime_addr
    }

    pub fn version(&self) -> &PythonVersion {
        &self.version
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    /// A cheap clone of the shared layout, for consumers that want to hold it
    /// alongside a snapshot (e.g. the diagram's `CollectedData`).
    pub fn resolved_arc(&self) -> Arc<Resolved> {
        Arc::clone(&self.resolved)
    }

    /// The `_Py_DebugOffsets` version word for 3.13+ (`None` for pre-3.13).
    pub fn stored_hex(&self) -> Option<u64> {
        self.stored_hex
    }

    /// Whether this build exposes decodable GC generation stats. True for 3.13+
    /// (`Full`/`LayoutOnly`) and pre-3.13 3.9–3.12 (`Legacy` with the inline layout);
    /// false for 3.8 (global GC, not yet decoded) or any build without a stats layout.
    /// This is the capability the TUI picker and `list-pids` "S" column report.
    pub fn supports_gc_stats(&self) -> bool {
        self.resolved.table().gc_stats_kind != GcStatsKind::None
    }

    /// Read `size` bytes at `addr` through the held handle (no per-call open).
    pub fn read(&self, addr: u64, size: usize) -> Result<Vec<u8>> {
        reader::read_memory_h(&self.handle, addr, size)
    }

    /// Read a little-endian `u64` at `addr` through the held handle.
    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        reader::read_u64_h(&self.handle, addr)
    }

    /// Read a little-endian `i64` at `addr` through the held handle.
    pub fn read_i64(&self, addr: u64) -> Result<i64> {
        Ok(self.read_u64(addr)? as i64)
    }

    /// Read GC generation stats for the first (or, with `all_interpreters`, every)
    /// interpreter. Walks the interpreter chain and resolves each interpreter's
    /// stats region by its shape (`InlineArray` at a fixed offset, `RingBuffer`
    /// via the `gc.generation_stats` pointer). Reads go through the held handle.
    ///
    /// A NULL stats pointer is a normal transient state (stats not yet allocated,
    /// or teardown): that interpreter is skipped and the walk still advances — it
    /// never hangs (C1). A failed buffer read propagates as `Err` (C6).
    pub fn gc_stats(&self, all_interpreters: bool) -> Result<Vec<GcStat>> {
        let table = self.resolved.table();

        // Catch-all guard for an unregistered build: the process's own
        // `gc.generation_stats_size` records the TOTAL byte size of the ring-buffer
        // region. `attach` already SELECTED the best-matching layout, so for any
        // recognized build the reconstructed total equals `reported` and this stays
        // silent. It fires only when selection fell through with no matching
        // candidate — emitting a regeneration hint (C12). Ring-buffer versions only.
        if table.gc_stats_kind == GcStatsKind::RingBuffer
            && let Some(vo) = self.resolved.offsets()
        {
            let reported = vo.gc_generation_stats_size();
            if let (Some(item), Some(bases), Some(slots)) =
                (table.gc_item_size, table.gc_gen_base_offsets, table.gc_slots_per_gen)
            {
                let expected = bases[2] + slots[2] * item + 8;
                if reported != 0 && reported != expected {
                    eprintln!(
                        "warning: gc_generation_stats size mismatch for {:#010x}: the process \
                         reports {reported} bytes but gcscope's compiled layout expects {expected}. \
                         This build's GC ring layout may differ from the registered one — \
                         regenerate offsets with scripts/gen-offsets.py against this exact build.",
                        table.version_hex
                    );
                }
            }
        }

        let head_addr = self.read_u64(self.runtime_addr + table.runtime_interpreters_head())?;
        let next_off = table.interp_next();
        let id_off = table.interp_id();

        // Global-GC path (3.8): the GC state lives in `_PyRuntime` itself, not per
        // interpreter. Resolve the stats region once from the runtime and read it a
        // single time — reading it inside the interpreter walk would emit the same
        // global generations once per interpreter under `--all`.
        if table.interp_gc.is_none()
            && table.gc_stats_kind == GcStatsKind::InlineArray
            && let Some(runtime_gc) = table.runtime_gc
        {
            let iid = if head_addr != 0 { self.read_i64(head_addr + id_off)? } else { 0 };
            let mut global_table = table.clone();
            global_table.gc_stats_addr =
                Some(self.runtime_addr + runtime_gc + table.gc_stats_inline_off);
            return global_table.read_gc_stats(&self.handle, iid);
        }

        let gc_off = table.interp_gc.unwrap_or(0);

        let mut stats = Vec::new();
        let mut current = head_addr;
        let mut first = true;
        while current != 0 {
            let iid = self.read_i64(current + id_off)?;
            let gc_addr = current + gc_off; // this interpreter's `_gc_runtime_state`

            // Resolve this interpreter's stats address by its region shape.
            let stats_addr = match table.gc_stats_kind {
                GcStatsKind::None => None,
                GcStatsKind::InlineArray => Some(gc_addr + table.gc_stats_inline_off),
                GcStatsKind::RingBuffer => {
                    let gen_stats_off = self
                        .resolved
                        .offsets()
                        .map(|vo| vo.gc_generation_stats())
                        .unwrap_or(0);
                    if gen_stats_off == 0 {
                        None
                    } else {
                        let ptr = self.read_u64(gc_addr + gen_stats_off)?;
                        (ptr != 0).then_some(ptr)
                    }
                }
            };

            if let Some(addr) = stats_addr {
                let mut interp_table = table.clone();
                interp_table.gc_stats_addr = Some(addr);
                stats.extend(interp_table.read_gc_stats(&self.handle, iid)?);
            }

            // Always advance — the walk must make progress even for an interpreter
            // with no readable stats (this is what previously hung on NULL pointers).
            current = self.read_u64(current + next_off)?;
            if first && !all_interpreters {
                break;
            }
            first = false;
        }

        Ok(stats)
    }
}

/// Build the `(path, mtime)` cache key for a module path, or `None` if the mtime
/// can't be read (⇒ this session neither uses nor populates the layout cache).
fn exe_key_for(module_path: &str) -> Option<ExeKey> {
    let meta = std::fs::metadata(module_path).ok()?;
    let mtime = meta.modified().ok()?;
    Some((PathBuf::from(module_path), mtime))
}

/// Version backstop for a layout cache hit: for 3.13+ the cached layout is valid
/// only if the live `_Py_DebugOffsets` version word still equals the one it was
/// resolved from (guards a same-argv/same-mtime-but-different build). Pre-3.13
/// has no such word, so `(path, mtime)` identity alone is the guarantee.
fn layout_still_valid(
    handle: &ProcessHandle,
    runtime_addr: u64,
    entry: &CachedLayout,
) -> Result<bool> {
    match entry.stored_hex {
        Some(hex) => Ok(reader::read_u64_h(handle, runtime_addr + 8)? == hex),
        None => Ok(true),
    }
}

/// Find `_PyRuntime` in `pid`, dispatching on the interpreter's Python version.
///
/// 3.13+ is anchored by the `"xdebugpy"` cookie ([`process::find_runtime_module`]);
/// pre-3.13, which predates that cookie, is anchored by resolving the `_PyRuntime`
/// symbol and confirming it with the interpreter/thread cross-reference heuristic
/// ([`process::find_runtime_pre_3_13`]). Returns the runtime address, the module path
/// (the layout-cache identity), and the detected version so callers avoid re-detecting.
fn find_runtime_versioned(pid: u32) -> Result<(u64, String, PythonVersion)> {
    let version = version::detect(pid)?;
    if version.major != 3 {
        return Err(anyhow!(
            "Unsupported Python major version {}.{}",
            version.major,
            version.minor
        ));
    }

    let (addr, path) = if version.minor >= 13 {
        process::find_runtime_module(pid)?
    } else {
        let table = offsets::pre_3_13::table_for_version(version.major, version.minor)
            .ok_or_else(|| {
                anyhow!(
                    "Unsupported Python version {}.{} (no pre-3.13 offset table)",
                    version.major,
                    version.minor
                )
            })?;
        process::find_runtime_pre_3_13(pid, &table)?
    };
    Ok((addr, path, version))
}

/// Run the full resolve cascade (bindgen exact → same-minor fallback → pre-3.13)
/// and package it for the cache. This is the only path that reads offsets out of
/// the process; a cache hit skips it entirely.
fn resolve_layout(pid: u32, runtime_addr: u64, detected: PythonVersion) -> Result<CachedLayout> {
    if detected.minor >= 13 {
        // 3.13+: the live `_Py_DebugOffsets` word is authoritative; `stored`
        // drives dispatch inside `read_offsets` (exact or same-minor fallback).
        let (_addr, stored, offsets) = offsets::read_offsets(pid, &detected)?;
        let table = offsets.to_offset_table(pid, runtime_addr);
        let version = PythonVersion::from_hex(stored).unwrap_or(detected);
        let resolved = if offsets::has_exact_layout(stored) {
            Resolved::Full { offsets, table }
        } else {
            Resolved::LayoutOnly { offsets, table }
        };
        Ok(CachedLayout {
            resolved: Arc::new(resolved),
            version,
            stored_hex: Some(stored),
        })
    } else {
        // Pre-3.13: no self-describing struct; use the minor-level table.
        let table = offsets::pre_3_13::table_for_version(detected.major, detected.minor)
            .ok_or_else(|| {
                anyhow!(
                    "Unsupported Python version {}.{} (no pre-3.13 offset table)",
                    detected.major,
                    detected.minor
                )
            })?;
        Ok(CachedLayout {
            resolved: Arc::new(Resolved::Legacy { table }),
            version: detected,
            stored_hex: None,
        })
    }
}
