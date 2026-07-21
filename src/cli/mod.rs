use clap::{Parser, Subcommand};

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
    /// Monitor GC activity in real-time and export to Chrome Trace
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
        /// Arguments passed to the script/module
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
    /// Live TUI diagram of Python runtime memory layout
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
    },
    /// Generate ASCII diagram of Python runtime memory layout (stdout)
    Ascii {
        #[arg(allow_hyphen_values = true)]
        pid: i32,
        /// Watch mode: continuously poll and redraw
        #[arg(short, long)]
        watch: bool,
        /// Polling interval in milliseconds (default: 1000, requires --watch)
        #[arg(short, long, default_value = "1000", requires = "watch")]
        rate: u64,
    },
}
