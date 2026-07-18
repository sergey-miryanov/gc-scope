use anyhow::Result;
use proc_maps::{get_process_maps, MapRange};

pub fn list_regions(pid: u32) -> Result<Vec<MapRange>> {
    let maps = get_process_maps(pid as proc_maps::Pid)?;
    Ok(maps)
}

pub fn print_region(m: &MapRange) {
    let perms = format!(
        "{}{}{}",
        if m.is_read() { "r" } else { "-" },
        if m.is_write() { "w" } else { "-" },
        if m.is_exec() { "x" } else { "-" },
    );
    let path = m
        .filename()
        .and_then(|p| p.to_str())
        .unwrap_or("-");
    println!(
        "{:#018x}-{:#018x}  {:>12}  {:>4}  {}",
        m.start(),
        m.start() + m.size(),
        m.size(),
        perms,
        path,
    );
}
