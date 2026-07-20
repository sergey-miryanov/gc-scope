pub fn hex_dump(bytes: &[u8], start_addr: u64) {
    print!("{}", format_hex_dump(bytes, start_addr));
}

/// Format a hex dump of `bytes` starting at `start_addr` into a `String`.
///
/// Split out from [`hex_dump`] so the formatting logic can be exercised
/// without writing to stdout (e.g. for tests and benchmarks).
pub fn format_hex_dump(bytes: &[u8], start_addr: u64) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let addr = start_addr + (i * 16) as u64;

        let (first, second) = chunk.split_at(8.min(chunk.len()));
        let first_hex: String = first
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");
        let second_hex: String = second
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(" ");

        let hex = if second_hex.is_empty() {
            format!("{:23}", first_hex)
        } else {
            format!("{:23}  {}", first_hex, second_hex)
        };

        let ascii: String = chunk
            .iter()
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();

        out.push_str(&format!("{:#018x}  {}  {}\n", addr, hex, ascii));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical row pins the whole format at once: the `{:#018x}` address
    /// (0x + 16 hex digits), the 8|8 split with a double-space between the two
    /// hex groups, and the ASCII gutter. Getting any column width wrong here
    /// silently misaligns every dump the tool prints.
    #[test]
    fn formats_a_full_16_byte_row_exactly() {
        let out = format_hex_dump(b"Hello, World!\x00\xff\x7f", 0x1000);
        assert_eq!(
            out,
            "0x0000000000001000  48 65 6c 6c 6f 2c 20 57  6f 72 6c 64 21 00 ff 7f  Hello, World!...\n"
        );
    }

    /// The ASCII gutter shows graphic bytes and spaces verbatim and collapses
    /// everything else to `.` — NUL, high bytes, and DEL (0x7f, non-graphic) all
    /// become dots while a literal space stays a space.
    #[test]
    fn renders_only_printable_bytes_in_the_ascii_gutter() {
        let out = format_hex_dump(b"A \x00\x7f\xffz", 0);
        let ascii = out.trim_end_matches('\n').rsplit("  ").next().unwrap();
        assert_eq!(ascii, "A ...z");
    }

    /// A row shorter than 8 bytes has no second hex group; the first group is
    /// left-padded to a fixed 23-column width so a short final row still lines up
    /// under a full one. A stray second group here would mean the split logic ran
    /// on an empty slice.
    #[test]
    fn pads_a_short_row_and_emits_no_second_group() {
        let out = format_hex_dump(b"\xde\xad\xbe\xef", 0);
        assert_eq!(
            out,
            "0x0000000000000000  de ad be ef              ....\n"
        );
        assert_eq!(out.lines().count(), 1);
    }

    /// The address column advances by exactly 16 per row, so a reader can locate a
    /// byte by its printed address. An off-by-one in the `i * 16` stride would make
    /// every row after the first point at the wrong offset.
    #[test]
    fn increments_the_address_by_16_each_row() {
        let out = format_hex_dump(&[0u8; 40], 0xdead_0000);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3); // 40 bytes → 16 + 16 + 8
        assert!(lines[0].starts_with("0x00000000dead0000"), "{}", lines[0]);
        assert!(lines[1].starts_with("0x00000000dead0010"), "{}", lines[1]);
        assert!(lines[2].starts_with("0x00000000dead0020"), "{}", lines[2]);
    }

    /// Empty input produces empty output — not a stray blank line or a header —
    /// so callers can concatenate dumps without spurious gaps.
    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(format_hex_dump(&[], 0x4000), "");
    }
}
