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
