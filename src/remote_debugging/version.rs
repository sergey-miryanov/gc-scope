use anyhow::{bail, Result};
use goblin::{elf, pe};

use crate::memory::{binary, reader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PythonVersion {
    pub major: u8,
    pub minor: u8,
    pub micro: u8,
    /// 0xA=alpha, 0xB=beta, 0xC=rc, 0xF=final
    pub release_level: u8,
    pub serial: u8,
}

impl PythonVersion {
    /// Parse from PY_VERSION_HEX encoding.
    /// Format: (major << 24) | (minor << 16) | (micro << 8) | (release_level << 4) | serial
    pub fn from_hex(v: u64) -> Option<Self> {
        let major = ((v >> 24) & 0xff) as u8;
        let minor = ((v >> 16) & 0xff) as u8;
        if major == 0 && minor == 0 {
            return None;
        }
        Some(PythonVersion {
            major,
            minor,
            micro: ((v >> 8) & 0xff) as u8,
            release_level: ((v >> 4) & 0xf) as u8,
            serial: (v & 0xf) as u8,
        })
    }

    /// Encode to PY_VERSION_HEX format.
    #[allow(dead_code)]
    pub fn to_hex(self) -> u64 {
        (self.major as u64) << 24
            | (self.minor as u64) << 16
            | (self.micro as u64) << 8
            | (self.release_level as u64) << 4
            | self.serial as u64
    }

    /// Parse from a version string like "3.15.0a8", "3.12.0", or "3.11.0rc1".
    ///
    /// Accepts strings that may have trailing content after the version
    /// (e.g. "3.12.0 (tags/v3.12.0, ...)" or "Python 3.11.0").
    #[allow(dead_code)]
    pub fn from_string(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix("Python ").unwrap_or(s);

        let mut chars = s.char_indices().peekable();
        let major = parse_digits(&mut chars)?;
        if chars.next()?.1 != '.' { return None; }
        let minor = parse_digits(&mut chars)?;

        let micro = if chars.peek().map(|&(_, c)| c) == Some('.') {
            chars.next();
            parse_digits(&mut chars)?
        } else {
            0
        };

        let (release_level, serial) = match chars.peek() {
            Some(&(_, c)) if c == 'a' || c == 'b' => {
                let level = if c == 'a' { 0xA } else { 0xB };
                chars.next();
                let serial = parse_digits(&mut chars)?;
                (level, serial.min(0xF))
            }
            Some(&(_, 'r')) => {
                chars.next();
                if chars.next()?.1 != 'c' { return None; }
                let serial = parse_digits(&mut chars)?;
                (0xC, serial.min(0xF))
            }
            _ => (0xF, 0),
        };

        Some(PythonVersion { major, minor, micro, release_level, serial })
    }
}

impl std::fmt::Display for PythonVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)?;
        match self.release_level {
            0xF => {}
            0xA => write!(f, "a{}", self.serial)?,
            0xB => write!(f, "b{}", self.serial)?,
            0xC => write!(f, "rc{}", self.serial)?,
            _ => write!(f, "-{:x}{}", self.release_level, self.serial)?,
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn parse_digits(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> Option<u8> {
    let mut n: u8 = 0;
    let mut started = false;
    while let Some(&(_, c)) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.checked_mul(10)?.checked_add((c as u8) - b'0')?;
        chars.next();
        started = true;
    }
    if started { Some(n) } else { None }
}

// ── Public API ──────────────────────────────────────────────

pub fn detect(pid: u32) -> Result<PythonVersion> {
    let modules = binary::find_python_modules(pid)?;
    if modules.is_empty() {
        bail!("No Python modules found in process {}", pid);
    }

    for (path, base_addr) in &modules {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        let sym_addr = resolve_symbol_in_bytes(&bytes, *base_addr, "Py_Version");

        if let Some(abs_addr) = sym_addr
            && let Some(ver) = read_version_from_process(pid, abs_addr)
            && ver.major == 3
        {
            return Ok(ver);
        }
    }

    for (path, _base_addr) in &modules {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // Scan the read-only data section first (where `PY_VERSION` lives), which
        // avoids stray `"3.x"` bytes elsewhere in the image; fall back to the whole
        // image if that section can't be located or holds no match, so no build regresses.
        let scanned = match read_only_data(&bytes) {
            Some(ro) => scan_for_version_string(ro).or_else(|| scan_for_version_string(&bytes)),
            None => scan_for_version_string(&bytes),
        };
        if let Some(ver) = scanned
            && ver.major == 3
        {
            return Ok(ver);
        }
    }

    bail!("Could not detect Python version for pid {}", pid);
}

// ── Symbol resolution ───────────────────────────────────────

/// Resolve `name` to an absolute load address within one already-read module image.
/// Dispatches on the binary format; returns `None` if the symbol is absent.
pub fn resolve_symbol_in_bytes(bytes: &[u8], base_addr: usize, name: &str) -> Option<u64> {
    match binary::classify(bytes) {
        Some(binary::BinaryKind::Elf) => resolve_symbol_elf(bytes, base_addr, name),
        Some(binary::BinaryKind::Pe) => resolve_symbol_pe(bytes, base_addr, name),
        Some(binary::BinaryKind::MachO) => resolve_symbol_macho(bytes, base_addr, name),
        None => None,
    }
}

// ── Internal helpers ────────────────────────────────────────

fn read_version_from_process(pid: u32, addr: u64) -> Option<PythonVersion> {
    if let Ok(bytes) = reader::read_memory(pid, addr, 8) {
        let val64 = u64::from_le_bytes(bytes[..8].try_into().unwrap());
        if let Some(ver) = PythonVersion::from_hex(val64) {
            return Some(ver);
        }
    }
    if let Ok(bytes) = reader::read_memory(pid, addr, 4) {
        let val32 = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        if let Some(ver) = PythonVersion::from_hex(val32 as u64) {
            return Some(ver);
        }
    }
    None
}

fn resolve_symbol_elf(bytes: &[u8], base_addr: usize, sym_name: &str) -> Option<u64> {
    let elf_obj = elf::Elf::parse(bytes).ok()?;
    let load_bias = binary::elf_load_bias(&elf_obj)?;

    for sym in elf_obj.dynsyms.iter() {
        if elf_obj.dynstrtab.get_at(sym.st_name) == Some(sym_name) {
            return Some(
                (base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)),
            );
        }
    }

    for sym in elf_obj.syms.iter() {
        if elf_obj.strtab.get_at(sym.st_name) == Some(sym_name) {
            return Some(
                (base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)),
            );
        }
    }

    None
}

