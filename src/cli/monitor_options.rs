use clap::{Args, ValueEnum};

/// Which encoder writes the trace.
///
/// Lives here rather than beside the exporters because it is argument parsing: the subsystems
/// stay clap-free ([ADR 0008](../../docs/adr/0008-reader-consumer-package-layering.md)), so
/// what an operator may type is the CLI's to say and how it is encoded is
/// `monitor::exporters`'.
#[derive(ValueEnum, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TraceFormat {
    /// Chrome Trace JSON, readable by `chrome://tracing` and ui.perfetto.dev
    #[default]
    Chrome,
}

/// Shared monitoring options for run and monitor commands.
///
/// `PartialEq`/`Debug` so a test can assert that two command lines ask for the same thing
/// without enumerating the fields — a comparison written field by field stops covering the
/// next one added.
#[derive(Args, Clone, PartialEq, Debug)]
pub struct MonitorOptions {
    /// Polling rate in milliseconds
    #[arg(short, long, default_value = "100")]
    pub rate: u64,

    /// Output trace file path
    #[arg(short = 'o', long, default_value = "gcmon_trace.json")]
    pub output: String,

    /// Trace format to write to that path
    #[arg(long, value_enum, default_value_t = TraceFormat::Chrome)]
    pub format: TraceFormat,

    /// Print a per-generation summary of what was observed when the run ends
    #[arg(long)]
    pub summary: bool,

    /// Write that summary as JSON to PATH. `-` means stdout, which `monitor` accepts and
    /// `run` refuses, since `run` forwards the target's own output there
    #[arg(long, value_name = "PATH")]
    pub summary_json: Option<String>,
}
