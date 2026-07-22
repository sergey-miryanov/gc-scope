//! Per-PID attached session.
//!
//! A `PySession` resolves an attached process's immutable facts ONCE — the
//! process handle, the `_PyRuntime` address, the Python version, and the offset
//! layout — and hands out cheap reads through the single held handle. It is the
//! one "resolve offsets for this PID" facade: every consumer attaches, then
//! matches on [`Resolved`] and degrades uniformly.
//!
//! See `docs/adr/0001-pysession-resolve-once-facade.md`. `attach` + reads, the
//! `(exe_path, mtime)`-keyed layout cache and `revalidate`, plus
//! `gc_stats`/`collect`, are consumed by gc-stats, monitor, the TUI,
//! and list-pids.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(feature = "test-hooks")]
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use read_process_memory::ProcessHandle;

use crate::memory::{process, reader};
use crate::remote_debugging::gc_stats::GcStat;
use crate::remote_debugging::offsets::{
    self, VersionedOffsets,
    offset_table::{GcStatsKind, GcStatsRegion, OffsetTable},
};
use crate::remote_debugging::version::{self, PythonVersion};

/// Identity of the Python binary a layout was resolved from: its on-disk path
/// plus mtime. A rebuilt binary (in-place upgrade) gets a new mtime and so a new
/// cache entry. See `docs/adr/0001-pysession-resolve-once-facade.md`.
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

/// What the command-line comparison alone can conclude, before the
/// `soft_reattach` backstop runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CmdlineCheck {
    /// The new read came back `None` — the process is gone.
    Gone,
    /// Both command lines are known and differ — definitely a different program.
    Differs,
    /// Inconclusive (identical, or either side unreadable) — defer to the backstop.
    Inconclusive,
}

/// Decide the revalidation from the command line alone.
///
/// `read_cmdline` is best-effort: it returns `Some("")` when the OS still has the
/// process but its command line can't be read right now — a transient access
/// failure, or a process caught mid-teardown (and `revalidate` runs precisely
/// after a failed read). An empty string is therefore *unknown*, NOT evidence of a
/// different program: concluding `Changed` from it would spuriously drop a still
/// valid session and its dedup marks. A definite `Changed` needs *both* sides
/// non-empty and differing; every other case defers to the exe-key/version
/// backstop in `soft_reattach`.
fn classify_cmdline(old: Option<&str>, new: Option<&str>) -> CmdlineCheck {
    match (old, new) {
        (_, None) => CmdlineCheck::Gone,
        (Some(old), Some(new)) if !old.is_empty() && !new.is_empty() && old != new => {
            CmdlineCheck::Differs
        }
        _ => CmdlineCheck::Inconclusive,
    }
}

/// Resolve a [`GcStatsRegion`] to a concrete stats-region address, performing the single
/// ring-pointer read a `Deref` needs through `read_ptr`. Split out of
/// [`PySession::gc_stats_region_addr`] so its fail-open branching is unit-testable without a
/// live target: `Absent` → `None` (no read at all — a read would fail *open* on a bogus
/// address); `Direct(a)` → `Some(a)` (already an address, no read); `Deref(p)` → read `p` and
/// treat NULL as the normal "stats not yet allocated / teardown" state (`None`, not an error).
/// A genuine read failure propagates, since a non-`Absent` region asserted stats should be there.
fn resolve_stats_region<F>(region: GcStatsRegion, read_ptr: F) -> Result<Option<u64>>
where
    F: FnOnce(u64) -> Result<u64>,
{
    match region {
        GcStatsRegion::Absent => Ok(None),
        GcStatsRegion::Direct(addr) => Ok(Some(addr)),
        GcStatsRegion::Deref(ptr_addr) => {
            let ptr = read_ptr(ptr_addr).context("Failed to read gc.generation_stats pointer")?;
            Ok((ptr != 0).then_some(ptr))
        }
    }
}

/// Whether a stats kind yields any decodable entries. `None` is the one non-reading kind:
/// [`PySession::supports_gc_stats`] (the capability the TUI picker and `list-pids` "S" column
/// report) reports it as unsupported, and [`PySession::gc_stats`] fast-returns an empty vec for
/// it. Both key off this single predicate.
fn kind_reads_stats(kind: GcStatsKind) -> bool {
    kind != GcStatsKind::None
}

