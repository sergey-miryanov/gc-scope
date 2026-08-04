use anyhow::{Result, bail};
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

/// Characters the version grammar can consume. Doubles as the candidate boundary in
/// `scan_for_version_string`: a run of these is as far as a literal could extend, and
/// it is ASCII, so a `&str` over it always builds.
const fn is_version_char(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'.' | b'a' | b'b' | b'c' | b'r')
}

/// Bytes that may follow an embedded `PY_VERSION` literal. Anything else means the match
/// is part of a longer token — `-` is what rejects the `v3.14.0-dirty` build tag sitting
/// a few bytes from the literal in a 3.14 image.
///
/// `+` terminates rather than rejects: CPython appends it to `PY_VERSION` after tagging
/// while `PY_VERSION_HEX` keeps the released value, so a branch checkout emits
/// `3.10.16+` and must still read as `3.10.16` (ADR 0012).
const fn is_terminator(b: u8) -> bool {
    matches!(b, 0 | b' ' | b'(' | b'\n' | b'\r' | b'\t' | b'"' | b'+')
}

/// The one version grammar: a bare fully-qualified `X.Y.Z` with an optional `aN`/`bN`/
/// `rcN` suffix and nothing else — no whitespace, no `Python ` prefix, no trailing
/// content. Callers delimit the candidate first; see `scan_for_version_string`.
///
/// A serial past `0xF` does not fit the hex and is refused rather than clamped or
/// mirrored — [ADR 0012](../../docs/adr/0012-version-detection-fails-closed.md) has the
/// reasoning.
fn parse_exact(s: &str) -> Option<PythonVersion> {
    let mut chars = s.char_indices().peekable();

    let major = parse_digits(&mut chars)?;
    if chars.next()?.1 != '.' {
        return None;
    }
    let minor = parse_digits(&mut chars)?;
    if chars.next()?.1 != '.' {
        return None;
    }
    let micro = parse_digits(&mut chars)?;

    let (release_level, serial) = match chars.peek() {
        Some(&(_, c)) if c == 'a' || c == 'b' => {
            let level = if c == 'a' { 0xA } else { 0xB };
            chars.next();
            (level, parse_digits(&mut chars)?)
        }
        Some(&(_, 'r')) => {
            chars.next();
            if chars.next()?.1 != 'c' {
                return None;
            }
            (0xC, parse_digits(&mut chars)?)
        }
        _ => (0xF, 0),
    };

    // Nothing may remain. In the scanner this is the rule that catches an in-charset but
    // ungrammatical tail (`3.12.0c`, `3.12.0rc1a`); `3.12.0z` never reaches it, since `z`
    // is outside the charset, ends the run early, and is refused by the terminator rule.
    if chars.next().is_some() || serial > 0xF {
        return None;
    }

    Some(PythonVersion {
        major,
        minor,
        micro,
        release_level,
        serial,
    })
}

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
        if let Some(ver) = scan_image_for_version(&bytes)
            && ver.major == 3
        {
            return Ok(ver);
        }
    }

    bail!("Could not detect Python version for pid {}", pid);
}

// ── Symbol resolution ───────────────────────────────────────

/// Read one already-read module image's embedded `PY_VERSION` literal.
///
/// The read-only data section first — where the literal is emitted, and away from stray
/// `"3.x"` bytes elsewhere in the image — then the whole image if that section cannot be
/// located or holds no match, so no build regresses.
///
/// This is `detect`'s second phase, exposed because `detect` short-circuits on the
/// `Py_Version` symbol from 3.11 up: without a way in here, the scan is only ever
/// exercised live on the pre-3.11 legs, which is a third of the matrix.
pub fn scan_image_for_version(bytes: &[u8]) -> Option<PythonVersion> {
    match read_only_data(bytes) {
        Some(ro) => scan_for_version_string(ro).or_else(|| scan_for_version_string(bytes)),
        None => scan_for_version_string(bytes),
    }
}

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
            return Some((base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)));
        }
    }

    for sym in elf_obj.syms.iter() {
        if elf_obj.strtab.get_at(sym.st_name) == Some(sym_name) {
            return Some((base_addr as u64).wrapping_add(sym.st_value.wrapping_sub(load_bias)));
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

    let text_vmaddr = macho.segments.iter().find_map(|seg| {
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
                    (base_addr as u64).wrapping_add(nlist.n_value.wrapping_sub(text_vmaddr)),
                );
            }
        }
    }

    None
}

