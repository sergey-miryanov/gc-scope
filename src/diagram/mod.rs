pub mod pid_dialog;
pub mod render;
pub mod tui_v2;

use anyhow::{Context, Result};

use crate::snapshot::collect::CollectRequest;
use crate::snapshot::poller::SnapshotPoller;

pub fn run_tui(pid: Option<u32>, rate_ms: u64, duration_secs: Option<u64>, glitch: bool) -> Result<()> {
    tui_v2::run_tui(pid, rate_ms, duration_secs, glitch)
}

/// Attaches, polls a single snapshot, and writes one static TUI frame (plain text) to
/// `path` — the non-interactive counterpart to `run_tui`, used by `tui --output`. The
/// frame renders with the default view (slot 0, tree + hex shown, DebugOffsets hex).
pub fn run_tui_snapshot(pid: u32, path: &str) -> Result<()> {
    let mut poller = SnapshotPoller::attach_with(pid, CollectRequest::diagram())?;
    let data = poller.poll()?;
    let frame = tui_v2::render_snapshot(&data, 0, true, true, false);
    std::fs::write(path, frame).with_context(|| format!("writing TUI snapshot to {path}"))?;
    Ok(())
}
