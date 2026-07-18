pub fn hex_dump(bytes: &[u8], start_addr: u64) {
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

        println!("{:#018x}  {}  {}", addr, hex, ascii);
    }
}
