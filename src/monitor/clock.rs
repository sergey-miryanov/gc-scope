//! The machine's clocks, read the way CPython reads them.
//!
//! A gcscope trace carries readings from two sources
//! ([ADR 0022](../../docs/adr/0022-every-timestamp-names-its-clock.md)): what CPython published
//! on a Record, and what gcscope noted about itself. Until now those were two *domains* — the
//! target's counter, and nanoseconds since the run began — sharing a trace and no origin, which
//! is why nothing was allowed to hold both.
//!
//! This module collapses them into one, and the reason it can is that they were never really
//! two. The Probe times a Collection with `PyTime_PerfCounterRaw`, and CPython's perf counter is
//! the machine's monotonic clock: `CLOCK_MONOTONIC` on Linux, `mach_absolute_time` on macOS,
//! `QueryPerformanceCounter` on Windows. Every one of those is **system-wide** — gcscope reads
//! the same counter in its own process and gets a value in the same domain, exactly, with no
//! correspondence to estimate. So the Observer's clock stops being a domain of its own and
//! becomes an origin within the target's.
//!
//! **Which clock a platform's CPython uses is a fact to verify, not to infer**, and it fails
//! open exactly as the image-layout facts in
//! [ADR 0004](../../docs/adr/0004-per-platform-image-layout.md) do: a wrong choice still reads a
//! plausible number, and the trace is merely wrong by however far the two counters happen to sit
//! apart. `tests/live_smoke.rs` holds the gate — it attaches to a real interpreter and checks a
//! real Record's timestamp against [`monotonic_now_ns`] — and CI runs it on all three.
//!
//! ## Why a second clock as well
//!
//! Perfetto converts a trace into *trace time*, and its default is BOOTTIME. A domain that
//! cannot be related to it is not merely unconverted: trace-processor reports
//! `clock_sync_failure_no_path` and drops the events. So the anchor is a pair —
//! [`RunClock::boottime_ns`] beside [`RunClock::monotonic_ns`], read back to back — and it is
//! read once. The two differ by however long the machine has spent suspended, so a laptop that
//! sleeps mid-capture shifts everything after it by the nap; nothing inside the trace moves
//! relative to anything else, which is the property that matters, and the alternative is
//! re-anchoring mid-run and asking a reader to believe two disagreeing claims.
//!
//! ## Why these calls do not fail
//!
//! Every read below is documented as infallible on the platforms gcscope supports:
//! `clock_gettime` fails only on a clock id it does not know, `QueryPerformanceCounter` "does
//! not fail on Windows XP or later", and `GetTickCount64` returns no error at all. They are
//! spelled that way rather than as `Option`, because the alternative is a fallback branch that
//! no test can reach and that would quietly write the trace this module exists to prevent.

/// One reading of both clocks, taken back to back, and the origin the Observer counts from.
///
/// `monotonic_ns` is the load-bearing half: it is what turns "nanoseconds since the run began"
/// into a point in the same domain a Record's timestamps are in. `boottime_ns` only tells
/// Perfetto where that domain sits on its own timeline; without it a trace is still internally
/// exact and merely floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunClock {
    monotonic_ns: i64,
    boottime_ns: u64,
}

impl RunClock {
    /// Read both clocks now. Called once, when a run begins.
    pub fn start() -> Self {
        // Back to back and in this order, so the pair describes one instant as nearly as two
        // reads can. Whatever gap is left shifts the whole trace by it and nothing within.
        let monotonic_ns = monotonic_now_ns();
        let boottime_ns = boottime_now_ns();
        RunClock {
            monotonic_ns,
            boottime_ns,
        }
    }

    /// A fixed anchor, for tests that pin bytes. A real one is two live readings, so a trace
    /// built on one is never twice the same.
    pub fn at(monotonic_ns: i64, boottime_ns: u64) -> Self {
        RunClock {
            monotonic_ns,
            boottime_ns,
        }
    }

    /// Where the run began on the machine's monotonic clock: the origin an Observer reading is
    /// counted from, and the value the clock snapshot publishes.
    pub fn monotonic_ns(&self) -> i64 {
        self.monotonic_ns
    }

