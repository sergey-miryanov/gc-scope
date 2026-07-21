use anyhow::Result;
use clap::Parser;
use gcscope::cli::{monitor as cli_monitor, Cli, Command};
use gcscope::{diagram, list_pids, memory, remote_debugging};

fn resolve_pid(pid: i32) -> u32 {
    if pid == -1 { std::process::id() } else { pid as u32 }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::List { pid } => {
            let pid = resolve_pid(pid);
            let maps = memory::regions::list_regions(pid)?;
            println!("{:<20} {:>18} {:>4}  PATH", "ADDRESSES", "SIZE", "PERMS");
            println!("{}", "-".repeat(80));
            for m in &maps {
                memory::regions::print_region(m);
            }
        }
        Command::Read { pid, address, size } => {
            let pid = resolve_pid(pid);
            let addr = parse_address(&address)?;
            let bytes = memory::reader::read_memory(pid, addr, size)?;
            memory::dump::hex_dump(&bytes, addr);
        }
        Command::FindRuntime { pid } => {
            let pid = resolve_pid(pid);
            // Attach dispatches version-aware finding (cookie for 3.13+, symbol +
            // cross-reference heuristic for pre-3.13), so this works on every version.
            let session = remote_debugging::session::PySession::attach(pid)?;
            println!("PyRuntime at {:#018x}", session.runtime_addr());
        }
        Command::ReadRuntime { pid } => {
            let pid = resolve_pid(pid);
            let version = remote_debugging::version::detect(pid)?;
            let (addr, stored, offsets) = remote_debugging::offsets::read_offsets(pid, &version)?;
            let report = offsets.validate();
            println!("PyRuntime at {:#018x}  (version {})", addr, version);
            println!("(stored version: 0x{:08x})\n", stored);

            // Which compiled layout is actually in play, and how it was chosen. A
            // fallback silently substituting a differently-shaped struct is the one
            // failure mode that looks like a decode bug but isn't.
            let exact = remote_debugging::offsets::has_exact_layout(stored);
            println!(
                "layout selected: 0x{:08x} ({})",
                offsets.expected_version(),
                if exact {
                    "exact match".to_string()
                } else {
                    format!("FALLBACK — no compiled layout for 0x{stored:08x}")
                }
            );

            // The derived GC-stats geometry, so a size/stride mismatch is readable
            // rather than inferred.
            let table = offsets.to_offset_table(pid, addr);
            println!("\nGC stats geometry (how gcscope will decode):");
            print!("{}", table.describe_gc_geometry());
            let reported = offsets.gc_generation_stats_size();
            let expected = table.gc_stats_region_size();
            println!("  process reports  : {reported} bytes (gc.generation_stats_size)");
            if reported != 0 && expected != 0 && reported != expected {
                println!(
                    "  *** MISMATCH: process {reported} vs gcscope {expected} — \
                     gc-stats will refuse to decode this build ***"
                );
            }

            println!("\n{}", offsets);
            println!("{}", report);
        }
        Command::GcStats { pid, all } => {
            let pid = resolve_pid(pid);
            let session = remote_debugging::session::PySession::attach(pid)?;
            let stats = session.gc_stats(all)?;
            remote_debugging::gc_stats::print_stats(&stats);
        }
        Command::Monitor { pid, opts } => {
            let pid = resolve_pid(pid);
            let exit_code = cli_monitor::monitor(pid, &opts)?;
            std::process::exit(exit_code);
        }
        Command::Run { python, script, module, script_args, opts } => {
            let python = python.unwrap_or_else(|| "python".to_string());
            let exit_code = cli_monitor::run(&python, script.as_deref(), module.as_deref(),
                &script_args, &opts)?;
            std::process::exit(exit_code);
        }
        Command::ListPids { tree, no_cmdline } => {
            let (processes, pid_info_map) = list_pids::list_python_processes()?;
            if tree {
                list_pids::print_process_tree(&processes, &pid_info_map, no_cmdline);
            } else {
                list_pids::print_process_table(&processes, no_cmdline);
            }
        }
        Command::Tui { pid, rate, duration, glitch, output } => {
            if let Some(path) = output {
                // Snapshot mode: a file gets one static frame, not the interactive UI, so a
                // PID is required (no terminal to run the picker in).
                if pid == 0 {
                    anyhow::bail!("tui --output requires an explicit PID");
                }
                diagram::run_tui_snapshot(resolve_pid(pid), &path)?;
            } else {
                let dur = if duration > 0 { Some(duration) } else { None };
                let pid_opt = if pid == 0 { None } else { Some(resolve_pid(pid)) };
                diagram::run_tui(pid_opt, rate, dur, glitch)?;
            }
        }
    }

    Ok(())
}

fn parse_address(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        u64::from_str_radix(&s[2..], 16)
            .map_err(|e| anyhow::anyhow!("Invalid hex address '{}': {}", s, e))
    } else if s.starts_with(|c: char| c.is_ascii_digit()) {
        u64::from_str_radix(s, 16).or_else(|_| {
            s.parse::<u64>()
                .map_err(|e| anyhow::anyhow!("Invalid address '{}': {}", s, e))
        })
    } else {
        Err(anyhow::anyhow!("Invalid address format '{}'", s))
    }
}
