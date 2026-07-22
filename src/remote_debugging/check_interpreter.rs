use anyhow::{Result, bail};
use proc_maps::{MapRange, get_process_maps};

use crate::memory::reader;

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

/// High-level check: given a candidate `_PyRuntime` address and the offsets needed to
/// walk it, confirm that scanning the runtime for a self-consistent `PyInterpreterState`
/// recovers exactly the pointer stored in `interpreters_head`. Returns `true` when the
/// candidate is a valid runtime whose offsets line up with the live process.
///
/// A failed read (bogus candidate address landing in unmapped memory, process gone) is
/// simply "not a valid runtime" ⇒ `false`; the caller tries the next candidate.
pub fn check_runtime(
    pid: u32,
    runtime_addr: u64,
    runtime_state_size: u64,
    runtime_interpreters_head: u64,
    threads_head_offset: u64,
    thread_interp_offset: u64,
) -> bool {
    let expected = match reader::read_u64(pid, runtime_addr + runtime_interpreters_head) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let rt_size = runtime_state_size as usize;
    let bytes = match reader::read_memory(pid, runtime_addr, rt_size) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let words: &[u64] = unsafe {
        std::slice::from_raw_parts(
            bytes.as_ptr() as *const u64,
            bytes.len() / std::mem::size_of::<u64>(),
        )
    };

    match check_interpreter_addresses(pid, words, threads_head_offset, thread_interp_offset) {
        Ok(found) => found == expected,
        Err(_) => false,
    }
}

// `check_runtime` is the cookie-less anchor for pre-3.13 runtimes: its sole caller is
// `memory::process::find_runtime_pre_3_13`, which resolves the `_PyRuntime` symbol and
// then confirms the candidate here. 3.13+ finds the runtime by the `"xdebugpy"` cookie
// instead and never touches this module.
