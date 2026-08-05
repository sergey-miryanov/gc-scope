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

    // `Mach::parse` parses a thin image eagerly, so the guard has to run first.
    if !macho_entry_point_math_is_safe(bytes) {
        return None;
    }
    match Mach::parse(bytes).ok()? {
        Mach::Binary(macho) => Some((macho, 0)),
        Mach::Fat(fat) => {
            // Latency only: the hang fix is the `break` below. Without this bound a 64 MB
            // corrupt image still terminates, after walking every entry that fits (~2.4s
            // debug). Mutation testing reports the bound unpinned for that reason: only a
            // timing assertion could tell the difference, and that flakes.
            if fat.narches > bytes.len() / goblin::mach::fat::SIZEOF_FAT_ARCH {
                return None;
            }
            let want = if cfg!(target_arch = "aarch64") {
                goblin::mach::cputype::CPU_TYPE_ARM64
            } else {
                goblin::mach::cputype::CPU_TYPE_X86_64
            };
            for arch in fat.iter_arches() {
                // `narches` is unchecked and the iterator yields one `Err` per unreadable
                // entry rather than stopping, so a `.flatten()` here walks every one of the
                // ~4e9 a corrupt header can claim and spins for minutes.
                // A table that stops parsing cannot be trusted past that point. (Found by
                // `image_scan_survives_adversarial_bytes`; `0xcafebabe` is also the Java
                // class-file magic, so a mapped `.class` reaches here.)
                let Ok(arch) = arch else { break };
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
                //
                // A slice is a thin image in its own right, so it needs the guard too.
                if !macho_entry_point_math_is_safe(slice) {
                    return None;
                }
                return MachO::parse(slice, 0).ok().map(|m| (m, start));
            }
            None
        }
    }
}

