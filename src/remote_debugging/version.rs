use anyhow::{bail, Result};
use goblin::{elf, mach, pe};

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

        let sym_addr = match binary::classify(&bytes) {
            Some(binary::BinaryKind::Elf) => try_py_version_elf(&bytes, *base_addr),
            Some(binary::BinaryKind::Pe) => try_py_version_pe(&bytes, *base_addr),
            Some(binary::BinaryKind::MachO) => try_py_version_macho(&bytes, *base_addr),
            None => continue,
        };

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
        if let Some(ver) = scan_for_version_string(&bytes)
            && ver.major == 3
        {
            return Ok(ver);
        }
    }

    bail!("Could not detect Python version for pid {}", pid);
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

fn try_py_version_elf(bytes: &[u8], base_addr: usize) -> Option<u64> {
    let elf_obj = elf::Elf::parse(bytes).ok()?;
    let load_bias = binary::elf_load_bias(&elf_obj)?;

    for sym in elf_obj.dynsyms.iter() {
        let name = elf_obj.dynstrtab.get_at(sym.st_name)?;
        if name == "Py_Version" {
            return Some(
                (base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)),
            );
        }
    }

    for sym in elf_obj.syms.iter() {
        let name = elf_obj.strtab.get_at(sym.st_name)?;
        if name == "Py_Version" {
            return Some(
                (base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)),
            );
        }
    }

    None
}

fn try_py_version_pe(bytes: &[u8], base_addr: usize) -> Option<u64> {
    let pe_obj = pe::PE::parse(bytes).ok()?;

    for export in &pe_obj.exports {
        if export.name == Some("Py_Version") {
            return Some((base_addr as u64).wrapping_add(export.rva as u64));
        }
    }

    None
}

fn try_py_version_macho(bytes: &[u8], base_addr: usize) -> Option<u64> {
    let macho = mach::MachO::parse(bytes, 0).ok()?;

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
            if name == "Py_Version" && !nlist.is_undefined() {
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

        // Parse minor
        let (minor, mut j) = parse_micro_digits(bytes, i + 2)?;

        // Parse .micro if present
        let micro = if bytes.get(j).copied() == Some(b'.') {
            let (m, next) = parse_micro_digits(bytes, j + 1)?;
            j = next;
            m
        } else {
            0
        };

        // Parse optional release suffix: aN, bN, rcN
        let (release_level, serial) = match bytes.get(j).copied() {
            Some(b'a') => {
                let (s, next) = parse_micro_digits(bytes, j + 1)?;
                j = next;
                (0xA, s)
            }
            Some(b'b') => {
                let (s, next) = parse_micro_digits(bytes, j + 1)?;
                j = next;
                (0xB, s)
            }
            Some(b'r') if bytes.get(j + 1).copied() == Some(b'c') => {
                let (s, next) = parse_micro_digits(bytes, j + 2)?;
                j = next;
                (0xC, s)
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