fn resolve_symbol_pe(bytes: &[u8], base_addr: usize, sym_name: &str) -> Option<u64> {
    let pe_obj = pe::PE::parse(bytes).ok()?;

    for export in &pe_obj.exports {
        if export.name == Some(sym_name) {
            return Some((base_addr as u64).wrapping_add(export.rva as u64));
        }
    }

    None
}

fn resolve_symbol_macho(bytes: &[u8], base_addr: usize, sym_name: &str) -> Option<u64> {
    // Virtual addresses only below, so the slice offset is not needed here.
    let (macho, _) = binary::parse_macho(bytes)?;

    let text_vmaddr = macho
        .segments
        .iter()
        .find_map(|seg| {
            let name = seg.name().ok()?;
            if name == "__TEXT" {
                Some(seg.vmaddr)
            } else {
                None
            }
        })?;

    if let Some(symbols) = &macho.symbols {
        for (name, nlist) in symbols.iter().flatten() {
            // Mach-O prefixes C symbols with an underscore, so `_PyRuntime` is
            // stored as `__PyRuntime` and `Py_Version` as `_Py_Version`. Accept
            // the undecorated spelling too rather than assuming either form.
            let matches = name == sym_name || name.strip_prefix('_') == Some(sym_name);
            if matches && !nlist.is_undefined() {
                return Some(
                    (base_addr as u64)
                        .wrapping_add(nlist.n_value.wrapping_sub(text_vmaddr)),
                );
            }
        }
    }

    None
}