/// Where an [`PySession`]'s layout came from on `attach`. Exposed so a caller — or a
/// lifecycle test — can tell whether the binary was re-parsed or the process-wide cache
/// was reused (ADR 0001's E1/E2 fast path). Purely informational: both tiers behave
/// identically for reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutSource {
    /// Resolved fresh this attach (the full cascade + parse), then cached.
    Parsed,
    /// Reused from the process-wide layout cache — no re-parse.
    Cached,
}

/// The resolved offset layout for an attached process.
///
/// The three variants are the support tiers. `Full` and `LayoutOnly` both carry the
/// bindgen `VersionedOffsets` (the fallback path still reads a real struct) plus the
/// flat `OffsetTable` derived from it; they differ only in confidence (exact hex vs.
/// same-minor fallback). `Legacy` (3.8–3.12) has no self-describing struct, only a
/// hardcoded table: it supports interpreter navigation and — for 3.9–3.12 — GC
/// generation stats (see [`PySession::supports_gc_stats`]), but not the
/// `_Py_DebugOffsets` struct panels of the TUI.
#[derive(Debug)]
pub enum Resolved {
    Full {
        offsets: VersionedOffsets,
        table: OffsetTable,
    },
    LayoutOnly {
        offsets: VersionedOffsets,
        table: OffsetTable,
    },
    Legacy {
        table: OffsetTable,
    },
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
    /// Whether this attach re-parsed the binary or reused the cache (§6).
    layout_source: LayoutSource,
    /// Test-only fault seam: number of upcoming `gc_stats` calls to fail before
    /// reading for real. Present only under the `test-hooks` feature, so production
    /// builds carry neither the field nor its read-path check. Lets a test drive the
    /// monitor's read-fail → `revalidate` → retry orchestration against a genuinely
    /// live process. See [`PySession::inject_gc_stats_faults`].
    #[cfg(feature = "test-hooks")]
    gc_fault_countdown: AtomicU32,
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

