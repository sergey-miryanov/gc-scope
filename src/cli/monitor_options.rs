use clap::Args;

/// Shared monitoring options for run and monitor commands.
#[derive(Args, Clone)]
pub struct MonitorOptions {
    /// Polling rate in milliseconds
    #[arg(short, long, default_value = "100")]
    pub rate: u64,

    /// Output trace file path
    #[arg(short = 'o', long, default_value = "gcmon_trace.json")]
    pub output: String,
}
