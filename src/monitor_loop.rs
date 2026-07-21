use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::memory::process;
use crate::monitor::MonitorContext;

/// Status returned from polling a single process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PollStatus {
    /// GC stats read successfully.
    Ok,
    /// Process does not exist or has no PyRuntime mapping.
    InvalidProcess,
}

/// Per-PID decision: given a PollStatus, should we keep polling this PID?
pub trait WaitPolicy {
    fn wait(&mut self, status: PollStatus) -> bool;
}

/// Continue only on Ok; stop on any failure.
#[allow(dead_code)]
pub struct NoWaitPolicy;

impl WaitPolicy for NoWaitPolicy {
    fn wait(&mut self, status: PollStatus) -> bool {
        matches!(status, PollStatus::Ok)
    }
}

/// On InvalidProcess before first Ok, retry for up to `timeout`.
/// After first Ok, stop immediately on InvalidProcess.
pub struct StartupTimeoutPolicy {
    start_time: Instant,
    timeout: Duration,
    has_seen_alive: bool,
}

impl StartupTimeoutPolicy {
    pub fn new(timeout: Duration) -> Self {
        StartupTimeoutPolicy {
            start_time: Instant::now(),
            timeout,
            has_seen_alive: false,
        }
    }
}

impl WaitPolicy for StartupTimeoutPolicy {
    fn wait(&mut self, status: PollStatus) -> bool {
        match status {
            PollStatus::Ok => {
                self.has_seen_alive = true;
                true
            }
            PollStatus::InvalidProcess => {
                if self.has_seen_alive {
                    false
                } else {
                    self.start_time.elapsed() < self.timeout
                }
            }
        }
    }
}

/// Run the main monitoring loop.
///
/// Discovers children of all alive PIDs each iteration (grandchildren included).
/// Detects vanished PIDs that were in the children list but no longer appear.
/// Uses per-PID WaitPolicy instances created by `wait_policy_factory`.
pub fn run_loop<F, P>(
    ctx: &mut MonitorContext,
    main_pid: u32,
    rate_ms: u64,
    running: &AtomicBool,
    mut wait_policy_factory: F,
) -> Result<()>
where
    F: FnMut() -> P,
    P: WaitPolicy,
{
    let mut alive_pids: Vec<u32> = vec![main_pid];
    let mut pid_policies: HashMap<u32, P> = HashMap::new();
    let mut seen_pids: HashSet<u32> = HashSet::new();

    while running.load(Ordering::SeqCst) {
        // 1. Discover children of ALL alive PIDs (grandchildren too)
        let mut current_pids_set: HashSet<u32> = HashSet::new();
        for &pid in &alive_pids {
            current_pids_set.insert(pid);
            let children = process::get_child_pids(pid);
            current_pids_set.extend(children);
        }

        // 2. Detect vanished PIDs (were in seen_pids, no longer in current)
        for &pid in &seen_pids {
            if !current_pids_set.contains(&pid) {
                ctx.mark_died(pid);
                pid_policies.remove(&pid);
                alive_pids.retain(|&p| p != pid);
            }
        }
        seen_pids = current_pids_set.clone();

        // 3. Add new PIDs
        for &pid in &current_pids_set {
            if !alive_pids.contains(&pid) {
                alive_pids.push(pid);
            }
        }

        if alive_pids.is_empty() {
            break;
        }

        // 4. Poll each PID
        let mut any_alive = false;

        for &current_pid in &alive_pids.clone() {
            let status = ctx.poll(current_pid);

            let policy = pid_policies.entry(current_pid)
                .or_insert_with(&mut wait_policy_factory);

            if policy.wait(status) {
                any_alive = true;
            } else {
                ctx.mark_died(current_pid);
                alive_pids.retain(|&p| p != current_pid);
                pid_policies.remove(&current_pid);
                seen_pids.remove(&current_pid);
            }
        }

        if !any_alive {
            break;
        }

        if !running.load(Ordering::SeqCst) {
            break;
        }

        std::thread::sleep(Duration::from_millis(rate_ms));
    }

    // 5. Mark remaining PIDs as died
    for &p in &alive_pids {
        ctx.mark_died(p);
    }

    if alive_pids.is_empty() {
        eprintln!("All processes have exited.");
    } else {
        eprintln!("Monitoring stopped. {} process(es) still alive.", alive_pids.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_wait_policy_continues_only_while_ok() {
        let mut p = NoWaitPolicy;
        assert!(p.wait(PollStatus::Ok));
        assert!(!p.wait(PollStatus::InvalidProcess));
    }

    #[test]
    fn startup_timeout_retries_invalid_until_the_first_ok() {
        // A generous window: before any successful read, an invalid process is retried
        // rather than dropped (it may just not be ready yet).
        let mut p = StartupTimeoutPolicy::new(Duration::from_secs(3600));
        assert!(p.wait(PollStatus::InvalidProcess), "retry while inside the startup window");
        // The first Ok marks the process alive and keeps polling.
        assert!(p.wait(PollStatus::Ok));
        // After a success, an invalid process gives up at once — no more grace period.
        assert!(!p.wait(PollStatus::InvalidProcess), "post-startup failure stops immediately");
    }

    #[test]
    fn startup_timeout_gives_up_once_the_window_has_elapsed() {
        // A zero-length window: `elapsed() < ZERO` is never true, so a process that has
        // never been seen alive is abandoned on the very first invalid poll.
        let mut p = StartupTimeoutPolicy::new(Duration::ZERO);
        assert!(!p.wait(PollStatus::InvalidProcess));
    }
}
