use anyhow::{bail, Result};
use proc_maps::{get_process_maps, MapRange};

use crate::memory::{process, reader};
use crate::remote_debugging::offsets;
use crate::remote_debugging::offsets::offset_table::OffsetTable;
use crate::remote_debugging::version;

pub struct CheckResult {
    pub expected: u64,
    pub found: u64,
    pub match_ok: bool,
}

fn maps_contains(maps: &[MapRange], addr: usize) -> bool {
    maps.iter()
        .any(|m| addr >= m.start() && addr < m.start() + m.size())
}

/// Scan a slice of potential pointer values from the target process
/// for a valid `PyInterpreterState` address.
///
/// Each word is treated as a candidate `PyInterpreterState*`.  Validation
/// follows the `tstate_head → interp` back-pointer chain:
///
///   1. candidate is in mapped memory
///   2. `*(candidate + threads_head)` → a valid `PyThreadState*`
///   3. `*(threadstate  + interp)`   → equals candidate
pub fn check_interpreter_addresses(
    pid: u32,
    words: &[u64],
    threads_head_offset: u64,
    thread_interp_offset: u64,
) -> Result<u64> {
    let maps = get_process_maps(pid as proc_maps::Pid)
        .map_err(|e| anyhow::anyhow!("Failed to get process memory maps: {}", e))?;

    for &candidate in words {
        if !maps_contains(&maps, candidate as usize) {
            continue;
        }

        let tstate_ptr = match reader::read_u64(pid, candidate + threads_head_offset) {
            Ok(ptr) => ptr,
            Err(_) => continue,
        };

        if tstate_ptr == 0 || !maps_contains(&maps, tstate_ptr as usize) {
            continue;
        }

        let interp_ptr = match reader::read_u64(pid, tstate_ptr + thread_interp_offset) {
            Ok(ptr) => ptr,
            Err(_) => continue,
        };

        if interp_ptr == candidate {
            return Ok(candidate);
        }
    }

    bail!("Failed to find a valid PyInterpreterState address");
}

/// High-level check: given a known-good `_PyRuntime` address and the offsets
/// obtained from `_Py_DebugOffsets`, verify that [`check_interpreter_addresses`]
/// finds the same `interpreters_head` pointer.
pub fn check_runtime(
    pid: u32,
    runtime_addr: u64,
    runtime_state_size: u64,
    runtime_interpreters_head: u64,
    threads_head_offset: u64,
    thread_interp_offset: u64,
) -> Result<CheckResult> {
    let expected =
        reader::read_u64(pid, runtime_addr + runtime_interpreters_head)?;

    let rt_size = runtime_state_size as usize;
    let bytes = reader::read_memory(pid, runtime_addr, rt_size)?;

    let words: &[u64] = unsafe {
        std::slice::from_raw_parts(
            bytes.as_ptr() as *const u64,
            bytes.len() / std::mem::size_of::<u64>(),
        )
    };

    let found = check_interpreter_addresses(
        pid,
        words,
        threads_head_offset,
        thread_interp_offset,
    )?;

    Ok(CheckResult {
        expected,
        found,
        match_ok: found == expected,
    })
}

/// Full-chain verification for a given PID: find runtime, detect version,
/// read offsets, and run [`check_interpreter_addresses`].
///
/// Works for any supported Python version (3.8+).
/// Returns `None` if any step fails (process exited, unsupported version, etc.).
pub fn verify_process(pid: u32) -> Option<bool> {
    let runtime_addr = process::find_runtime(pid).ok()?;
    let ver = version::detect(pid).ok()?;
    if ver.major != 3 {
        return None;
    }

    // 3.13+: offsets come from the process's self-describing `_Py_DebugOffsets`, read
    // through the versioned bindgen layout. `read_offsets` handles both exact and
    // same-minor-fallback dispatch, so it covers any 3.13+ build; an `Err` means the
    // version is genuinely unsupported (can't verify).
    if ver.minor >= 13 {
        let (_addr, _stored, offs) = offsets::read_offsets(pid, &ver).ok()?;
        let result = check_runtime(
            pid,
            runtime_addr,
            offs.runtime_state_size(),
            offs.runtime_interpreters_head(),
            offs.interpreter_state_threads_head(),
            offs.thread_state_interp(),
        )
        .ok()?;
        return Some(result.match_ok);
    }

    // Pre-3.13 path: hardcoded, minor-level offset tables. There is no self-describing
    // `_Py_DebugOffsets` to validate against here, so we use the minor's table for any
    // micro (pre-3.13 offsets are stable within a minor) and warn once. GC generation
    // stats are not available on these versions.
    let table = offsets::pre_3_13::table_for_version(ver.major, ver.minor)?;
    warn_pre_3_13_once(ver.major, ver.minor, ver.micro);
    verify_with_table(pid, runtime_addr, &table)
}

/// Warn once per (major, minor) that pre-3.13 support uses minor-level hardcoded
/// offsets (micro not distinguished) and provides no GC generation stats.
fn warn_pre_3_13_once(major: u8, minor: u8, micro: u8) {
    static WARNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<(u8, u8)>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    if WARNED.lock().unwrap().insert((major, minor)) {
        eprintln!(
            "warning: Python {major}.{minor}.{micro} predates _Py_DebugOffsets; using \
             hardcoded {major}.{minor}.x offsets (navigation only, no GC generation stats).",
        );
    }
}

fn verify_with_table(pid: u32, runtime_addr: u64, table: &OffsetTable) -> Option<bool> {
    let scan_size = table.runtime_interpreters_head + 64;
    let result = check_runtime(
        pid,
        runtime_addr,
        scan_size,
        table.runtime_interpreters_head,
        table.interp_threads_head,
        table.thread_interp,
    )
    .ok()?;
    Some(result.match_ok)
}
