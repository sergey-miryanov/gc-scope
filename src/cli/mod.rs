use clap::{Parser, Subcommand};

pub mod gc_stats;
pub mod monitor;
pub mod monitor_options;

use crate::cli::monitor_options::MonitorOptions;

#[derive(Parser)]
#[command(name = "gcscope", about = "Process memory analysis tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List memory regions of a process
    List {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
    },
    /// Read memory from a process address
    Read {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
        address: String,
        size: usize,
    },
    /// Locate the PyRuntime struct in a remote Python process
    FindRuntime {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
    },
    /// Read and display the PyRuntime debug offsets
    ReadRuntime {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
    },
    /// Read GC stats from a remote Python process
    GcStats {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
        #[arg(short, long)]
        all: bool,
    },
    /// Monitor GC activity in real-time and export it as a trace
    Monitor {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
        #[command(flatten)]
        opts: MonitorOptions,
    },
    /// Run a Python script/module with GC monitoring
    Run {
        /// Python executable path (default: "python" from PATH)
        #[arg(short, long)]
        python: Option<String>,
        /// Script path to run
        #[arg(short, long, conflicts_with = "module")]
        script: Option<String>,
        /// Module name to run (like python -m)
        #[arg(short, long, conflicts_with = "script")]
        module: Option<String>,
        /// Arguments passed to the script/module. gcscope claims its own options first, so
        /// put them after `--` if the program takes a flag gcscope also has (`--format`,
        /// `--output`, `--rate`, `--summary`)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        script_args: Vec<String>,
        #[command(flatten)]
        opts: MonitorOptions,
    },
    /// List running Python processes
    ListPids {
        /// Show process tree with parent-child relationships
        #[arg(short, long)]
        tree: bool,
        /// Hide the Command Line column
        #[arg(short = 'C', long)]
        no_cmdline: bool,
    },
    /// Full-screen TUI of Python runtime memory layout (or `--output` to dump a frame)
    Tui {
        /// PID (0 = show interactive process list)
        #[arg(allow_hyphen_values = true, default_value = "0")]
        pid: i32,
        /// Polling interval in milliseconds (default: 1000)
        #[arg(short, long, default_value = "1000")]
        rate: u64,
        /// Auto-exit after this many seconds (0 = no limit)
        #[arg(short, long, default_value = "0")]
        duration: u64,
        /// Enable visual glitch effects
        #[arg(short, long)]
        glitch: bool,
        /// Write a single static frame (plain text) to this file and exit, instead of
        /// launching the interactive terminal UI. Requires an explicit PID.
        #[arg(short, long, value_name = "FILE")]
        output: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::monitor_options::TraceFormat;
    use clap::Parser;

    fn monitoring_options(args: &[&str]) -> MonitorOptions {
        let parsed = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        match parsed.command {
            Command::Monitor { opts, .. } | Command::Run { opts, .. } => opts,
            _ => panic!("{args:?} is not a monitoring command"),
        }
    }

    /// The flag exists so a second format has somewhere to arrive, so an operator who never
    /// types it gets the run they got before it existed. Omitting `--format` and spelling out
    /// the default are the same invocation — compared whole, so a field added later is
    /// covered without anyone remembering to add it here.
    #[test]
    fn omitting_the_format_flag_asks_for_the_same_thing_as_naming_chrome() {
        for (flagless, explicit) in [
            (
                vec!["gcscope", "monitor", "1234"],
                vec!["gcscope", "monitor", "1234", "--format", "chrome"],
            ),
            (
                vec!["gcscope", "run", "-s", "spin.py"],
                vec!["gcscope", "run", "-s", "spin.py", "--format", "chrome"],
            ),
        ] {
            let default = monitoring_options(&flagless);
            assert_eq!(default.format, TraceFormat::Chrome);
            assert_eq!(monitoring_options(&explicit), default);
        }
    }

    /// `run` forwards `script_args` to the target, but a gcscope flag is claimed by gcscope
    /// first — so a program taking its own `--format` needs the `--` separator. True of every
    /// option `MonitorOptions` owns, and `--format` is a common enough name to make it worth
    /// pinning: the separator has to keep working.
    #[test]
    fn a_scripts_own_format_flag_reaches_it_after_the_separator() {
        let parsed =
            Cli::try_parse_from(["gcscope", "run", "-s", "app.py", "--", "--format", "json"])
                .expect("`--` ends gcscope's own options");
        let Command::Run {
            opts, script_args, ..
        } = parsed.command
        else {
            panic!("expected the run subcommand");
        };

        assert_eq!(script_args, vec!["--format", "json"]);
        assert_eq!(opts.format, TraceFormat::Chrome);
    }
}