    /// The same instant on Perfetto's trace clock.
    pub fn boottime_ns(&self) -> u64 {
        self.boottime_ns
    }

    /// An Observer reading — nanoseconds since the run began — as a point on the machine's
    /// monotonic clock, which is the domain a Record's timestamps are already in.
    ///
    /// `None` for a reading that cannot be one: a negative, which is no elapsed time, and an
    /// addition that would overflow. A caller writes no timestamp rather than a wrong one
    /// ([ADR 0022](../../docs/adr/0022-every-timestamp-names-its-clock.md)).
    pub fn observer_to_monotonic(&self, observer_ns: i64) -> Option<i64> {
        if observer_ns < 0 {
            return None;
        }
        self.monotonic_ns.checked_add(observer_ns)
    }
}

/// When this process began monitoring, read once and shared.
///
/// One reading rather than one per caller, because the origin is what makes two timestamps
/// comparable: the monitor counts its Observer readings from it and the Perfetto encoder places
/// them with it, and an encoder that anchored a few milliseconds after the monitor started would
/// put every marker slightly before the run it belongs to. Whichever asks first fixes it, and
/// they are within a call of each other.
pub fn run_clock() -> RunClock {
    static RUN: std::sync::OnceLock<RunClock> = std::sync::OnceLock::new();
    *RUN.get_or_init(RunClock::start)
}

/// The machine's monotonic clock in nanoseconds — the counter CPython's perf counter reads, so
/// a value from here and a Record's `ts_start` are two points on one timeline.
pub fn monotonic_now_ns() -> i64 {
    platform::monotonic_ns()
}

/// The same instant on a clock counting from boot and through suspend: Perfetto's trace clock.
pub fn boottime_now_ns() -> u64 {
    platform::boottime_ns()
}

