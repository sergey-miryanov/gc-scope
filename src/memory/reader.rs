use anyhow::{Context, Result};
use read_process_memory::{copy_address, ProcessHandle};

fn create_handle(pid: u32) -> Result<ProcessHandle> {
    (pid as read_process_memory::Pid)
        .try_into()
        .context("Failed to create process handle")
}

pub fn read_memory(pid: u32, addr: u64, size: usize) -> Result<Vec<u8>> {
    let handle = create_handle(pid)?;
    let bytes = copy_address(addr as usize, size, &handle)
        .context("Failed to read process memory")?;
    Ok(bytes)
}

pub fn read_u64(pid: u32, addr: u64) -> Result<u64> {
    let handle = create_handle(pid)?;
    let mut buf = [0u8; 8];
    let bytes = copy_address(addr as usize, 8, &handle)
        .context("Failed to read process memory")?;
    buf.copy_from_slice(&bytes);
    Ok(u64::from_le_bytes(buf))
}