        let (cached, layout_source) = match cached {
            Some(entry) => (entry, LayoutSource::Cached),
            None => {
                let entry = resolve_layout(pid, runtime_addr, version)?;
                if let Some(key) = &exe_key {
                    LAYOUT_CACHE
                        .lock()
                        .unwrap()
                        .insert(key.clone(), entry.clone());
                }
                (entry, LayoutSource::Parsed)
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
            layout_source,
            #[cfg(feature = "test-hooks")]
            gc_fault_countdown: AtomicU32::new(0),
        })
    }

    /// Re-check a session whose read just failed (§6.3). Never retries on its own
    /// — that decision belongs to the caller's `WaitPolicy`. On `Fresh` the
    /// session has been soft re-attached (fresh handle + runtime address) and the
    /// caller should retry the read; on `Changed`/`Dead` the caller gives up.
    pub fn revalidate(&mut self) -> Revalidated {
        // Different command line ⇒ definitely a different program on this PID. An
        // unreadable (empty) command line is inconclusive — not a difference — so
        // it defers to the soft-reattach backstop instead of falsely reporting
        // Changed (see `classify_cmdline`).
        let current = process::read_cmdline(self.pid);
        match classify_cmdline(self.cmdline.as_deref(), current.as_deref()) {
            CmdlineCheck::Gone => return Revalidated::Dead,
            CmdlineCheck::Differs => return Revalidated::Changed,
            CmdlineCheck::Inconclusive => {}
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
    /// alongside a snapshot (e.g. the TUI's `CollectedData`).
    pub fn resolved_arc(&self) -> Arc<Resolved> {
        Arc::clone(&self.resolved)
    }

    /// The `_Py_DebugOffsets` version word for 3.13+ (`None` for pre-3.13).
    pub fn stored_hex(&self) -> Option<u64> {
        self.stored_hex
    }

    /// Whether this attach re-parsed the binary ([`LayoutSource::Parsed`]) or reused the
    /// process-wide layout cache ([`LayoutSource::Cached`]). The second attach to a still-
    /// live binary is a cache hit; see the lifecycle tests.
    pub fn layout_source(&self) -> LayoutSource {
        self.layout_source
    }

    /// Whether this build exposes decodable GC generation stats. True for 3.13+
    /// (`Full`/`LayoutOnly`) and all pre-3.13 3.8–3.12 (`Legacy` with the inline layout —
    /// 3.8 through its global GC, 3.9–3.12 per-interpreter); false only for a build with no
    /// stats layout (`GcStatsKind::None`). This is the capability the TUI picker and
    /// `list-pids` "S" column report.
    pub fn supports_gc_stats(&self) -> bool {
        kind_reads_stats(self.resolved.table().gc_stats_kind)
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

    /// Resolve one interpreter's GC generation-stats region address from its
    /// `_gc_runtime_state` address (`gc_addr`).
    ///
    /// The single reader-layer entry point for stats-region resolution: both this
    /// session's own [`gc_stats`](Self::gc_stats) (the monitor path) and the TUI
    /// collector call it, so the fail-open "which address, deref the ring, handle NULL"
    /// logic lives in one place and any fix reaches both. The geometry decision is the pure
    /// [`OffsetTable::gc_stats_region`]; the one ring-pointer read a `Deref` needs happens
    /// here through the held handle. A NULL ring pointer (stats not yet allocated /
    /// teardown) reads back as `Ok(None)`.
    pub fn gc_stats_region_addr(&self, gc_addr: u64) -> Result<Option<u64>> {
        let gen_stats_off = self
            .resolved
            .offsets()
            .map(|vo| vo.gc_generation_stats())
            .unwrap_or(0);
        let region = self
            .resolved
            .table()
            .gc_stats_region(gc_addr, gen_stats_off);
        resolve_stats_region(region, |ptr_addr| self.read_u64(ptr_addr))
    }

    /// Test hook: force the next `n` `gc_stats` calls on this session to fail with
    /// an error, then read normally. Arms the fault seam so a test can reproduce a
    /// transient read failure on a live process and exercise the monitor's
    /// `revalidate`/retry path deterministically. Compiled only under the
    /// `test-hooks` feature; not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_gc_stats_faults(&self, n: u32) {
        self.gc_fault_countdown.store(n, Ordering::Relaxed);
    }

    /// Consume one armed fault, if any. Returns `true` exactly when a fault was
    /// pending (and decrements it). A no-op single atomic load when unarmed.
    #[cfg(feature = "test-hooks")]
    fn take_injected_fault(&self) -> bool {
        self.gc_fault_countdown
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
    }

    /// Read GC generation stats for the first (or, with `all_interpreters`, every)
    /// interpreter. Dispatches on this build's stats-region shape; each kind has its own
    /// reader below. Reads go through the held handle. A NULL / absent stats region is a
    /// normal transient state and is skipped without hanging (C1); a failed buffer read
    /// propagates as `Err` (C6).
    pub fn gc_stats(&self, all_interpreters: bool) -> Result<Vec<GcStat>> {
        #[cfg(feature = "test-hooks")]
        if self.take_injected_fault() {
            bail!("gc_stats: injected fault (test hook)");
        }
        match self.resolved.table().gc_stats_kind {
            GcStatsKind::None => Ok(Vec::new()),
            GcStatsKind::InlineArray => self.gc_stats_inline(all_interpreters),
            GcStatsKind::RingBuffer => self.gc_stats_ring(all_interpreters),
        }
    }

    /// `InlineArray` builds (3.8–3.14): the stats sit at a fixed offset inside
    /// `_gc_runtime_state`. 3.8 keeps that state global in `_PyRuntime` (no per-interpreter
    /// `gc`); 3.9+ has it per interpreter.
    fn gc_stats_inline(&self, all_interpreters: bool) -> Result<Vec<GcStat>> {
        let head_addr = self.read_interpreters_head()?;
        if self.resolved.table().has_global_gc() {
            return self.gc_stats_global(head_addr);
        }
        self.gc_stats_per_interpreter(head_addr, all_interpreters)
    }

    /// `RingBuffer` builds (3.15+): the stats hang off the `gc.generation_stats` pointer,
    /// always per interpreter. The process-published ring size is verified first (fail
    /// closed on an unregistered build) so a wrong layout can't decode into garbage.
    fn gc_stats_ring(&self, all_interpreters: bool) -> Result<Vec<GcStat>> {
        self.verify_ring_stats_size()?;
        let head_addr = self.read_interpreters_head()?;
        self.gc_stats_per_interpreter(head_addr, all_interpreters)
    }

    /// Address of the interpreter-list head (`_PyRuntime.interpreters.head`).
    fn read_interpreters_head(&self) -> Result<u64> {
        let head_off = self.resolved.table().runtime_interpreters_head();
        self.read_u64(self.runtime_addr + head_off)
    }

    /// Fail-closed size guard for a ring-buffer build (called only from [`gc_stats_ring`]).
    /// The process publishes the true byte size of its ring region in
    /// `gc.generation_stats_size`; `attach` has already selected the best-matching layout,
    /// so for any recognized build the reconstructed size equals what the process reports
    /// and this stays silent. A mismatch means the per-entry stride or the field offsets are
    /// wrong — every number we could decode would be garbage — so bail with a regenerate
    /// hint (C12) rather than decode nonsense. (This is how a 3.15.0b4 target silently
    /// decoded through the 3.15.0a8 layout, 96-byte entries vs 64, while every CI leg stayed
    /// green.)
    fn verify_ring_stats_size(&self) -> Result<()> {
        let table = self.resolved.table();
        let Some(vo) = self.resolved.offsets() else {
            return Ok(());
        };
        let reported = vo.gc_generation_stats_size();
        let expected = table.gc_stats_region_size();
        if reported != 0 && expected != 0 && reported != expected {
            bail!(
                "gc_generation_stats size mismatch for {:#010x}: the process reports \
                 {reported} bytes but gcscope's compiled layout expects {expected}. \
                 This build's GC ring layout differs from the registered one; decoding \
                 it would report garbage. Run `gcscope read-runtime <pid>` to see the \
                 selected layout and its geometry, then regenerate offsets with \
                 scripts/gen-offsets.py against this exact build.",
                table.version_hex
            );
        }
        Ok(())
    }

    /// 3.8 global-GC path: the GC state lives in `_PyRuntime` itself, not per interpreter.
    /// Resolve the stats region once from the runtime and read it a single time — reading it
    /// inside the interpreter walk would emit the same global generations once per
    /// interpreter under `--all`. The caller has already confirmed the global-GC shape.
    fn gc_stats_global(&self, head_addr: u64) -> Result<Vec<GcStat>> {
        let table = self.resolved.table();
        let iid = if head_addr != 0 {
            self.read_i64(head_addr + table.interp_id())?
        } else {
            0
        };
        let gc_addr = table.gc_state_addr(self.runtime_addr, head_addr);
        let mut global_table = table.clone();
        global_table.gc_stats_addr = self.gc_stats_region_addr(gc_addr)?;
        global_table.read_gc_stats(&self.handle, iid)
    }

    /// Per-interpreter walk, shared by 3.9+ inline and all ring builds: follow the
    /// interpreter chain from `head_addr`, resolving and reading each interpreter's stats
    /// region (the shape difference is absorbed by [`gc_stats_region_addr`](Self::gc_stats_region_addr)).
    /// Stops after the first interpreter unless `all_interpreters`. Always advances the walk
    /// even for an interpreter with no readable stats, so a NULL region never hangs it (C1).
    fn gc_stats_per_interpreter(
        &self,
        head_addr: u64,
        all_interpreters: bool,
    ) -> Result<Vec<GcStat>> {
        let table = self.resolved.table();
        let next_off = table.interp_next();
        let id_off = table.interp_id();

        let mut stats = Vec::new();
        let mut current = head_addr;
        let mut first = true;
        while current != 0 {
            let iid = self.read_i64(current + id_off)?;
            let gc_addr = table.gc_state_addr(self.runtime_addr, current);
            if let Some(addr) = self.gc_stats_region_addr(gc_addr)? {
                let mut interp_table = table.clone();
                interp_table.gc_stats_addr = Some(addr);
                stats.extend(interp_table.read_gc_stats(&self.handle, iid)?);
            }

            // Always advance — the walk must make progress even for an interpreter with no
            // readable stats (this is what previously hung on NULL pointers).
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
        let table = offsets::pre_3_13::table_for_version(version.major, version.minor).ok_or_else(
            || {
                anyhow!(
                    "Unsupported Python version {}.{} (no pre-3.13 offset table)",
                    version.major,
                    version.minor
                )
            },
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Two different, readable command lines are the one case that lets the fast
    /// path short-circuit to a definite Changed — a genuinely reused PID.
    #[test]
    fn distinct_nonempty_cmdlines_are_a_definite_change() {
        assert_eq!(
            classify_cmdline(Some("python spin.py"), Some("python other.py")),
            CmdlineCheck::Differs
        );
    }

    /// A `None` new read means the process is gone, whatever the old cmdline was.
    #[test]
    fn a_none_new_read_is_gone() {
        assert_eq!(
            classify_cmdline(Some("python spin.py"), None),
            CmdlineCheck::Gone
        );
        assert_eq!(classify_cmdline(None, None), CmdlineCheck::Gone);
    }

    /// Identical command lines settle nothing on their own — a same-argv relaunch
    /// looks identical too, so the backstop must decide.
    #[test]
    fn identical_cmdlines_are_inconclusive() {
        assert_eq!(
            classify_cmdline(Some("python spin.py"), Some("python spin.py")),
            CmdlineCheck::Inconclusive
        );
    }

    /// The regression guard for the Windows `read_cmdline` fix: a still-live
    /// process whose command line momentarily reads back empty must NOT be judged
    /// Changed — that empty is "unknown", not a difference. Before the guard, a
    /// populated baseline vs. an empty re-read would have short-circuited to
    /// Changed and dropped a valid session.
    #[test]
    fn an_empty_read_is_inconclusive_not_a_change() {
        // New read unreadable while the baseline is known.
        assert_eq!(
            classify_cmdline(Some("python spin.py"), Some("")),
            CmdlineCheck::Inconclusive
        );
        // Baseline was unreadable at attach; a later real cmdline still can't be
        // called a *change* with nothing to compare against.
        assert_eq!(
            classify_cmdline(Some(""), Some("python spin.py")),
            CmdlineCheck::Inconclusive
        );
        // Both unreadable.
        assert_eq!(
            classify_cmdline(Some(""), Some("")),
            CmdlineCheck::Inconclusive
        );
    }

    /// No baseline captured at attach ⇒ nothing to compare, so a new read alone
    /// can't prove a change; defer. (Only a `None` *new* read means gone.)
    #[test]
    fn a_missing_baseline_is_inconclusive() {
        assert_eq!(
            classify_cmdline(None, Some("python spin.py")),
            CmdlineCheck::Inconclusive
        );
    }

    // ── stats-region resolution (pure; the one ring read is injected) ──

    /// `Absent` is the "this build exposes no stats" outcome (a `None`-kind build, or a ring
    /// whose pointer field is unresolved). It must resolve to `None` WITHOUT any pointer read
    /// — a read here would fail *open* on a bogus address and hand back garbage. The reader
    /// closure panics to prove it is never called.
    #[test]
    fn absent_region_resolves_to_none_without_reading() {
        let got = resolve_stats_region(GcStatsRegion::Absent, |_| {
            panic!("Absent must not trigger a pointer read")
        })
        .unwrap();
        assert_eq!(got, None);
    }

    /// `Direct` (inline builds) is already the stats address — hand it back verbatim, again
    /// with no read.
    #[test]
    fn direct_region_returns_the_address_verbatim() {
        let got = resolve_stats_region(GcStatsRegion::Direct(0xdead_beef), |_| {
            panic!("Direct must not trigger a pointer read")
        })
        .unwrap();
        assert_eq!(got, Some(0xdead_beef));
    }

    /// `Deref` (ring builds) reads the pointer at the *pointer-field* address it is handed; a
    /// non-NULL value is the stats region, a NULL value is the normal "not allocated yet /
    /// teardown" state and resolves to `None` — skipped, never an error.
    #[test]
    fn deref_region_reads_the_pointer_and_maps_null_to_none() {
        // Non-NULL → Some(pointer). The closure also confirms it is handed the pointer-FIELD
        // address (0x1000), not the region address it returns.
        let got = resolve_stats_region(GcStatsRegion::Deref(0x1000), |addr| {
            assert_eq!(addr, 0x1000);
            Ok(0x4000)
        })
        .unwrap();
        assert_eq!(got, Some(0x4000));

        // NULL → None, not an error.
        let got = resolve_stats_region(GcStatsRegion::Deref(0x1000), |_| Ok(0)).unwrap();
        assert_eq!(got, None);
    }

    /// A failed pointer read on a `Deref` is a real error (a non-`Absent` region asserted the
    /// stats should be there) and propagates *with context* — it is NOT swallowed to `None`.
    #[test]
    fn deref_region_propagates_a_read_error() {
        let err = resolve_stats_region(GcStatsRegion::Deref(0x1000), |_| {
            Err(anyhow!("simulated read failure"))
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("gc.generation_stats pointer"),
            "the read error must carry its context: {err}"
        );
    }

    /// `None` is the single stats kind that reads nothing — it is what `supports_gc_stats`
    /// reports as unsupported and what `gc_stats` fast-returns an empty vec for. Every other
    /// kind reads.
    #[test]
    fn only_the_none_kind_reads_no_stats() {
        assert!(!kind_reads_stats(GcStatsKind::None));
        assert!(kind_reads_stats(GcStatsKind::InlineArray));
        assert!(kind_reads_stats(GcStatsKind::RingBuffer));
    }
}
