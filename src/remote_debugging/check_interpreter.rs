use anyhow::{bail, Result};
use proc_maps::{get_process_maps, MapRange};

use crate::memory::reader;

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

// The full-chain PID verification that used to live here (`verify_process` +
// `verify_with_table` + `warn_pre_3_13_once`) moved to `PySession::verify`, so the
// three-way resolve cascade lives in exactly one place (`PySession::attach`).
// `check_runtime` above is still used by `PySession::verify`, the offsets fallback
// validator, and `main.rs`'s `find-runtime --check`.
