mod cli;
mod cli_monitor;
mod cli_monitor_options;
mod diagram;
mod list_pids;
mod exporters;
mod memory;
mod monitor;
mod monitor_loop;
mod remote_debugging;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

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
        Command::FindRuntime { pid, check } => {
            let pid = resolve_pid(pid);
            let addr = memory::process::find_runtime(pid)?;
            print!("PyRuntime at {:#018x}", addr);
            if check {
                match try_check_runtime(pid, addr) {
                    Ok(r) => {
                        if r.match_ok {
                            println!("  check OK ({:#018x})", r.found);
                        } else {
                            println!(
                                "  check MISMATCH: found {:#018x}, expected {:#018x}",
                                r.found, r.expected
                            );
                        }
                    }
                    Err(e) => {
                        println!("  check FAILED: {}", e);
                    }
                }
            } else {
                println!();
            }
        }
        Command::ReadRuntime { pid } => {
            let pid = resolve_pid(pid);
            let version = remote_debugging::version::detect(pid)?;
            let (addr, stored, offsets) = remote_debugging::offsets::read_offsets(pid, &version)?;
            let report = offsets.validate();
            println!("PyRuntime at {:#018x}  (version {})", addr, version);
            println!("(stored version: 0x{:08x})\n", stored);
            println!("{}", offsets);
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
        Command::ListPids { tree, no_cmdline, verify } => {
            let (processes, pid_info_map) = list_pids::list_python_processes(verify)?;
            if tree {
                list_pids::print_process_tree(&processes, &pid_info_map, no_cmdline, verify);
            } else {
                list_pids::print_process_table(&processes, no_cmdline, verify);
            }
        }
        Command::Diagram { pid, output } => {
            let pid = resolve_pid(pid);
            let path = std::path::Path::new(&output);
            diagram::run(pid, path)?;
        }
        Command::Tui { pid, rate, duration, glitch } => {
            let dur = if duration > 0 { Some(duration) } else { None };
            let pid_opt = if pid == 0 { None } else { Some(resolve_pid(pid)) };
            diagram::run_tui(pid_opt, rate, dur, glitch)?;
        }
        Command::Ascii { pid, watch, rate } => {
            let pid = resolve_pid(pid);
            if watch {
                diagram::run_ascii_watch(pid, rate)?;
            } else {
                diagram::run_ascii(pid)?;
            }
        }
    }

    Ok(())
}

/// Try to verify the runtime address by scanning for PyInterpreterState.
/// Uses `_Py_DebugOffsets` offsets read from the target process.
fn try_check_runtime(pid: u32, runtime_addr: u64) -> Result<
    remote_debugging::check_interpreter::CheckResult,
> {
    let version = remote_debugging::version::detect(pid)?;
    if version.major < 3 || version.minor < 13 {
        anyhow::bail!("check requires Python >= 3.13 (detected {})", version);
    }

    // Try the versioned code path first (works for 3.15.x where we have bindgen structs)
    if let Ok((_, _, offsets)) = remote_debugging::offsets::read_offsets(pid, &version) {
        return remote_debugging::check_interpreter::check_runtime(
            pid,
            runtime_addr,
            offsets.runtime_state_size(),
            offsets.runtime_interpreters_head(),
            offsets.interpreter_state_threads_head(),
            offsets.thread_state_interp(),
        );
    }

    // Fallback: read offsets directly from _Py_DebugOffsets in process memory.
    // The overall struct layout is:
    //   [ 0..8]  cookie
    //   [ 8..16] version
    //   [16..24] free_threaded
    //   [24..?]  runtime_state  (size + N×u64 fields, at least 3)
    //
    // The first field of every debug-offset sub-struct is its `size`.
    // We use this to skip from one sub-struct to the next.

    let buf = memory::reader::read_memory(pid, runtime_addr, 4096)?;

    fn le_u64(buf: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
    }

    let rt_size = le_u64(&buf, 24);
    let rt_interpreters_head = le_u64(&buf, 40);

    // interpreter_state starts right after runtime_state.
    // Determine runtime_state's byte count from the sub-struct header.
    // (runtime_state has at least the fields `size`, `finalizing`, `interpreters_head`,
    //  but may have more in some versions – we count by advancing past
    //  each known field + any extras.  Since every debug sub-struct starts
    //  with `size`, we can compute the boundary by walking.)
    //
    // For now, assume the known 3-field layout (24 bytes), confirmed for 3.15+.
    // If it's wrong for this version, we'll fall through with a clear error.

    let is_off: usize = 48; // runtime_state occupies bytes 24..48 for known versions
    let is_size = le_u64(&buf, is_off);
    let threads_head_off = is_off + 24; // threads_head is at offset 24 within interpreter_state
    let threads_head_offset = le_u64(&buf, threads_head_off);

    // thread_state starts after interpreter_state
    let ts_off = is_off + is_size as usize;
    if ts_off + 32 > buf.len() {
        anyhow::bail!(
            "computed thread_state offset {} exceeds buffer (layout unknown for this version)",
            ts_off
        );
    }
    let thread_interp_offset = le_u64(&buf, ts_off + 24);

    remote_debugging::check_interpreter::check_runtime(
        pid,
        runtime_addr,
        rt_size,
        rt_interpreters_head,
        threads_head_offset,
        thread_interp_offset,
    )
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
