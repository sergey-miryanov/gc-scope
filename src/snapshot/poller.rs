use anyhow::Result;

use crate::remote_debugging::session::{PySession, Revalidated};
use crate::snapshot::collect::{CollectRequest, CollectedData, collect_data};

/// Single-PID snapshot producer: owns the attached [`PySession`], hands out a full
/// [`CollectedData`] per tick, and absorbs transient read failures with the monitor's
/// revalidate ladder.
///
/// This is the single-PID sibling of [`crate::monitor::MonitorContext`]. Where that owns
/// a `HashMap<u32, PySession>` and emits deduped *event deltas* into an `EventsExporter`,
/// this owns one session and returns a *full snapshot* to its caller. The `Fresh/Changed/
/// Dead` arms of [`poll`](Self::poll) deliberately mirror `MonitorContext::poll` so the
/// kinship is visible on sight. A future multi-PID snapshot mode would wrap
/// `HashMap<u32, SnapshotPoller>` exactly as `MonitorContext` wraps sessions — a mechanical
/// extension, not a rewrite.
pub struct SnapshotPoller {
    session: PySession,
    request: CollectRequest,
}

impl SnapshotPoller {
    /// Attach to `pid`, resolving its `_PyRuntime` and offsets once. Collects every layer;
    /// use [`attach_with`](Self::attach_with) to narrow to the layers a consumer renders.
    pub fn attach(pid: u32) -> Result<Self> {
        Self::attach_with(pid, CollectRequest::all())
    }

    /// Attach to `pid`, collecting only the layers named in `request` on each [`poll`](Self::poll).
    pub fn attach_with(pid: u32, request: CollectRequest) -> Result<Self> {
        Ok(Self {
            session: PySession::attach(pid)?,
            request,
        })
    }

    /// The PID this poller is currently attached to.
    pub fn pid(&self) -> u32 {
        self.session.pid()
    }

    /// Re-target to a different PID. Attaches the new PID first and swaps the held session
    /// in only on success, so a failed attach leaves the current session live — mirroring
    /// the TUI pick-pid "commit only once it fully resolves" rule.
    pub fn retarget(&mut self, pid: u32) -> Result<()> {
        self.session = PySession::attach(pid)?;
        Ok(())
    }

    /// Read → validate → extract one full snapshot.
    ///
    /// On a read failure, run the monitor's revalidate ladder:
    /// - [`Revalidated::Fresh`] — the handle/runtime addr were soft re-attached; retry the
    ///   read once (a transient blip, absorbed).
    /// - [`Revalidated::Changed`] — a different program now holds this PID; propagate.
    /// - [`Revalidated::Dead`] — the process is gone; propagate.
    pub fn poll(&mut self) -> Result<CollectedData> {
        match collect_data(&self.session, &self.request) {
            Ok(data) => Ok(data),
            Err(e) => match self.session.revalidate() {
                Revalidated::Fresh => collect_data(&self.session, &self.request),
                Revalidated::Changed => {
                    Err(e.context("target PID is now held by a different program"))
                }
                Revalidated::Dead => Err(e.context("target process is gone")),
            },
        }
    }

    /// Test seam: install a pre-built (possibly fault-armed) session without attaching, so
    /// the revalidate ladder can be driven off a terminal. Mirrors
    /// [`crate::monitor::MonitorContext::insert_session_for_test`].
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn from_session(session: PySession) -> Self {
        Self {
            session,
            request: CollectRequest::all(),
        }
    }
}