fn parse_micro_digits(bytes: &[u8], start: usize) -> Option<(u8, usize)> {
    let mut j = start;
    let mut val: u8 = 0;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        val = val.checked_mul(10)?.checked_add(bytes[j] - b'0')?;
        j += 1;
    }
    if j == start { None } else { Some((val, j)) }
}

/// File-offset range of the binary's read-only data section — PE `.rdata`, ELF
/// `.rodata`, Mach-O `__TEXT,__cstring` — where the `PY_VERSION` string literal is
/// emitted. `None` if the format isn't recognized or the section isn't present.
fn ro_data_range(bytes: &[u8]) -> Option<(usize, usize)> {
    match binary::classify(bytes)? {
        binary::BinaryKind::Pe => {
            let pe = pe::PE::parse(bytes).ok()?;
            let s = pe.sections.iter().find(|s| {
                s.name().map(|n| n.trim_end_matches('\0') == ".rdata").unwrap_or(false)
            })?;
            Some((s.pointer_to_raw_data as usize, s.size_of_raw_data as usize))
        }
        binary::BinaryKind::Elf => {
            let elf = elf::Elf::parse(bytes).ok()?;
            let s = elf.section_headers.iter().find(|s| {
                elf.shdr_strtab
                    .get_at(s.sh_name)
                    .map(|n| n.trim_end_matches('\0') == ".rodata")
                    .unwrap_or(false)
            })?;
            Some((s.sh_offset as usize, s.sh_size as usize))
        }
        binary::BinaryKind::MachO => {
            let (macho, slice_at) = binary::parse_macho(bytes)?;
            for seg in &macho.segments {
                if seg.name().ok()? != "__TEXT" {
                    continue;
                }
                for (sect, _data) in seg.sections().ok()? {
                    if sect.name().ok()? == "__cstring" {
                        // File offset, so it needs rebasing onto the fat slice.
                        return Some((slice_at + sect.offset as usize, sect.size as usize));
                    }
                }
            }
            None
        }
    }
}

/// The read-only data section as a byte slice, or `None` if it can't be located.
fn read_only_data(bytes: &[u8]) -> Option<&[u8]> {
    let (start, len) = ro_data_range(bytes)?;
    let end = start.saturating_add(len).min(bytes.len());
    (start < end).then(|| &bytes[start..end])
}

