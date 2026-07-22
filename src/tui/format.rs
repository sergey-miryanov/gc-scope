//! Leaf value→string formatters shared across the TUI section builders. The thresholds and
//! units are the contract the section tests pin exactly, so they live in one place.
use std::time::Duration;

pub(super) fn fmt_val(val: u64) -> String {
    if val > 0xFFFF_FFFF {
        format!("{:#x}", val)
    } else {
        val.to_string()
    }
}

pub(super) fn fmt_thousands(val: u64) -> String {
    let s = val.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.char_indices() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(c);
    }
    out
}

pub(super) fn fmt_bytes(val: u64) -> String {
    if val >= 1000 * 1000 {
        format!("{:.1}M", val as f64 / (1000.0 * 1000.0))
    } else if val >= 1000 {
        format!("{:.1}K", val as f64 / 1000.0)
    } else {
        format!("{}", val)
    }
}

pub(super) fn fmt_duration(dur: f64) -> String {
    if dur < 1.0 {
        format!("{:.3}ms", dur * 1000.0)
    } else {
        format!("{:.3}s", dur)
    }
}

pub(super) fn fmt_duration_ns(d: Duration) -> String {
    let ns = d.as_nanos() as f64;
    if ns >= 1_000_000.0 {
        format!("{:.3}ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1}\u{00b5}s", ns / 1_000.0)
    } else {
        format!("{:.0}ns", ns)
    }
}

pub(super) fn fmt_rate(rate: f64) -> String {
    if rate >= 10.0 {
        format!("{:.1}/s", rate)
    } else if rate >= 0.01 {
        format!("{:.2}/s", rate)
    } else {
        "0.0/s".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Leaf input→string logic every section depends on; the thresholds and units are
    // the contract, so pin them exactly.

    #[test]
    fn fmt_val_switches_to_hex_only_above_u32_max() {
        assert_eq!(fmt_val(0), "0");
        assert_eq!(fmt_val(255), "255");
        // Exactly u32::MAX still renders decimal (the guard is strictly greater).
        assert_eq!(fmt_val(0xFFFF_FFFF), "4294967295");
        assert_eq!(fmt_val(0x1_0000_0000), "0x100000000");
    }

    #[test]
    fn fmt_thousands_groups_from_the_right() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(123), "123");
        assert_eq!(fmt_thousands(1234), "1_234");
        assert_eq!(fmt_thousands(1_234_567), "1_234_567");
    }

    #[test]
    fn fmt_bytes_scales_at_the_k_and_m_thresholds() {
        assert_eq!(fmt_bytes(0), "0");
        assert_eq!(fmt_bytes(999), "999");
        assert_eq!(fmt_bytes(1000), "1.0K");
        assert_eq!(fmt_bytes(1500), "1.5K");
        assert_eq!(fmt_bytes(1_000_000), "1.0M");
        assert_eq!(fmt_bytes(2_500_000), "2.5M");
    }

    #[test]
    fn fmt_duration_crosses_from_ms_to_s_at_one_second() {
        assert_eq!(fmt_duration(0.0), "0.000ms");
        assert_eq!(fmt_duration(0.001), "1.000ms");
        assert_eq!(fmt_duration(0.5), "500.000ms");
        // 1.0 is NOT < 1.0, so it renders in seconds.
        assert_eq!(fmt_duration(1.0), "1.000s");
        assert_eq!(fmt_duration(2.5), "2.500s");
    }

    #[test]
    fn fmt_duration_ns_picks_ns_us_ms_by_magnitude() {
        assert_eq!(fmt_duration_ns(Duration::from_nanos(0)), "0ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(500)), "500ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(999)), "999ns");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(1000)), "1.0\u{00b5}s");
        assert_eq!(fmt_duration_ns(Duration::from_micros(2)), "2.0\u{00b5}s");
        assert_eq!(fmt_duration_ns(Duration::from_nanos(1_500_000)), "1.500ms");
        assert_eq!(fmt_duration_ns(Duration::from_millis(5)), "5.000ms");
    }

    #[test]
    fn fmt_rate_uses_one_decimal_high_two_decimals_mid_and_floors_low_to_zero() {
        assert_eq!(fmt_rate(15.0), "15.0/s");
        assert_eq!(fmt_rate(10.0), "10.0/s");
        assert_eq!(fmt_rate(9.99), "9.99/s");
        assert_eq!(fmt_rate(1.5), "1.50/s");
        assert_eq!(fmt_rate(0.01), "0.01/s");
        // Below 0.01 collapses to the sentinel rather than "0.00/s".
        assert_eq!(fmt_rate(0.009), "0.0/s");
        assert_eq!(fmt_rate(0.0), "0.0/s");
    }
}
