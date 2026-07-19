use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sysinfo::System;

use crate::memory::process;
use crate::remote_debugging::session::{PySession, Tier};
use crate::remote_debugging::version;

pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub cmdline: String,
    pub version: Option<String>,
    pub runtime_found: bool,
    pub offsets_known: bool,
}

pub struct FlatRow {
    pub pid: u32,
    pub name: String,
    pub prefix: String,
    pub cmdline: String,
    pub is_python: bool,
    pub version: Option<String>,
    pub runtime_found: bool,
    pub offsets_known: bool,
}

struct ProcessEntry {
    pid: u32,
    ppid: u32,
    name: String,
    cmdline: String,
    is_python: bool,
    version: Option<String>,
    runtime_found: bool,
    offsets_known: bool,
}

pub fn list_python_processes() -> Result<(Vec<ProcessInfo>, HashMap<u32, (String, u32)>)> {
    let sys = System::new_all();

    // Build PID info map for ALL processes: pid → (name, ppid)
    let mut pid_info_map: HashMap<u32, (String, u32)> = HashMap::new();
    for (_, process) in sys.processes() {
        let pid = process.pid().as_u32();
        let ppid = process.parent().map(|p| p.as_u32()).unwrap_or(0);
        let name = process.name().to_string_lossy().to_string();
        pid_info_map.insert(pid, (name, ppid));
    }

    let mut result = Vec::new();
    for (_pid, process) in sys.processes() {
        let name = process.name().to_string_lossy().to_lowercase();
        if !name.contains("python") {
            continue;
        }
        let pid = process.pid().as_u32();
        let ppid = process.parent().map(|p| p.as_u32()).unwrap_or(0);
        let name_orig = process.name().to_string_lossy().to_string();
        let cmdline = process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        result.push(ProcessInfo { pid, ppid, name: name_orig, cmdline, version: None, runtime_found: false, offsets_known: false });
    }
    result.sort_by_key(|p| p.pid);

    // Resolve each process through a single `PySession::attach` — one pass that
    // finds the runtime, detects the version, and reads the offset layout. The
    // process-wide layout cache means PIDs sharing one interpreter binary parse its
    // offsets only once (E2).
    for p in &mut result {
        match PySession::attach(p.pid) {
            Ok(session) => {
                p.runtime_found = true;
                p.offsets_known = matches!(session.tier(), Tier::Full | Tier::LayoutOnly);
                p.version = Some(session.version().to_string());
            }
            Err(_) => {
                // Attach failed. The runtime may still exist (Python present but its
                // offsets are unsupported/unreadable) — distinguish that from a
                // non-Python process, and best-effort a version string for display.
                p.runtime_found = process::find_runtime(p.pid).is_ok();
                p.version = version::detect(p.pid).ok().map(|v| v.to_string());
            }
        }
    }

    Ok((result, pid_info_map))
}

pub fn print_process_table(processes: &[ProcessInfo], no_cmdline: bool) {
    if processes.is_empty() {
        println!("No Python processes found.");
        return;
    }
    if no_cmdline {
        println!("{:<8} {:<6} {:<18} {:<4} {:<4} {:<25}", "PID", "PPID", "Name", "R", "S", "Version");
        println!("{}", "-".repeat(65));
        for p in processes {
            let ver = p.version.as_deref().unwrap_or("-");
            let rnt = if p.runtime_found { "Y" } else { "N" };
            let off = if p.offsets_known { "Y" } else { "N" };
            println!("{:<8} {:<6} {:<18} {:<4} {:<4} {:<25}", p.pid, p.ppid, p.name, rnt, off, ver);
        }
    } else {
        println!("{:<8} {:<6} {:<18} {:<4} {:<4} {:<25} {}", "PID", "PPID", "Name", "R", "S", "Version", "Command Line");
        println!("{}", "-".repeat(128));
        for p in processes {
            let ver = p.version.as_deref().unwrap_or("-");
            let rnt = if p.runtime_found { "Y" } else { "N" };
            let off = if p.offsets_known { "Y" } else { "N" };
            println!("{:<8} {:<6} {:<18} {:<4} {:<4} {:<25} {}", p.pid, p.ppid, p.name, rnt, off, ver, p.cmdline);
        }
    }
}

// ── Tree builder ──────────────────────────────────────────────────