fn scan_for_version_string(bytes: &[u8]) -> Option<PythonVersion> {
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] != b'3' || bytes[i + 1] != b'.' {
            i += 1;
            continue;
        }
        if i > 0 && bytes[i - 1].is_ascii_digit() {
            i += 1;
            continue;
        }

        // Parse minor. A `"3."` not followed by digits (e.g. `"3.E"` in a float
        // literal) is not a version string — advance and keep scanning rather than
        // aborting the whole scan, which would miss the real version further in.
        let Some((minor, mut j)) = parse_micro_digits(bytes, i + 2) else {
            i += 1;
            continue;
        };

        // Require a micro component. The embedded `PY_VERSION` is always fully
        // qualified (`"X.Y.Z"`), so a bare `"3.1"` (e.g. in unrelated text, or a
        // truncated prefix) is a false positive: skip it and keep scanning for the
        // real version. Without this, a stray `"3.1 "` shadows the true `"3.10.x"`.
        if bytes.get(j).copied() != Some(b'.') {
            i += 1;
            continue;
        }
        let micro = match parse_micro_digits(bytes, j + 1) {
            Some((m, next)) => {
                j = next;
                m
            }
            None => {
                i += 1;
                continue;
            }
        };

        // Parse optional release suffix: aN, bN, rcN
        let (release_level, serial) = match bytes.get(j).copied() {
            Some(b'a') => match parse_micro_digits(bytes, j + 1) {
                Some((s, next)) => {
                    j = next;
                    (0xA, s)
                }
                None => {
                    i += 1;
                    continue;
                }
            },
            Some(b'b') => match parse_micro_digits(bytes, j + 1) {
                Some((s, next)) => {
                    j = next;
                    (0xB, s)
                }
                None => {
                    i += 1;
                    continue;
                }
            },
            Some(b'r') if bytes.get(j + 1).copied() == Some(b'c') => {
                match parse_micro_digits(bytes, j + 2) {
                    Some((s, next)) => {
                        j = next;
                        (0xC, s)
                    }
                    None => {
                        i += 1;
                        continue;
                    }
                }
            }
            _ => (0xF, 0),
        };

        // Validate trailing context
        let next = bytes.get(j).copied().unwrap_or(0);
        if next == 0 || next == b' ' || next == b'(' || next == b'\n' || next == b'\r'
            || next == b'\t' || next == b'"'
        {
            return Some(PythonVersion { major: 3, minor, micro, release_level, serial });
        }
        i = j;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u8, minor: u8, micro: u8, release_level: u8, serial: u8) -> PythonVersion {
        PythonVersion { major, minor, micro, release_level, serial }
    }

    #[test]
    fn hex_round_trips_every_release_level() {
        // One per level, including every hex the LAYOUTS registry can hold.
        for hex in [
            0x030800f0u64, // 3.8.0 final
            0x030d01f0,    // 3.13.1 final
            0x030f00a8,    // 3.15.0a8
            0x030f00b1,    // 3.15.0b1
            0x030f00c2,    // 3.15.0rc2
            0x031000a0,    // 3.16.0a0
        ] {
            let parsed = PythonVersion::from_hex(hex).expect("valid hex");
            assert_eq!(parsed.to_hex(), hex, "round-trip failed for {hex:#010x}");
        }
    }

    #[test]
    fn hex_decodes_each_field() {
        let parsed = PythonVersion::from_hex(0x030f00b1).unwrap();
        assert_eq!(parsed, v(3, 15, 0, 0xB, 1));
    }

    #[test]
    fn hex_rejects_absent_major_and_minor() {
        // The guard is on major AND minor both being zero — a zeroed read.
        assert_eq!(PythonVersion::from_hex(0), None);
        assert_eq!(PythonVersion::from_hex(0x0000_00f0), None);
        // A zero major with a non-zero minor is still decoded (not our call to reject).
        assert!(PythonVersion::from_hex(0x0001_0000).is_some());
    }

    #[test]
    fn from_string_parses_the_shapes_detect_actually_sees() {
        assert_eq!(PythonVersion::from_string("3.15.0a8"), Some(v(3, 15, 0, 0xA, 8)));
        assert_eq!(PythonVersion::from_string("3.12.0"), Some(v(3, 12, 0, 0xF, 0)));
        assert_eq!(PythonVersion::from_string("3.11.0rc1"), Some(v(3, 11, 0, 0xC, 1)));
        assert_eq!(PythonVersion::from_string("Python 3.11.0"), Some(v(3, 11, 0, 0xF, 0)));
        // Trailing content is allowed: this is what `python --version` and the
        // binary's embedded version string look like.
        assert_eq!(
            PythonVersion::from_string("3.12.0 (tags/v3.12.0, Oct  2 2023)"),
            Some(v(3, 12, 0, 0xF, 0))
        );
        // Micro is optional.
        assert_eq!(PythonVersion::from_string("3.12"), Some(v(3, 12, 0, 0xF, 0)));
    }

    #[test]
    fn from_string_rejects_non_versions() {
        for s in ["", "3", "3.x", "x3.12", "..", "3."] {
            assert_eq!(PythonVersion::from_string(s), None, "should reject {s:?}");
        }
    }

    #[test]
    fn from_string_rejects_overflowing_component() {
        // parse_digits accumulates into a u8 with checked_mul/checked_add. Without
        // those guards "3.999.0" would wrap to a plausible-looking minor and gcscope
        // would silently resolve the wrong layout.
        assert_eq!(PythonVersion::from_string("3.999.0"), None);
        assert_eq!(PythonVersion::from_string("3.12.999"), None);
    }

    #[test]
    fn display_round_trips_from_string() {
        for s in ["3.15.0a8", "3.15.0b1", "3.15.0rc1", "3.12.0", "3.8.19"] {
            let parsed = PythonVersion::from_string(s).expect(s);
            assert_eq!(parsed.to_string(), s);
        }
    }

    // ── binary version-string scan (the on-disk fallback for `detect`) ──
    // `scan_for_version_string` is the fallback when the `Py_Version` symbol can't
    // be read; it walks raw rodata bytes. Feeding it byte slices exercises the whole
    // scanner (and `parse_micro_digits`) without a real binary.

    /// A fully-qualified `PY_VERSION` literal embedded in surrounding bytes is found,
    /// with every field decoded — including the release suffix.
    #[test]
    fn scan_finds_an_embedded_fully_qualified_version() {
        assert_eq!(
            scan_for_version_string(b"\x00\x00garbage3.13.1 (main)\x00"),
            Some(v(3, 13, 1, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"junk\x003.15.0b1\x00more"),
            Some(v(3, 15, 0, 0xB, 1))
        );
        assert_eq!(
            scan_for_version_string(b"3.11.7rc2\n"),
            Some(v(3, 11, 7, 0xC, 2))
        );
    }

    /// The micro component is required: a bare `"3.13"` is a truncated/false hit and
    /// must be skipped so a real `"3.13.1"` further along still wins. Without the
    /// micro guard the scan would lock onto the first `"3.<minor>"` it sees.
    #[test]
    fn scan_skips_a_version_without_a_micro_and_keeps_looking() {
        assert_eq!(scan_for_version_string(b"3.13 then 3.13.4 "), Some(v(3, 13, 4, 0xF, 0)));
        // A lone minor-only string yields nothing.
        assert_eq!(scan_for_version_string(b"python 3.13\x00"), None);
    }

    /// A version glued to the wrong context is rejected: a trailing digit run past the
    /// micro (`3.1.23456...`) still parses as its own micro, but a `"3."` embedded in
    /// a longer number (`13.12.0`, where the leading digit precedes `3.`) must not be
    /// mistaken for a version.
    #[test]
    fn scan_rejects_a_version_embedded_in_a_larger_number() {
        // The `3.` here is preceded by `1`, so it's part of `13.12` — not a version.
        assert_eq!(scan_for_version_string(b"lib13.12.0"), None);
    }

    /// Trailing context must be a terminator (NUL, space, paren, quote, newline). A
    /// version followed by an identifier char (`3.12.0abc` with no valid suffix) is
    /// not accepted at that position.
    #[test]
    fn scan_requires_a_terminator_after_the_version() {
        assert_eq!(scan_for_version_string(b"3.12.0\""), Some(v(3, 12, 0, 0xF, 0)));
        assert_eq!(scan_for_version_string(b"3.12.0("), Some(v(3, 12, 0, 0xF, 0)));
        // 'z' is neither a release suffix nor a terminator → not a match here.
        assert_eq!(scan_for_version_string(b"3.12.0z"), None);
    }

    /// No `3.x.y` anywhere → no version, not a panic on the short-buffer boundary.
    #[test]
    fn scan_returns_none_when_absent() {
        assert_eq!(scan_for_version_string(b""), None);
        assert_eq!(scan_for_version_string(b"no version here"), None);
        assert_eq!(scan_for_version_string(b"3."), None);
    }

    /// `parse_micro_digits` accumulates into a `u8` with checked arithmetic, so a
    /// run that overflows 255 returns `None` rather than wrapping to a bogus value.
    #[test]
    fn parse_micro_digits_reads_a_run_and_guards_overflow() {
        assert_eq!(parse_micro_digits(b"07x", 0), Some((7, 2)));
        assert_eq!(parse_micro_digits(b"abc", 0), None); // no digits at start
        assert_eq!(parse_micro_digits(b"255", 0), Some((255, 3)));
        assert_eq!(parse_micro_digits(b"256", 0), None); // overflows u8
    }
}
