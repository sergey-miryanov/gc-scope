use anyhow::Result;
use remoteprocess::Process as RemoteProcess;

use crate::memory::{binary, reader};

const COOKIE: &[u8] = b"xdebugpy";
const MAX_DEPTH: u32 = 3;

#[allow(clippy::unnecessary_cast)]
pub fn get_child_pids(parent: u32) -> Vec<u32> {
    let pid = parent as remoteprocess::Pid;
    let process = match RemoteProcess::new(pid) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    process
        .child_processes()
        .ok()
        .map(|pairs| {
            pairs
                .into_iter()
                .map(|(child, _)| child as u32)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Binary format section finding
// ---------------------------------------------------------------------------

fn find_section_in_elf(bytes: &[u8], region_start: usize) -> Option<u64> {
    let elf = goblin::elf::Elf::parse(bytes).ok()?;
    let section = elf.section_headers.iter().find(|s| {
        elf.shdr_strtab
            .get_at(s.sh_name)
            .and_then(|name| {
                let s = name.trim_end_matches('\0');
                if s == "PyRuntime" { Some(true) } else { None }
            })
            .unwrap_or(false)
    })?;

    let load_bias = binary::elf_load_bias(&elf)?;
    let runtime_addr = (region_start as u64).wrapping_add(section.sh_addr.wrapping_sub(load_bias));
    Some(runtime_addr)
}

fn find_section_in_pe(bytes: &[u8], region_start: usize) -> Option<u64> {
    let pe = goblin::pe::PE::parse(bytes).ok()?;
    let section = pe.sections.iter().find(|s| {
        if let Ok(name) = s.name() {
            let trimmed = name.trim_end_matches('\0');
            trimmed == "PyRuntim" || trimmed == "PyRuntime"
        } else {
            false
        }
    })?;
    let runtime_addr = (region_start as u64).wrapping_add(section.virtual_address as u64);
    Some(runtime_addr)
}

fn find_section_in_macho(bytes: &[u8], region_start: usize) -> Option<u64> {
    let macho = goblin::mach::MachO::parse(bytes, 0).ok()?;

    let mut text_vmaddr: Option<u64> = None;
    let mut runtime_addr: Option<u64> = None;

    for segment in &macho.segments {
        let segname = segment.name().ok()?;
        if segname == "__TEXT" {
            text_vmaddr = Some(segment.vmaddr);
        }
        if segname == "__DATA" || segname == "__DATA_CONST" || segname == "__AUTH_CONST" {
            for sect in segment.sections().ok()? {
                let sectname = sect.0.name().ok()?;
                if sectname == "PyRuntime" {
                    let vmaddr = text_vmaddr?;
                    runtime_addr = Some(
                        (region_start as u64)
                            .wrapping_add(sect.0.addr.wrapping_sub(vmaddr)),
                    );
                }
            }
        }
    }
    runtime_addr
}

// ---------------------------------------------------------------------------
// Cookie validation
// ---------------------------------------------------------------------------

fn validate_cookie(pid: u32, addr: u64) -> Result<bool> {
    let data = reader::read_memory(pid, addr, COOKIE.len())?;
    Ok(&data[..] == COOKIE)
}

// ---------------------------------------------------------------------------
// Per-process lookup
// ---------------------------------------------------------------------------

fn try_find_runtime(pid: u32) -> Result<u64> {
    let modules = binary::find_python_modules(pid)?;
    if modules.is_empty() {
        anyhow::bail!("No python-related modules found in process {}", pid);
    }

    for (path, base_addr) in &modules {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let addr = match binary::classify(&bytes) {
            Some(binary::BinaryKind::Elf) => find_section_in_elf(&bytes, *base_addr),
            Some(binary::BinaryKind::Pe) => find_section_in_pe(&bytes, *base_addr),
            Some(binary::BinaryKind::MachO) => find_section_in_macho(&bytes, *base_addr),
            None => continue,
        };

        if let Some(addr) = addr
            && validate_cookie(pid, addr)?
        {
            return Ok(addr);
        }
    }

    anyhow::bail!(
        "Could not find valid PyRuntime section in process {}",
        pid
    );
}

fn search_pid_and_children(pid: u32, depth: u32) -> Result<u64> {
    if let Ok(addr) = try_find_runtime(pid) {
        return Ok(addr);
    }

    if depth >= MAX_DEPTH {
        anyhow::bail!(
            "Could not find valid PyRuntime in process {} or its children",
            pid
        );
    }

    let children = get_child_pids(pid);
    if children.is_empty() {
        return try_find_runtime(pid);
    }

    let mut errors = Vec::new();
    for child in children {
        match search_pid_and_children(child, depth + 1) {
            Ok(addr) => return Ok(addr),
            Err(e) => errors.push((child, e)),
        }
    }

    let details: Vec<String> = errors
        .iter()
        .map(|(p, e)| format!("  child {}: {}", p, e))
        .collect();
    anyhow::bail!(
        "Could not find valid PyRuntime section in process {} or its children:\n{}",
        pid,
        details.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn find_runtime(pid: u32) -> Result<u64> {
    search_pid_and_children(pid, 0)
}
