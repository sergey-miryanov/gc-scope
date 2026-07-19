use anyhow::Result;
use remoteprocess::Process as RemoteProcess;

use crate::memory::{binary, reader};
use crate::remote_debugging::{check_interpreter, offsets::offset_table::OffsetTable, version};

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
// Pre-3.13 runtime finding (no cookie exists yet)
// ---------------------------------------------------------------------------

/// Find `_PyRuntime` for a pre-3.13 interpreter.
///
/// The `"xdebugpy"` cookie (and its dedicated `PyRuntime` section) only exist from
/// 3.13 on, so [`find_runtime_module`]'s cookie anchor cannot be used here. Instead we
/// resolve the `_PyRuntime` symbol from each Python module's symbol table and confirm
/// the candidate structurally with the interpreter/thread cross-reference round-trip
/// (`interpreters_head → threads_head → interp`), which is what the cookie stands in
/// for on 3.13+. `table` supplies the pre-3.13 field offsets that drive the walk.
pub fn find_runtime_pre_3_13(pid: u32, table: &OffsetTable) -> Result<(u64, String)> {
    let modules = binary::find_python_modules(pid)?;
    if modules.is_empty() {
        anyhow::bail!("No python-related modules found in process {}", pid);
    }

    // Pre-3.13 has no `runtime_state.size`; scan a small window past the
    // interpreters_head field (matches the old `verify_with_table`).
    let scan_size = table.runtime_interpreters_head + 64;

    for (path, base_addr) in &modules {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let addr = match version::resolve_symbol_in_bytes(&bytes, *base_addr, "_PyRuntime") {
            Some(a) => a,
            None => continue,
        };

        if check_interpreter::check_runtime(
            pid,
            addr,
            scan_size,
            table.runtime_interpreters_head,
            table.interp_threads_head,
            table.thread_interp,
        ) {
            return Ok((addr, path.clone()));
        }
    }

    anyhow::bail!(
        "Could not find a valid pre-3.13 _PyRuntime in process {} \
         (symbol missing or cross-reference validation failed)",
        pid
    );
}

// ---------------------------------------------------------------------------
// Per-process lookup
// ---------------------------------------------------------------------------

fn try_find_runtime(pid: u32) -> Result<(u64, String)> {
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
            return Ok((addr, path.clone()));
        }
    }

    anyhow::bail!(
        "Could not find valid PyRuntime section in process {}",
        pid
    );
}

fn search_pid_and_children(pid: u32, depth: u32) -> Result<(u64, String)> {
    if let Ok(found) = try_find_runtime(pid) {
        return Ok(found);
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
            Ok(found) => return Ok(found),
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
    Ok(search_pid_and_children(pid, 0)?.0)
}

/// Like [`find_runtime`], but also returns the on-disk path of the Python
/// module (interpreter or libpython) whose `PyRuntime` section validated. That
/// path — not `argv[0]` — is the correct identity for a layout cache keyed by
/// binary (see `docs/pysession-plan.md` §6).
pub fn find_runtime_module(pid: u32) -> Result<(u64, String)> {
    search_pid_and_children(pid, 0)
}

/// Best-effort command line for `pid`, as a single space-joined string.
///
/// Used only as a change-detector for a reused PID (`PySession::revalidate`),
/// never on the hot read path. Returns `None` if the process is gone or its
/// command line is unavailable.
pub fn read_cmdline(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    let mut sys = System::new();
    let spid = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), true);
    let process = sys.process(spid)?;
    let cmd = process
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    Some(cmd)
}
