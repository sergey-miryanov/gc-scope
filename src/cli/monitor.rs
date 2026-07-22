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
use crate::monitor::{MonitorContext, StartupTimeoutPolicy, run_loop};

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

    ctx.close()?;
    eprintln!("Trace written to {}", opts.output);

    runner.returncode()
}
