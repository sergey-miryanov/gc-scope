//! A write-only protobuf wire encoder: enough of the format to write a Perfetto trace, and no
//! more. There is no reader here and no schema — a caller names the field number and the type
//! it means, and these functions append the bytes.
//!
//! Hand-rolled rather than generated, for the reason
//! [ADR 0021](../../../docs/adr/0021-hand-rolled-perfetto-encoder.md) gives. The half of that
//! decision that makes it safe lives outside this file: everything written here is decoded
//! back by the `perfetto` package in `tests/perfetto_encoder.rs`, because a wrong varint fails
//! **open** — it produces a file that parses to a different number, not one that fails to
//! parse.
//!
//! Two rules the wire format imposes that are easy to get backwards:
//!
//! - A negative `int32` or `int64` is written **sign-extended to 64 bits**, so it always takes
//!   ten bytes. gcmon masked to 32 bits and wrote `sibling_order_rank = -1` as `0`, which
//!   parses fine and reorders the UI.
//! - A submessage is its own encoded body, written as a length-delimited field. Nothing
//!   distinguishes it on the wire from a `bytes` field, so [`put_message`] and [`put_bytes`]
//!   emit the same shape; they are separate names so a reader sees which one was meant.

/// Wire type 0: `int32`, `int64`, `uint32`, `uint64`, `bool`, and enums.
pub const WIRE_VARINT: u32 = 0;
/// Wire type 1: `fixed64`, `sfixed64`, `double`.
pub const WIRE_FIXED64: u32 = 1;
/// Wire type 2: `string`, `bytes`, embedded messages, packed repeated fields.
pub const WIRE_LENGTH_DELIMITED: u32 = 2;

/// A base-128 varint, least-significant group first, continuation bit on every byte but the
/// last. Zero is one byte, not none.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// The tag that opens a field: its number and the wire type of what follows.
pub fn put_key(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    put_varint(out, u64::from((field << 3) | wire_type));
}

/// A `uint64`/`uint32`/`bool` field, or an enum whose value is not negative.
pub fn put_uint64(out: &mut Vec<u8>, field: u32, value: u64) {
    put_key(out, field, WIRE_VARINT);
    put_varint(out, value);
}

/// An `int64` field. Negatives are sign-extended, per the wire format.
pub fn put_int64(out: &mut Vec<u8>, field: u32, value: i64) {
    put_uint64(out, field, value as u64);
}

/// An `int32` field. Negatives are sign-extended to **64** bits — the width of the varint does
/// not follow the width of the declared type, and truncating to 32 writes a large positive
/// number instead.
pub fn put_int32(out: &mut Vec<u8>, field: u32, value: i32) {
    put_int64(out, field, i64::from(value));
}

/// A `double` field: IEEE-754 bits, little-endian, always eight bytes.
pub fn put_double(out: &mut Vec<u8>, field: u32, value: f64) {
    put_key(out, field, WIRE_FIXED64);
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}

/// A length-delimited field carrying raw bytes.
pub fn put_bytes(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    put_key(out, field, WIRE_LENGTH_DELIMITED);
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// A `string` field. Rust strings are already UTF-8, which is what the wire format wants.
pub fn put_string(out: &mut Vec<u8>, field: u32, value: &str) {
    put_bytes(out, field, value.as_bytes());
}

/// A submessage, given its encoded body. An empty body is still written: a field present with
/// length zero is how "this message exists and is empty" is spelled, and omitting it says
/// something else.
pub fn put_message(out: &mut Vec<u8>, field: u32, body: &[u8]) {
    put_bytes(out, field, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(&mut out, value);
        out
    }

    /// The boundaries, where a shift-by-seven loop goes wrong: zero must be one byte rather
    /// than none, and each multiple of seven bits adds one.
    #[test]
    fn a_varint_grows_one_byte_every_seven_bits() {
        assert_eq!(varint(0), [0x00]);
        assert_eq!(varint(1), [0x01]);
        assert_eq!(varint(127), [0x7f]);
        assert_eq!(varint(128), [0x80, 0x01]);
        assert_eq!(varint(300), [0xac, 0x02]);
        assert_eq!(
            varint(u64::MAX),
            [0xff; 9].iter().chain(&[0x01]).copied().collect::<Vec<_>>()
        );
    }

    /// A field key is the number shifted three places with the wire type below it. Field 60 is
    /// `TracePacket.track_descriptor`, past the one-byte boundary, so a key that is itself a
    /// multi-byte varint is covered.
    #[test]
    fn a_field_key_carries_the_number_and_the_wire_type() {
        let mut out = Vec::new();
        put_key(&mut out, 1, WIRE_LENGTH_DELIMITED);
        assert_eq!(out, [0x0a]);

        let mut out = Vec::new();
        put_key(&mut out, 60, WIRE_LENGTH_DELIMITED);
        assert_eq!(out, [0xe2, 0x03]);
    }

    /// The bug this file's header names: a negative written as an `int32` occupies ten bytes,
    /// because the varint is sign-extended to 64 bits regardless of the declared width. Masking
    /// to 32 bits produces a valid varint holding the wrong number.
    #[test]
    fn a_negative_int32_is_sign_extended_to_ten_bytes() {
        let mut out = Vec::new();
        put_int32(&mut out, 12, -1);
        assert_eq!(
            out,
            [
                0x60, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
            ]
        );

        let mut masked = Vec::new();
        put_varint(&mut masked, u64::from((-1i32) as u32));
        assert_eq!(masked.len(), 5, "what truncating to 32 bits would write");
    }

    #[test]
    fn i64_min_and_max_survive_as_int64() {
        let mut out = Vec::new();
        put_int64(&mut out, 1, i64::MIN);
        assert_eq!(out.len(), 11, "key plus a ten-byte varint");

        let mut out = Vec::new();
        put_int64(&mut out, 1, i64::MAX);
        assert_eq!(
            out,
            [0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]
        );
    }

    #[test]
    fn a_double_is_eight_little_endian_bytes() {
        let mut out = Vec::new();
        put_double(&mut out, 44, 1.0);
        assert_eq!(
            out,
            [0xe1, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]
        );
    }

    #[test]
    fn a_string_is_its_length_then_its_utf8() {
        let mut out = Vec::new();
        put_string(&mut out, 2, "GC");
        assert_eq!(out, [0x12, 0x02, b'G', b'C']);

        // Length is in bytes, not characters, so a multi-byte one must not read as one.
        let mut out = Vec::new();
        put_string(&mut out, 2, "é");
        assert_eq!(out, [0x12, 0x02, 0xc3, 0xa9]);
    }

    /// An empty submessage is a field with length zero, not an absent field. It is how a
    /// descriptor says "this process has no attributes I know" while still declaring one.
    #[test]
    fn an_empty_message_is_written_rather_than_skipped() {
        let mut out = Vec::new();
        put_message(&mut out, 3, &[]);
        assert_eq!(out, [0x1a, 0x00]);
    }

    /// Nesting is length-prefixing: the body is encoded first, then written whole.
    #[test]
    fn a_submessage_carries_its_bodys_length() {
        let mut body = Vec::new();
        put_uint64(&mut body, 1, 4242);

        let mut out = Vec::new();
        put_message(&mut out, 3, &body);
        assert_eq!(out, [0x1a, 0x03, 0x08, 0x92, 0x21]);
    }
}
