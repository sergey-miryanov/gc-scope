use anyhow::Result;
use proc_maps::{MapRange, get_process_maps};

pub fn list_regions(pid: u32) -> Result<Vec<MapRange>> {
    let maps = get_process_maps(pid as proc_maps::Pid)?;
    Ok(maps)
}

pub fn print_region(m: &MapRange) {
    let path = m.filename().and_then(|p| p.to_str());
    println!(
        "{}",
        format_region(
            m.start(),
            m.size(),
            m.is_read(),
            m.is_write(),
            m.is_exec(),
            path
        )
    );
}

/// Format one memory region as a single line: `start-end  size  perms  path`.
///
/// Split out from [`print_region`] so the formatting is testable without a live
/// `MapRange` (which `proc_maps` gives no way to construct); `print_region` just
/// unpacks the `MapRange` and forwards the primitives. A missing path renders as
/// `-`, and permissions render as an `rwx` triple with `-` for each absent bit.
pub fn format_region(
    start: usize,
    size: usize,
    is_read: bool,
    is_write: bool,
    is_exec: bool,
    path: Option<&str>,
) -> String {
    let perms = format!(
        "{}{}{}",
        if is_read { "r" } else { "-" },
        if is_write { "w" } else { "-" },
        if is_exec { "x" } else { "-" },
    );
    format!(
        "{:#018x}-{:#018x}  {:>12}  {:>4}  {}",
        start,
        start + size,
        size,
        perms,
        path.unwrap_or("-"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full line pins the column layout: an `0x`+16-digit start/end pair joined
    /// by `-`, the right-aligned size and permission columns, and the path. A width
    /// drift here misaligns every row `list-regions` prints.
    #[test]
    fn formats_a_region_line_exactly() {
        let out = format_region(0, 16, true, true, false, Some("[heap]"));
        assert_eq!(
            out,
            "0x0000000000000000-0x0000000000000010            16   rw-  [heap]"
        );
    }

    /// The end column is `start + size`, not a stored end — the region spans
    /// `[start, start+size)`. An off-by-`size` here would misreport every extent.
    #[test]
    fn end_address_is_start_plus_size() {
        let out = format_region(0x1000, 0x2500, true, false, true, None);
        assert!(
            out.starts_with("0x0000000000001000-0x0000000000003500"),
            "{out}"
        );
    }

    /// Each permission bit maps to its letter when set and `-` when clear, in
    /// r/w/x order — the same convention `/proc/pid/maps` uses.
    #[test]
    fn renders_each_permission_bit_independently() {
        let perms = |r, w, x| {
            let line = format_region(0, 0, r, w, x, None);
            // The perms column is the second-to-last whitespace-separated field.
            line.rsplit("  ").nth(1).unwrap().to_string()
        };
        assert_eq!(perms(true, true, true), "rwx");
        assert_eq!(perms(false, false, false), "---");
        assert_eq!(perms(true, false, false), "r--");
        assert_eq!(perms(false, true, false), "-w-");
        assert_eq!(perms(false, false, true), "--x");
        assert_eq!(perms(true, false, true), "r-x");
    }

    /// An anonymous mapping has no backing file; it must render as `-` rather than
    /// an empty column so the path field is never blank.
    #[test]
    fn renders_missing_path_as_dash() {
        let out = format_region(0, 4096, true, false, false, None);
        assert!(out.ends_with("  -"), "{out}");
    }
}
