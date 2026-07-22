use anyhow::{Context, Result};
use read_process_memory::{ProcessHandle, copy_address};

/// Open a process handle once and reuse it for many reads.
///
/// Prefer this + the `*_h` readers over the per-call `read_memory`/`read_u64`
/// on any hot path (interpreter walks, poll loops): those open and close a
/// fresh handle on every single read.
pub fn open_handle(pid: u32) -> Result<ProcessHandle> {
    (pid as read_process_memory::Pid)
        .try_into()
        .context("Failed to create process handle")
}

/// Read `size` bytes at `addr` through an already-open handle.
pub fn read_memory_h(handle: &ProcessHandle, addr: u64, size: usize) -> Result<Vec<u8>> {
    copy_address(addr as usize, size, handle).context("Failed to read process memory")
}

/// Read a little-endian `u64` at `addr` through an already-open handle.
pub fn read_u64_h(handle: &ProcessHandle, addr: u64) -> Result<u64> {
    let bytes = read_memory_h(handle, addr, 8)?;
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

/// Read `size` bytes at `addr`, opening a fresh handle for this call.
///
/// Convenience for one-shot reads; delegates to [`read_memory_h`]. Hot paths
/// should hold a handle from [`open_handle`] instead.
pub fn read_memory(pid: u32, addr: u64, size: usize) -> Result<Vec<u8>> {
    let handle = open_handle(pid)?;
    read_memory_h(&handle, addr, size)
}

/// Read a little-endian `u64` at `addr`, opening a fresh handle for this call.
pub fn read_u64(pid: u32, addr: u64) -> Result<u64> {
    let handle = open_handle(pid)?;
    read_u64_h(&handle, addr)
}