/// Whether `MachO::parse` can be handed `bytes` without panicking on its own arithmetic.
///
/// While parsing, goblin computes the entry point as
/// `__TEXT.vmaddr - __TEXT.fileoff + LC_MAIN.entryoff` with **unchecked** `u64` arithmetic
/// (`goblin-0.10.7`, `src/mach/mod.rs:280`), so a `__TEXT` whose `fileoff` exceeds its
/// `vmaddr` panics from inside `parse`, where `.ok()?` cannot see it. Debug builds and the
/// fuzz target have overflow checks on; release wraps instead, into a `MachO::entry` gcscope
/// never reads. `catch_unwind` is no answer — `libfuzzer-sys` aborts from its panic hook
/// before unwinding — so the arithmetic must not run at all. Hence the pre-walk, through
/// goblin's own `LoadCommand` parser to keep the layouts and endianness goblin's.
///
/// Conservative: answers "safe" wherever goblin would bail out first, so a divergent walk
/// can only reject an image goblin would have parsed. Real Mach-Os are unaffected — `__TEXT`
/// maps the head of the file, so its `vmaddr` is never below its `fileoff`.
///
/// Found by the `scan_image_for_version` fuzz target.
fn macho_entry_point_math_is_safe(bytes: &[u8]) -> bool {
    use goblin::mach::header::{SIZEOF_HEADER_32, SIZEOF_HEADER_64};
    use goblin::mach::load_command::{CommandVariant, LoadCommand};

    // No thin Mach-O header here — a fat header, or not Mach-O at all. Either way goblin
    // errors out before the arithmetic, so there is nothing to refuse.
    let Ok((_, Some(ctx))) = goblin::mach::parse_magic_and_ctx(bytes, 0) else {
        return true;
    };

    // `ncmds` and `sizeofcmds` are the 5th and 6th `u32` of the header, at the same offsets
    // in both forms — the 64-bit header differs only by a trailing `reserved` word.
    let word = |at: usize| -> Option<u32> {
        let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if ctx.le.is_little() {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        })
    };
    let (Some(ncmds), Some(sizeofcmds)) = (word(16), word(20)) else {
        return true;
    };
    let (ncmds, sizeofcmds) = (ncmds as usize, sizeofcmds as usize);
    // goblin's own bound, restated so this walk sees the commands goblin will see: past it
    // goblin returns `BufferTooShort` without parsing any of them.
    if ncmds > sizeofcmds / 8 || sizeofcmds > bytes.len() {
        return true;
    }

    let mut offset = if ctx.container.is_big() {
        SIZEOF_HEADER_64
    } else {
        SIZEOF_HEADER_32
    };
    let mut text: Option<(u64, u64)> = None; // (vmaddr, fileoff) of the first __TEXT
    let mut entryoff: Option<u64> = None; // first LC_MAIN, the only one dyld honors

    for _ in 0..ncmds {
        // A command goblin cannot parse aborts its loop with an `Err` too.
        let Ok(cmd) = LoadCommand::parse(bytes, &mut offset, ctx.le) else {
            return true;
        };
        let seg = match cmd.command {
            CommandVariant::Segment32(c) => {
                Some((c.segname, u64::from(c.vmaddr), u64::from(c.fileoff)))
            }
            CommandVariant::Segment64(c) => Some((c.segname, c.vmaddr, c.fileoff)),
            CommandVariant::Main(c) => {
                entryoff.get_or_insert(c.entryoff);
                None
            }
            _ => None,
        };
        // goblin compares the raw leading bytes rather than the trimmed name, and takes the
        // first `__TEXT` in load-command order. Match that, or the wrong segment is checked.
        if let Some((segname, vmaddr, fileoff)) = seg
            && segname[..7] == b"__TEXT\0"[..]
            && text.is_none()
        {
            text = Some((vmaddr, fileoff));
        }
    }

    // Without an LC_MAIN goblin takes the LC_UNIXTHREAD address (or 0) verbatim and does no
    // arithmetic; without a `__TEXT` segment it returns `Malformed` instead.
    let (Some(entryoff), Some((vmaddr, fileoff))) = (entryoff, text) else {
        return true;
    };
    vmaddr
        .checked_sub(fileoff)
        .and_then(|base| base.checked_add(entryoff))
        .is_some()
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
        assert!(
            kind(b"\x7fEL").is_none(),
            "3 bytes is below the 4-byte magic"
        );
        assert!(kind(b"not a binary").is_none());
        assert!(kind(&[0u8; 4]).is_none());
    }

    /// A fat header claiming more architectures than the file can hold is refused, not
    /// walked. Unbounded, that spins for minutes and hangs every subcommand through
    /// `version::detect`. The real assertion is that this returns at all; a regression
    /// shows up as the test timing out rather than failing.
    #[test]
    fn parse_macho_refuses_a_fat_header_claiming_impossible_arch_count() {
        // FAT_MAGIC, then nfat_arch = 0xffff_ffff (big-endian), then filler.
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0xff, 0xff, 0xff, 0xff];
        bytes.resize(256, 0x41);
        assert!(parse_macho(&bytes).is_none());

        // The bound is what the buffer can hold, so a plausible-but-still-too-large
        // count is refused too: 256 bytes hold at most 12 twenty-byte entries.
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x40];
        bytes.resize(256, 0x41);
        assert!(parse_macho(&bytes).is_none());
    }

    /// A thin 64-bit LE Mach-O holding one `__TEXT` segment and one LC_MAIN — the three
    /// fields goblin combines into the entry point, and nothing else. Padded past `fileoff`
    /// so the segment's data range stays in bounds; a short buffer fails earlier, for an
    /// unrelated reason.
    fn thin_macho_with_entry_point(vmaddr: u64, fileoff: u64, entryoff: u64) -> Vec<u8> {
        const LC_SEGMENT_64: u32 = 0x19;
        const LC_MAIN: u32 = 0x8000_0028;
        const SEG_CMD_SIZE: u32 = 72;
        const MAIN_CMD_SIZE: u32 = 24;

        let mut b = Vec::new();
        // mach_header_64
        b.extend(0xfeed_facfu32.to_le_bytes()); // magic MH_MAGIC_64
        b.extend(0x0100_0007u32.to_le_bytes()); // cputype CPU_TYPE_X86_64
        b.extend(3u32.to_le_bytes()); // cpusubtype
        b.extend(2u32.to_le_bytes()); // filetype MH_EXECUTE
        b.extend(2u32.to_le_bytes()); // ncmds
        b.extend((SEG_CMD_SIZE + MAIN_CMD_SIZE).to_le_bytes()); // sizeofcmds
        b.extend(0u32.to_le_bytes()); // flags
        b.extend(0u32.to_le_bytes()); // reserved

        // segment_command_64
        b.extend(LC_SEGMENT_64.to_le_bytes());
        b.extend(SEG_CMD_SIZE.to_le_bytes());
        b.extend(b"__TEXT\0\0\0\0\0\0\0\0\0\0"); // segname, 16 bytes
        b.extend(vmaddr.to_le_bytes());
        b.extend(0u64.to_le_bytes()); // vmsize
        b.extend(fileoff.to_le_bytes());
        b.extend(0u64.to_le_bytes()); // filesize — keeps the data range empty
        b.extend(0u32.to_le_bytes()); // maxprot
        b.extend(0u32.to_le_bytes()); // initprot
        b.extend(0u32.to_le_bytes()); // nsects
        b.extend(0u32.to_le_bytes()); // flags

        // entry_point_command
        b.extend(LC_MAIN.to_le_bytes());
        b.extend(MAIN_CMD_SIZE.to_le_bytes());
        b.extend(entryoff.to_le_bytes());
        b.extend(0u64.to_le_bytes()); // stacksize

        // `fileoff` indexes into the buffer during the segment parse, so it has to fit.
        if let Some(needed) = usize::try_from(fileoff).ok().filter(|n| *n > b.len()) {
            b.resize(needed, 0);
        }
        b
    }

    /// goblin resolves the entry point with unchecked arithmetic while parsing, so a `__TEXT`
    /// whose `fileoff` is above its `vmaddr` panics it from the inside (see
    /// `macho_entry_point_math_is_safe`). The assertion that matters is that these return at
    /// all — a regression is a panic, not a wrong answer.
    #[test]
    fn parse_macho_refuses_an_image_that_would_overflow_goblins_entry_point_math() {
        // Underflow: __TEXT starts below its own file offset.
        assert!(parse_macho(&thin_macho_with_entry_point(0, 0x20, 0)).is_none());
        // Overflow on the other side: the base is valid, adding `entryoff` wraps.
        assert!(parse_macho(&thin_macho_with_entry_point(u64::MAX, 0, u64::MAX)).is_none());
    }

    /// The guard rejects only the impossible arithmetic, not every image with an entry point.
    /// A real `__TEXT` maps the head of the file — `vmaddr` 0x1_0000_0000 over `fileoff` 0 —
    /// and must still parse, or the guard has silently disabled macOS.
    #[test]
    fn parse_macho_still_accepts_a_well_formed_entry_point() {
        let bytes = thin_macho_with_entry_point(0x1_0000_0000, 0, 0x4000);
        let (macho, slice_at) = parse_macho(&bytes).expect("a well-formed thin image parses");
        assert_eq!(slice_at, 0, "a thin image is not a fat slice");
        assert_eq!(
            macho.entry, 0x1_0000_4000,
            "vmaddr - fileoff + entryoff, the value the guard proved safe to compute"
        );
    }
}
