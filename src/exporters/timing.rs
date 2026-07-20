//! Timestamp conversion for trace exporters.
//!
//! CPython records GC timestamps in nanoseconds (`PyTime_t`); the Chrome trace
//! format expects microseconds. This is the only arithmetic between a decoded
//! `GcStat` and an emitted trace event.

/// Convert a nanosecond timestamp to the microseconds a Chrome trace expects.
///
/// Integer division truncates toward zero, so sub-microsecond events collapse to
/// the enclosing microsecond rather than rounding to the nearest one. That is
/// deliberate: a GC pause shorter than 1 µs renders as a zero-width slice, which
/// is honest, whereas rounding could push an `end` before its `begin`.
pub fn ts_us(ts_ns: i64) -> i64 {
    ts_ns / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_nanoseconds_to_microseconds() {
        assert_eq!(ts_us(0), 0);
        assert_eq!(ts_us(1_000), 1);
        assert_eq!(ts_us(1_000_000), 1_000);
    }

    /// Truncation toward zero, not rounding and not flooring — a begin/end pair
    /// must never invert, which rounding at the boundary could cause.
    #[test]
    fn truncates_toward_zero() {
        assert_eq!(ts_us(1_500), 1);
        assert_eq!(ts_us(999), 0);
        assert_eq!(ts_us(-1_500), -1);
        assert_eq!(ts_us(-999), 0);
    }

    /// Monotonic-clock values are large; the conversion must not overflow at the
    /// extremes of the type CPython hands us.
    #[test]
    fn handles_the_full_i64_range() {
        assert_eq!(ts_us(i64::MAX), i64::MAX / 1000);
        assert_eq!(ts_us(i64::MIN), i64::MIN / 1000);
    }
}
