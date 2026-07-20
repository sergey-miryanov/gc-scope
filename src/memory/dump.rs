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