/// File-offset range of the binary's read-only data section — PE `.rdata`, ELF
/// `.rodata`, Mach-O `__TEXT,__cstring` — where the `PY_VERSION` string literal is
/// emitted. `None` if the format isn't recognized or the section isn't present.
fn ro_data_range(bytes: &[u8]) -> Option<(usize, usize)> {
    match binary::classify(bytes)? {
        binary::BinaryKind::Pe => {
            let pe = pe::PE::parse(bytes).ok()?;
            let s = pe.sections.iter().find(|s| {
                s.name()
                    .map(|n| n.trim_end_matches('\0') == ".rdata")
                    .unwrap_or(false)
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

/// Locate an embedded `PY_VERSION` literal in a byte image and decode it.
///
/// `detect` reaches this whenever the `Py_Version` symbol cannot be resolved, which is
/// every attach below 3.11 — so it is the sole version source for 3.8/3.9/3.10, not a
/// rare path. It locates and delimits; `parse_exact` is the only thing that parses.
fn scan_for_version_string(bytes: &[u8]) -> Option<PythonVersion> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        // A preceding version character means this `3` sits inside something longer:
        // `lib13.12.0`, `1.3.12.0`, the `a3` of `3.13.1a3.12.0`.
        if bytes[i] != b'3' || bytes[i + 1] != b'.' || (i > 0 && is_version_char(bytes[i - 1])) {
            i += 1;
            continue;
        }

        // As far as a version could extend; ASCII by construction, so the `&str` always
        // builds. Requiring the *whole* run to parse is what rejects `3.12.0c`, where the
        // run swallows a charset character the grammar cannot use.
        let end = bytes[i..]
            .iter()
            .position(|&b| !is_version_char(b))
            .map_or(bytes.len(), |n| i + n);

        if let Ok(candidate) = std::str::from_utf8(&bytes[i..end])
            && let Some(ver) = parse_exact(candidate)
            && is_terminator(bytes.get(end).copied().unwrap_or(0))
        {
            return Some(ver);
        }

        // One byte. The anchor guard above already rejects every position inside a run,
        // so `i = end` would behave identically today; this is its backstop, and the only
        // thing preserving the property if that guard is ever narrowed.
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u8, minor: u8, micro: u8, release_level: u8, serial: u8) -> PythonVersion {
        PythonVersion {
            major,
            minor,
            micro,
            release_level,
            serial,
        }
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
    fn parse_exact_parses_the_shapes_detect_actually_sees() {
        for (s, want) in [
            ("3.15.0a8", v(3, 15, 0, 0xA, 8)),
            ("3.12.0", v(3, 12, 0, 0xF, 0)),
            ("3.11.0rc1", v(3, 11, 0, 0xC, 1)),
            ("3.15.0b1", v(3, 15, 0, 0xB, 1)),
            ("3.8.19", v(3, 8, 19, 0xF, 0)),
            // Zero-padded components are accepted, as they were before: the rule is
            // the grammar, not the canonical rendering.
            ("3.08.1", v(3, 8, 1, 0xF, 0)),
            // Digits are consumed greedily to the end of the run, up to what a u8
            // holds — the component is not capped at two digits.
            ("3.1.234", v(3, 1, 234, 0xF, 0)),
        ] {
            assert_eq!(parse_exact(s), Some(want), "should parse {s:?}");
        }
    }

    /// Strict where its predecessor was permissive: the input is a candidate already
    /// delimited out of arbitrary binary data, so anything beyond the bare grammar is a
    /// reason to refuse. The first three were *accepted* by the old `from_string`.
    #[test]
    fn parse_exact_refuses_anything_but_a_bare_fully_qualified_version() {
        for s in [
            "Python 3.11.0",                      // no prefix stripping
            "3.12.0 (tags/v3.12.0, Oct  2 2023)", // no trailing content
            "3.12",                               // micro is mandatory
            " 3.12.0",                            // no trimming
            "3.12.0 ",
            "3.12.0z",
            "3.12.0rx",
            "3.12.0r",
            "3.12.0a",
            "3.12.0.1",
            // A component followed by something that is not the next separator. The
            // scanner cannot produce these (its charset run stops at `x`), but
            // `parse_exact` is the grammar's definition and owns the rule.
            "3x.12.0",
            "3.12x.0",
            "",
            "3",
            "3.x",
            "x3.12",
            "..",
            "3.",
        ] {
            assert_eq!(parse_exact(s), None, "should reject {s:?}");
        }
    }

    #[test]
    fn parse_exact_rejects_overflowing_component() {
        // parse_digits accumulates into a u8 with checked_mul/checked_add. Without
        // those guards "3.999.0" would wrap to a plausible-looking minor and gcscope
        // would silently resolve the wrong layout.
        assert_eq!(parse_exact("3.999.0"), None);
        assert_eq!(parse_exact("3.12.999"), None);
    }

    /// The serial field is four bits wide, so a build past 0xF cannot be represented and
    /// is refused. `scan_refuses_a_serial_that_does_not_fit_the_hex` carries the why.
    #[test]
    fn parse_exact_refuses_a_serial_that_does_not_fit_the_hex() {
        assert_eq!(parse_exact("3.15.0b17"), None);
        assert_eq!(parse_exact("3.15.0a16"), None);
        assert_eq!(parse_exact("3.15.0rc99"), None);
        assert_eq!(parse_exact("3.15.0b15"), Some(v(3, 15, 0, 0xB, 15)));
    }

    /// `from_hex` decodes the release level from a nibble read out of live process
    /// memory, so it can yield a level the grammar has no spelling for — a corrupt read,
    /// or a level CPython has not defined. Rendering that as `-<hex><serial>` keeps it
    /// diagnosable; rendering it as a final release would state something false. The form
    /// is deliberately not re-parseable, since no such build can be named.
    #[test]
    fn display_marks_an_unrecognized_release_level() {
        assert_eq!(v(3, 15, 0, 0xD, 2).to_string(), "3.15.0-d2");
        assert_eq!(v(3, 15, 0, 0x0, 0).to_string(), "3.15.0-00");
        assert_eq!(parse_exact("3.15.0-d2"), None);
        // The nibble comes straight off a hex read, which is how such a value arrives.
        let decoded = PythonVersion::from_hex(0x030f00d2).expect("valid hex");
        assert_eq!(decoded.to_string(), "3.15.0-d2");
    }

    #[test]
    fn display_round_trips_parse_exact() {
        for s in ["3.15.0a8", "3.15.0b1", "3.15.0rc1", "3.12.0", "3.8.19"] {
            let parsed = parse_exact(s).expect(s);
            assert_eq!(parsed.to_string(), s);
        }
    }

    // ── binary version-string scan (the on-disk source for `detect` below 3.11) ──
    // The `Py_Version` symbol arrives in 3.11, so this is the sole version source for
    // 3.8/3.9/3.10. Byte slices exercise the whole locate-delimit-parse chain without
    // a real binary.

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
        assert_eq!(
            scan_for_version_string(b"3.13 then 3.13.4 "),
            Some(v(3, 13, 4, 0xF, 0))
        );
        // A lone minor-only string yields nothing.
        assert_eq!(scan_for_version_string(b"python 3.13\x00"), None);
    }

    /// Trailing context must be a terminator (NUL, space, paren, quote, newline). A
    /// version followed by an identifier char (`3.12.0abc` with no valid suffix) is
    /// not accepted at that position.
    #[test]
    fn scan_requires_a_terminator_after_the_version() {
        assert_eq!(
            scan_for_version_string(b"3.12.0\""),
            Some(v(3, 12, 0, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"3.12.0("),
            Some(v(3, 12, 0, 0xF, 0))
        );
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

    /// A serial past 0xF names a build the hex cannot hold. Refuse rather than invent a
    /// neighbour: clamping reports `b15`, and mirroring `patchlevel.h`'s
    /// `(level << 4) | serial` reports `b1` — a live `LAYOUTS` row, wrongly decoded.
    #[test]
    fn scan_refuses_a_serial_that_does_not_fit_the_hex() {
        assert_eq!(scan_for_version_string(b"3.15.0b17\x00"), None);
        assert_eq!(scan_for_version_string(b"3.15.0a16\x00"), None);
        assert_eq!(scan_for_version_string(b"3.15.0rc99\x00"), None);
        // The boundary itself still parses.
        assert_eq!(
            scan_for_version_string(b"3.15.0b15\x00"),
            Some(v(3, 15, 0, 0xB, 15))
        );
    }

    /// The `3` must not follow any character the grammar can consume — a digit
    /// (`lib13.12.0`), a dot (`1.3.12.0`), a suffix letter (`3.13.1a3.12.0`) — or the
    /// scan reads a version out of the middle of something else.
    #[test]
    fn scan_rejects_an_anchor_glued_to_a_longer_token() {
        for bytes in [
            b"lib13.12.0".as_slice(),
            b"1.3.12.0\x00".as_slice(),
            b"3.13.1a3.12.0\x00".as_slice(),
            b"libpython3.13.so.1.0".as_slice(),
        ] {
            let label = String::from_utf8_lossy(bytes);
            assert_eq!(
                scan_for_version_string(bytes),
                None,
                "should reject {label:?}"
            );
        }
    }

    /// A version glued to a failed candidate is still found (the C5 property). What
    /// secures this is now the anchor guard, not the one-byte advance: every position
    /// inside a run has a version-character predecessor, so `i = end` behaves identically
    /// — these cases pin the outcome, which holds under either advance.
    #[test]
    fn scan_advances_one_byte_past_a_failed_candidate() {
        assert_eq!(
            scan_for_version_string(b"3.999.0-3.13.1\x00"),
            Some(v(3, 13, 1, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"3.12.0zzz3.13.1\x00"),
            Some(v(3, 13, 1, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"3.13 then 3.13.4 "),
            Some(v(3, 13, 4, 0xF, 0))
        );
    }

    /// A branch checkout spells `PY_VERSION` as `"X.Y.Z+"` while `PY_VERSION_HEX` keeps
    /// the released value. Below 3.11 the scanner is the only source, so rejecting `+`
    /// makes such an interpreter unattachable.
    #[test]
    fn scan_accepts_the_dev_branch_plus_suffix() {
        assert_eq!(
            scan_for_version_string(b"3.10.16+\x00"),
            Some(v(3, 10, 16, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"3.15.0a8+\x00"),
            Some(v(3, 15, 0, 0xA, 8))
        );
        // The `+` ends the version; it is not part of it.
        assert_eq!(
            scan_for_version_string(b"3.10.16+extra"),
            Some(v(3, 10, 16, 0xF, 0))
        );
    }

    /// Two rules refuse two similar-looking tails, and conflating them invites a
    /// "simplification" that drops one. `c` is in the charset, so the run swallows it and
    /// `parse_exact` refuses the whole run; `z` is not, so the run ends early,
    /// `parse_exact` accepts `3.12.0`, and the terminator rule does the refusing.
    #[test]
    fn scan_refuses_in_charset_and_out_of_charset_tails_by_different_rules() {
        assert_eq!(scan_for_version_string(b"3.12.0c\x00"), None);
        assert_eq!(scan_for_version_string(b"3.12.0rc1a\x00"), None);
        assert_eq!(parse_exact("3.12.0c"), None);
        // Out-of-charset: `parse_exact` accepts what the run holds, the terminator rejects.
        assert_eq!(parse_exact("3.12.0"), Some(v(3, 12, 0, 0xF, 0)));
        assert_eq!(scan_for_version_string(b"3.12.0z"), None);
    }

    /// Every version the type can represent, rendered and scanned back. `Display` and the
    /// grammar describe one format, and a table names only a few points on it; sweeping
    /// catches a rule that is right at the examples someone thought of and wrong
    /// elsewhere — the gap that hid both the `3.15.0b17` divergence and the `+` suffix.
    #[test]
    fn every_representable_version_round_trips_through_the_scanner() {
        let mut checked = 0usize;
        for minor in 0..=20u8 {
            for micro in 0..=99u8 {
                let mut cases = vec![v(3, minor, micro, 0xF, 0)];
                for level in [0xA, 0xB, 0xC] {
                    for serial in 0..=0xFu8 {
                        cases.push(v(3, minor, micro, level, serial));
                    }
                }
                for want in cases {
                    // Embedded, not bare, so the anchor guard is exercised too. `junk`
                    // holds no version character, so it cannot mask the anchor.
                    let mut buf = b"junk\x00".to_vec();
                    buf.extend_from_slice(want.to_string().as_bytes());
                    buf.push(0);
                    assert_eq!(
                        scan_for_version_string(&buf),
                        Some(want),
                        "round-trip failed for {want}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(
            checked,
            21 * 100 * 49,
            "the sweep must cover the whole space"
        );

        // Every terminator, against a spread of shapes.
        for term in [0u8, b' ', b'(', b'\n', b'\r', b'\t', b'"', b'+'] {
            for want in [
                v(3, 8, 19, 0xF, 0),
                v(3, 15, 0, 0xA, 8),
                v(3, 13, 1, 0xC, 15),
                v(3, 10, 16, 0xF, 0),
            ] {
                let mut buf = want.to_string().into_bytes();
                buf.push(term);
                assert_eq!(
                    scan_for_version_string(&buf),
                    Some(want),
                    "terminator {term:#04x} failed for {want}"
                );
            }
        }
    }

    /// `scan_image_for_version` over adversarial bytes: it must return, and what it
    /// returns must be representable. The everywhere-it-runs half of the fuzzing story
    /// (`fuzz/` is coverage-guided but Linux-only); deterministic, so a failure reproduces.
    ///
    /// The buffers start with real image magics on purpose. Behind a valid-looking header
    /// the bytes reach goblin's header parsing, `parse_macho`'s fat-slice arithmetic and
    /// the section-range clamping — where a hang was in fact found — rather than only the
    /// string grammar, whose bounds are guarded by construction.
    #[test]
    fn image_scan_survives_adversarial_bytes() {
        // splitmix64: tiny, deterministic, and good enough to shake out structure.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };

        // Magics `binary::classify` dispatches on, plus a non-image control.
        const MAGICS: [&[u8]; 6] = [
            &[0x7f, b'E', b'L', b'F'],
            &[b'M', b'Z', 0x90, 0x00],
            &[0xfe, 0xed, 0xfa, 0xce],
            &[0xcf, 0xfa, 0xed, 0xfe],
            &[0xca, 0xfe, 0xba, 0xbe],
            &[0xde, 0xad, 0xbe, 0xef],
        ];
        // Fragments that push the generator toward the interesting grammar boundaries
        // instead of relying on random bytes to spell a version.
        const SEEDS: [&[u8]; 8] = [
            b"3.13.1\0",
            b"3.15.0b17\0",
            b"3.10.16+",
            b"3.12.0z",
            b"3.999.0-3.13.1\0",
            b"3.",
            b"3.13.1a3.12.0\0",
            b"PY_VERSION",
        ];

        for round in 0..4000u32 {
            let mut buf = MAGICS[(round as usize) % MAGICS.len()].to_vec();
            let len = 8 + (next() % 248) as usize;
            while buf.len() < len {
                let r = next();
                if r % 5 == 0 {
                    buf.extend_from_slice(SEEDS[(r >> 8) as usize % SEEDS.len()]);
                } else {
                    buf.extend_from_slice(&r.to_le_bytes());
                }
            }
            buf.truncate(len.max(4));

            // The assertion is mostly that this returns at all.
            if let Some(found) = scan_image_for_version(&buf) {
                assert_eq!(found.major, 3, "round {round}: the scan anchors on `3.`");
                assert!(
                    found.serial <= 0xF,
                    "round {round}: serial {} does not fit the hex",
                    found.serial
                );
                assert!(
                    matches!(found.release_level, 0xA | 0xB | 0xC | 0xF),
                    "round {round}: release level {:#x} is not one the grammar produces",
                    found.release_level
                );
            }
        }
    }

    /// End-of-buffer terminates a candidate as surely as a NUL does: the embedded
    /// literal can be the last thing in the section handed to the scanner.
    #[test]
    fn scan_accepts_a_version_terminated_by_end_of_buffer() {
        assert_eq!(
            scan_for_version_string(b"3.12.0"),
            Some(v(3, 12, 0, 0xF, 0))
        );
        assert_eq!(
            scan_for_version_string(b"junk\x003.15.0b1"),
            Some(v(3, 15, 0, 0xB, 1))
        );
    }
}