pub fn build_flat_rows(
    processes: &[ProcessInfo],
    pid_info_map: &HashMap<u32, (String, u32)>,
) -> Vec<FlatRow> {
    let python_pids: HashSet<u32> = processes.iter().map(|p| p.pid).collect();

    let mut entries: HashMap<u32, ProcessEntry> = HashMap::new();
    for p in processes {
        entries.insert(p.pid, ProcessEntry {
            pid: p.pid,
            ppid: p.ppid,
            name: p.name.clone(),
            cmdline: p.cmdline.clone(),
            is_python: true,
            version: p.version.clone(),
            runtime_found: p.runtime_found,
            offsets_known: p.offsets_known,
        });
    }

    // Walk up the Python parent chain, then create a context entry
    // for the first non-Python ancestor.
    for p in processes {
        let mut current_ppid = p.ppid;
        while current_ppid > 4 && python_pids.contains(&current_ppid) {
            if let Some(parent) = entries.get(&current_ppid) {
                current_ppid = parent.ppid;
            } else {
                break;
            }
        }
        if current_ppid > 4 && !entries.contains_key(&current_ppid) {
            if let Some((name, ppid)) = pid_info_map.get(&current_ppid) {
                entries.insert(current_ppid, ProcessEntry {
                    pid: current_ppid,
                    ppid: *ppid,
                    name: name.clone(),
                    cmdline: String::new(),
                    is_python: false,
                    version: None,
                    runtime_found: false,
                    offsets_known: false,
                });
            }
        }
    }

    // Build parent→children map
    let mut children_by_ppid: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, entry) in &entries {
        children_by_ppid.entry(entry.ppid).or_default().push(pid);
    }
    for children in children_by_ppid.values_mut() {
        children.sort();
    }

    // Find roots: entries whose PPID is not in entries or PPID ≤ 4
    let mut roots: Vec<u32> = Vec::new();
    for (&pid, entry) in &entries {
        if entry.ppid <= 4 || !entries.contains_key(&entry.ppid) {
            roots.push(pid);
        }
    }
    roots.sort();

    // Flatten recursively
    let mut flat_rows = Vec::new();
    for (i, &root_pid) in roots.iter().enumerate() {
        flatten_node(root_pid, &entries, &children_by_ppid, &[], i == roots.len() - 1, &mut flat_rows);
    }

    flat_rows
}

fn flatten_node(
    pid: u32,
    entries: &HashMap<u32, ProcessEntry>,
    children_by_ppid: &HashMap<u32, Vec<u32>>,
    ancestor_is_last: &[bool],
    node_is_last: bool,
    flat_rows: &mut Vec<FlatRow>,
) {
    let entry = &entries[&pid];

    let mut prefix = String::new();
    for &is_last in ancestor_is_last {
        if is_last { prefix.push_str("    "); }
        else { prefix.push_str("|   "); }
    }
    prefix.push_str("+-- ");

    flat_rows.push(FlatRow {
        pid: entry.pid,
        name: entry.name.clone(),
        prefix,
        cmdline: entry.cmdline.clone(),
        is_python: entry.is_python,
        version: entry.version.clone(),
        runtime_found: entry.runtime_found,
        offsets_known: entry.offsets_known,
    });

    if let Some(children) = children_by_ppid.get(&pid) {
        for (i, &child_pid) in children.iter().enumerate() {
            let mut child_ancestor = ancestor_is_last.to_vec();
            child_ancestor.push(node_is_last);
            flatten_node(child_pid, entries, children_by_ppid, &child_ancestor, i == children.len() - 1, flat_rows);
        }
    }
}

fn prefix_depth(prefix: &str) -> usize {
    (prefix.len().saturating_sub(4)) / 4
}

fn write_row(no_cmdline: bool, row: &FlatRow) {
    let display_name = if row.is_python {
        row.version.as_deref().unwrap_or("-").to_string()
    } else {
        row.name.clone()
    };
    let indent = "  ".repeat(prefix_depth(&row.prefix));
    let full_name = format!("{}{}", indent, display_name);
    let r_char = if row.is_python && row.runtime_found { "Y" } else if row.is_python { "N" } else { "-" };
    let s_char = if row.is_python && row.offsets_known { "Y" } else if row.is_python { "N" } else { "-" };

    if no_cmdline {
        println!("{:>8}  {}  {}  {:<22}",
            row.pid, r_char, s_char, full_name);
    } else {
        println!("{:>8}  {}  {}  {:<22}    {}",
            row.pid, r_char, s_char, full_name, row.cmdline);
    }
}

pub fn print_process_tree(processes: &[ProcessInfo], pid_info_map: &HashMap<u32, (String, u32)>, no_cmdline: bool) {
    if processes.is_empty() {
        println!("No Python processes found.");
        return;
    }

    let flat_rows = build_flat_rows(processes, pid_info_map);

    if no_cmdline {
        println!("{:>8}  {}  {}  {:<22}", "PID", "R", "S", "Version/Name");
        println!("{}", "-".repeat(50));
    } else {
        println!("{:>8}  {}  {}  {:<22}    {}", "PID", "R", "S", "Version/Name", "Command Line");
        println!("{}", "-".repeat(80));
    }

    for row in &flat_rows {
        write_row(no_cmdline, row);
    }
}
