pub mod frame;
mod format;
mod gc_view;
mod glitch;
mod layout;
pub mod pid_dialog;
mod sections;
pub mod tree;

#[cfg(test)]
mod test_support;

use anyhow::{Context, Result};

use crate::snapshot::collect::CollectRequest;
use crate::snapshot::poller::SnapshotPoller;

pub use layout::render_snapshot;

pub fn run_tui(pid: Option<u32>, rate_ms: u64, duration_secs: Option<u64>, glitch: bool) -> Result<()> {
    frame::run_tui(pid, rate_ms, duration_secs, glitch)
}

/// Attaches, polls a single snapshot, and writes one static TUI frame (plain text) to
/// `path` — the non-interactive counterpart to `run_tui`, used by `tui --output`. The
/// frame renders with the default view (entry 0, tree + hex shown, DebugOffsets hex).
pub fn run_tui_snapshot(pid: u32, path: &str) -> Result<()> {
    let mut poller = SnapshotPoller::attach_with(pid, CollectRequest::tui())?;
    let data = poller.poll()?;
    let frame = render_snapshot(&data, 0, true, true, false, false);
    std::fs::write(path, frame).with_context(|| format!("writing TUI snapshot to {path}"))?;
    Ok(())
}