#[cfg(unix)]
mod platform {
    /// macOS. CPython times with `mach_absolute_time`, and Darwin documents `CLOCK_UPTIME_RAW`
    /// as that same counter already in nanoseconds — which is also why it is the *monotonic*
    /// one here and `CLOCK_MONOTONIC` is the boot one: on Darwin the names are the other way
    /// round from Linux's. `CLOCK_MONOTONIC` keeps counting while the machine sleeps;
    /// `CLOCK_UPTIME_RAW` does not.
    #[cfg(target_vendor = "apple")]
    const MONOTONIC: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(target_vendor = "apple")]
    const BOOTTIME: libc::clockid_t = libc::CLOCK_MONOTONIC;

    /// Linux, where CPython's perf counter is `clock_gettime(CLOCK_MONOTONIC)` outright.
    #[cfg(target_os = "linux")]
    const MONOTONIC: libc::clockid_t = libc::CLOCK_MONOTONIC;
    #[cfg(target_os = "linux")]
    const BOOTTIME: libc::clockid_t = libc::CLOCK_BOOTTIME;

    /// Any other Unix. gcscope is built and tested on three platforms; this keeps a fourth
    /// compiling, with the anchor degraded to a single clock — the trace stays internally exact
    /// and loses the suspend time that would have placed it correctly against other producers.
    #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "linux")))]
    const MONOTONIC: libc::clockid_t = libc::CLOCK_MONOTONIC;
    #[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "linux")))]
    const BOOTTIME: libc::clockid_t = libc::CLOCK_MONOTONIC;

    fn read(clock: libc::clockid_t) -> i128 {
        // Zeroed rather than built from a literal: on some targets `timespec` carries private
        // padding fields, and a struct literal does not compile there.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        // Fails only on a clock id the kernel does not know, and both above are constants from
        // the platform's own headers.
        unsafe { libc::clock_gettime(clock, &mut ts) };
        i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec)
    }

    pub(super) fn monotonic_ns() -> i64 {
        read(MONOTONIC) as i64
    }

    pub(super) fn boottime_ns() -> u64 {
        read(BOOTTIME).max(0) as u64
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;

    /// The performance counter in nanoseconds, the way CPython scales it.
    ///
    /// Whole seconds first and the remainder after, rather than `ticks * 1e9 / frequency`: the
    /// counter is around 7×10¹⁴ on a machine up for a week, and multiplying that by a billion
    /// overflows an `i64` in the first hour of uptime. The usual frequency is 10 MHz, where the
    /// division is exact and this is `ticks * 100`.
    pub(super) fn monotonic_ns() -> i64 {
        let mut frequency = 0i64;
        let mut ticks = 0i64;
        // Neither fails on Windows XP or later, which Microsoft documents outright.
        unsafe {
            QueryPerformanceFrequency(&mut frequency);
            QueryPerformanceCounter(&mut ticks);
        }
        if frequency <= 0 {
            return 0;
        }
        (ticks / frequency) * 1_000_000_000 + (ticks % frequency) * 1_000_000_000 / frequency
    }

    /// Milliseconds since boot, suspend included, which is what Perfetto's BOOTTIME means. Its
    /// resolution is the scheduler tick rather than the performance counter's, and that costs
    /// the whole trace one uniform shift of up to ~16 ms — not one event its place beside
    /// another.
    pub(super) fn boottime_ns() -> u64 {
        unsafe { GetTickCount64() }.saturating_mul(1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both clocks read something a timeline can hold, and both move forwards. Thin, because
    /// the claim that matters — that this is the counter CPython published — cannot be made
    /// against gcscope's own process: it needs a live interpreter and a real Record, and that
    /// test is in `tests/live_smoke.rs`.
    #[test]
    fn both_clocks_advance() {
        let first = RunClock::start();
        assert!(first.monotonic_ns() > 0, "{}", first.monotonic_ns());
        assert!(first.boottime_ns() > 0, "{}", first.boottime_ns());

        // Busy-wait rather than sleep: a sleep in a unit test is a slow test, and the counters
        // move at nanosecond resolution on every platform here.
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_millis(2) {
            std::hint::spin_loop();
        }

        let second = RunClock::start();
        assert!(
            second.monotonic_ns() > first.monotonic_ns(),
            "the monotonic clock stood still: {} then {}",
            first.monotonic_ns(),
            second.monotonic_ns()
        );
        assert!(second.boottime_ns() >= first.boottime_ns());
    }

    /// The two clocks are readings of one machine, so they cannot be eras apart. A pairing that
    /// is (say) a monotonic counter beside a wall clock would show up here as the ~55 years
    /// between the epoch and now, and nowhere else until a trace came out unreadable.
    #[test]
    fn the_pair_describes_one_machine() {
        let clock = RunClock::start();
        let monotonic = clock.monotonic_ns();
        let boottime = clock.boottime_ns() as i64;
        let apart = (boottime - monotonic).abs();
        // A year of suspended time is far more than any real gap and far less than the
        // difference a wrong clock would show.
        assert!(
            apart < 365 * 24 * 60 * 60 * 1_000_000_000i64,
            "the clocks are {apart} ns apart: monotonic {monotonic}, boottime {boottime}"
        );
    }

    /// An Observer reading is an offset into the domain the anchor names, so the run's origin
    /// maps to the origin and everything after it moves by its own elapsed time.
    #[test]
    fn an_observer_reading_becomes_a_point_on_the_machines_clock() {
        let clock = RunClock::at(5_000_000_000, 9_000_000_000);
        assert_eq!(clock.observer_to_monotonic(0), Some(5_000_000_000));
        assert_eq!(clock.observer_to_monotonic(1_500), Some(5_000_001_500));
    }

    /// A reading that is not elapsed time places nothing. `mark_process_lifecycle` already
    /// refuses a negative, and a second refusal here costs nothing: this is the arithmetic that
    /// would otherwise hand back a point *before* the run began and look like a measurement.
    #[test]
    fn a_reading_that_cannot_be_elapsed_time_maps_to_nothing() {
        let clock = RunClock::at(5_000, 9_000);
        assert_eq!(clock.observer_to_monotonic(-1), None);
        assert_eq!(clock.observer_to_monotonic(i64::MIN), None);
    }

    /// And one that would run off the end of the timeline rather than wrapping around it.
    #[test]
    fn an_addition_that_would_overflow_maps_to_nothing() {
        let clock = RunClock::at(i64::MAX - 10, 0);
        assert_eq!(clock.observer_to_monotonic(10), Some(i64::MAX));
        assert_eq!(clock.observer_to_monotonic(11), None);
    }
}
