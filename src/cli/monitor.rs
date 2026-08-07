use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::cli::monitor_options::MonitorOptions;
use crate::monitor::exporters::EventsExporter;
use crate::monitor::exporters::chrome::ChromeTraceExporter;
use crate::monitor::{MonitorContext, StartupTimeoutPolicy, run_loop, statistics, summary_json};

// ---------------------------------------------------------------------------
// ProcessRunner — abstracts attach-vs-spawn
// ---------------------------------------------------------------------------

trait ProcessRunner {
    /// Start (or attach to) the process and return its PID.
    fn start(&mut self) -> Result<u32>;
    /// Return the process exit code (waits if the process was spawned).
    fn returncode(&mut self) -> Result<i32>;
}

/// Attach to an already-running process (monitor command).
struct ExternalProcessRunner {
    pid: u32,
}

impl ProcessRunner for ExternalProcessRunner {
    fn start(&mut self) -> Result<u32> {
        Ok(self.pid)
    }
    fn returncode(&mut self) -> Result<i32> {
        Ok(0)
    }
}

/// Spawn a child Python process with I/O forwarding (run command).
struct ChildProcessRunner {
    cmd: Command,
    child: Option<Child>,
}

impl ChildProcessRunner {
    fn new(
        python: &str,
        script: Option<&str>,
        module: Option<&str>,
        script_args: &[String],
    ) -> Result<Self> {
        let mut cmd = Command::new(python);
        cmd.arg("-u");

        match (script, module) {
            (Some(s), None) => {
                cmd.arg(s);
            }
            (None, Some(m)) => {
                cmd.arg("-m").arg(m);
            }
            _ => anyhow::bail!("Must specify either --script or --module"),
        }

        cmd.args(script_args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        Ok(ChildProcessRunner { cmd, child: None })
    }
}

impl ProcessRunner for ChildProcessRunner {
    fn start(&mut self) -> Result<u32> {
        let mut child = self.cmd.spawn().context("Failed to spawn Python process")?;
        let pid = child.id();

        let stdout = child.stdout.take().context("Failed to capture stdout")?;
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                println!("{}", line);
            }
        });

        let stderr = child.stderr.take().context("Failed to capture stderr")?;
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("{}", line);
            }
        });

        self.child = Some(child);
        Ok(pid)
    }

    fn returncode(&mut self) -> Result<i32> {
        let status = self.child.as_mut().unwrap().wait()?;
        Ok(status.code().unwrap_or(-1))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Attach to a running process and monitor its GC activity.
pub fn monitor(pid: u32, opts: &MonitorOptions) -> Result<i32> {
    run_monitoring_loop(&mut ExternalProcessRunner { pid }, opts)
}

/// Run a Python script or module with GC monitoring.
pub fn run(
    python: &str,
    script: Option<&str>,
    module: Option<&str>,
    script_args: &[String],
    opts: &MonitorOptions,
) -> Result<i32> {
    // Checked before the spawn, so an operator hears about it now rather than after the run
    // they were monitoring.
    if opts.summary_json.as_deref() == Some(summary_json::STDOUT) {
        anyhow::bail!(
            "`run` forwards the target's own stdout to gcscope's, so `--summary-json -` would \
             hand its consumer the document with the program's output mixed into it. Name a \
             path, or attach with `monitor`, whose stdout is gcscope's alone."
        );
    }
    let mut runner = ChildProcessRunner::new(python, script, module, script_args)?;
    run_monitoring_loop(&mut runner, opts)
}

// ---------------------------------------------------------------------------
// Shared monitoring loop
// ---------------------------------------------------------------------------

/// Create exporter, set up ctrlc, run monitor loop, close, return exit code.
fn run_monitoring_loop(runner: &mut impl ProcessRunner, opts: &MonitorOptions) -> Result<i32> {
    let pid = runner.start()?;
    eprintln!("Monitoring PID: {}", pid);

    let mut exporter = ChromeTraceExporter::new();
    let path = PathBuf::from(&opts.output);
    exporter.open(&path)?;

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))?;

    let mut ctx = MonitorContext::new(&mut exporter);
    run_loop(&mut ctx, pid, opts.rate, &running, || {
        StartupTimeoutPolicy::new(Duration::from_secs(2))
    })?;

    // One folded summary behind both renderings, so the table and the document cannot
    // disagree about what the run did.
    let summary = ctx.summary();
    if opts.summary {
        // On stderr, beside the other run messages: `run` forwards the target's stdout to
        // ours, and this is gcscope talking, not the program being watched. That leaves
        // stdout free for the JSON.
        for line in statistics::render(&summary) {
            eprintln!("{}", line);
        }
    }

    // The trace is finished before the summary is written. `close` is what terminates the
    // JSON array, so a mistyped `--summary-json` path returning early here used to truncate
    // the trace the whole run was for — and the operator only finds out afterwards.
    ctx.close()?;
    eprintln!("Trace written to {}", opts.output);

    let written = match &opts.summary_json {
        Some(destination) => summary_json::write(destination, &summary),
        None => Ok(()),
    };

    // Reaped either way, so a failed write does not leave the child unwaited. The error still
    // decides what gcscope exits with.
    let code = runner.returncode();
    written.and(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(summary_json: Option<&str>) -> MonitorOptions {
        MonitorOptions {
            rate: 100,
            output: "gcmon_trace.json".to_string(),
            summary: false,
            summary_json: summary_json.map(str::to_string),
        }
    }

    /// `run` forwards the target's stdout to gcscope's, so a document written there arrives
    /// with the program's own output around it. Refused up front, before the spawn: an
    /// operator who asked for a machine-readable summary and got an unparseable stream would
    /// only find out after the run they were monitoring.
    #[test]
    fn run_refuses_to_share_stdout_with_the_program_it_watches() {
        let error = run(
            "python",
            Some("spin.py"),
            None,
            &[],
            &options(Some(summary_json::STDOUT)),
        )
        .expect_err("stdout is not gcscope's alone here");

        let message = format!("{error}");
        assert!(message.contains("--summary-json -"), "{message}");
        assert!(message.contains("monitor"), "{message}");
    }
}
