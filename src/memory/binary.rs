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
        if !lower.contains("python") {
            continue;
        }
        // Which mapping is the image *base* differs by platform, and the answer
        // is not "the lowest one" everywhere:
        //
        //  * ELF/PE — the first mapping is the load base, and section addresses
        //    are rebased off it (see `elf_load_bias`).
        //  * Mach-O — the kernel attributes several unrelated low-address
        //    reservations to the image path, so the first mapping is typically a
        //    no-access `---` range well below the real image. The Mach-O header
        //    sits at the start of __TEXT, which is the executable mapping, and
        //    every section `vmaddr` is relative to that. Anchoring on the first
        //    mapping instead lands ~14 MB low — still inside *some* mapped
        //    region, so the read succeeds and silently returns garbage rather
        //    than failing cleanly.
        if cfg!(target_os = "macos") && !m.is_exec() {
            continue;
        }
        if !modules.iter().any(|(p, _)| *p == path) {
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
                if arch.cputype != want {
                    continue;
                }
                let start = arch.offset as usize;
                let end = start.checked_add(arch.size as usize)?;
                let slice = bytes.get(start..end)?;
                // Parse the slice as a standalone image (offset 0), NOT in place
                // via `MachO::parse(bytes, start)`. A slice's internal file
                // offsets — `symtab.symoff`/`stroff`, `section.offset` — are
                // relative to the slice, and goblin indexes them directly into
                // whatever buffer it is handed. Parsing in place makes it read the
                // symbol table from the wrong slice and silently yield **no
                // symbols**, which is invisible to anything that only touches
                // virtual addresses (`vmaddr`, `n_value`) and breaks everything
                // that reads the symbol table.
                return MachO::parse(slice, 0).ok().map(|m| (m, start));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(bytes: &[u8]) -> Option<&'static str> {
        classify(bytes).map(|k| match k {
            BinaryKind::Elf => "elf",
            BinaryKind::Pe => "pe",
            BinaryKind::MachO => "macho",
        })
    }

    /// The magic table is the first fork in every finder, and each platform's real
    /// image was only ever confirmed by a live CI leg (ADR 0004). Pin the bytes.
    #[test]
    fn classifies_each_format_magic() {
        assert_eq!(kind(b"\x7fELF\x02\x01\x01\x00"), Some("elf"));
        assert_eq!(kind(b"MZ\x90\x00"), Some("pe"));
        // Mach-O 32/64-bit, both byte orders.
        assert_eq!(kind(&0xfeedfaceu32.to_le_bytes()), Some("macho"));
        assert_eq!(kind(&0xcefaedfeu32.to_le_bytes()), Some("macho"));
        assert_eq!(kind(&0xfeedfacfu32.to_le_bytes()), Some("macho"));
        assert_eq!(kind(&0xcffaedfeu32.to_le_bytes()), Some("macho"));
        // Universal ("fat") — what every macOS framework build actually is.
        assert_eq!(kind(&0xcafebabeu32.to_le_bytes()), Some("macho"));
        assert_eq!(kind(&0xbebafecau32.to_le_bytes()), Some("macho"));
    }

    #[test]
    fn rejects_short_and_unknown_input() {
        assert!(kind(b"").is_none());
        assert!(kind(b"\x7fEL").is_none(), "3 bytes is below the 4-byte magic");
        assert!(kind(b"not a binary").is_none());
        assert!(kind(&[0u8; 4]).is_none());
    }
}
