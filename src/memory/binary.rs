use anyhow::Result;
use proc_maps::get_process_maps;

/// Magic byte classification for binary formats.
pub enum BinaryKind {
    Elf,
    Pe,
    MachO,
}

/// Categories the binary format from its magic bytes.
pub fn classify(bytes: &[u8]) -> Option<BinaryKind> {
    if bytes.len() < 4 {
        return None;
    }
    match u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) {
        0x464c457f => Some(BinaryKind::Elf),
        0x00905a4d | 0x905a4d00 => Some(BinaryKind::Pe),
        0xfeedface | 0xcefaedfe | 0xfeedfacf | 0xcffaedfe | 0xbebafeca | 0xcafebabe => {
            Some(BinaryKind::MachO)
        }
        _ => None,
    }
}

/// Find Python-related modules in a process.
///
/// Returns a list of (file_path, memory_base_address) pairs, deduplicated by path.
pub fn find_python_modules(pid: u32) -> Result<Vec<(String, usize)>> {
    let maps = get_process_maps(pid as proc_maps::Pid)
        .map_err(|e| anyhow::anyhow!("Failed to get process memory maps: {}", e))?;

    let mut modules: Vec<(String, usize)> = Vec::new();
    for m in &maps {
        let path = match m.filename().and_then(|p| p.to_str()) {
            Some(p) => p,
            None => continue,
        };
        let lower = path.to_lowercase();
        if lower.contains("python")
            && !modules.iter().any(|(p, _)| *p == path)
        {
            modules.push((path.to_string(), m.start()));
        }
    }
    Ok(modules)
}

/// Parse a Mach-O image, transparently unwrapping a universal ("fat") binary.
///
/// macOS ships Python as a framework built `universal2` (x86_64 + arm64), so
/// offset 0 holds a *fat header* rather than a Mach-O header and a plain
/// `MachO::parse(bytes, 0)` fails outright. Every macOS code path — both the
/// `PyRuntime` section lookup and symbol resolution — has to go through here.
///
/// gcscope reads a process on the machine it runs on, so the slice to pick is
/// the host architecture: the target executes that slice, and its addresses are
/// the ones present in the process's memory map.
///
/// Returns the parsed image **and the slice's offset within `bytes`**. Callers
/// working in virtual addresses (`vmaddr`, `n_value`) can ignore the offset;
/// callers that index back into `bytes` with a *file* offset (`sect.offset`)
/// must add it, because a slice's internal file offsets are relative to the
/// start of that slice rather than the start of the fat file.
pub fn parse_macho(bytes: &[u8]) -> Option<(goblin::mach::MachO<'_>, usize)> {
    use goblin::mach::{Mach, MachO};

    match Mach::parse(bytes).ok()? {
        Mach::Binary(macho) => Some((macho, 0)),
        Mach::Fat(fat) => {
            let want = if cfg!(target_arch = "aarch64") {
                goblin::mach::cputype::CPU_TYPE_ARM64
            } else {
                goblin::mach::cputype::CPU_TYPE_X86_64
            };
            for arch in fat.iter_arches().flatten() {
                if arch.cputype == want {
                    let at = arch.offset as usize;
                    return MachO::parse(bytes, at).ok().map(|m| (m, at));
                }
            }
            None
        }
    }
}

/// Calculate the load bias for an ELF binary.
///
/// The load bias is the virtual address of the first PT_LOAD segment,
/// aligned down to the page boundary. Used to convert section/symbol
/// virtual addresses to runtime addresses.
pub fn elf_load_bias(elf: &goblin::elf::Elf) -> Option<u64> {
    let first_load = elf
        .program_headers
        .iter()
        .find(|ph| ph.p_type == goblin::elf::program_header::PT_LOAD)?;
    Some(first_load.p_vaddr - (first_load.p_vaddr % first_load.p_align))
}
