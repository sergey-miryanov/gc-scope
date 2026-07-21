pub mod ascii;
pub mod pid_dialog;
pub mod render;
pub mod tui_v2;

use anyhow::Result;

use crate::snapshot::collect::{
    avg_collection_time_per_gen, collections_rate_from_slots, CollectRequest,
};
use crate::snapshot::poller::SnapshotPoller;

fn fmt_duration_ns(d: std::time::Duration) -> String {
    let ns = d.as_nanos() as f64;
    if ns >= 1_000_000.0 {
        format!("{:.3}ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.1}\u{00b5}s", ns / 1_000.0)
    } else {
        format!("{:.0}ns", ns)
    }
}

pub fn run_ascii(pid: u32) -> Result<()> {
    let mut poller = SnapshotPoller::attach_with(pid, CollectRequest::diagram())?;
    let data = poller.poll()?;
    let stats = &data.interpreter.gc.generation_stats;
    let slots = &stats.slots;
    let (rate_per_gen, avg_coll_time_per_gen) = (
        collections_rate_from_slots(slots, stats.has_timestamps),
        avg_collection_time_per_gen(slots, stats.has_duration),
    );
    print!("{}", ascii::render_ascii(&data, rate_per_gen, avg_coll_time_per_gen));
    Ok(())
}

pub fn run_ascii_watch(pid: u32, rate_ms: u64) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    let mut poller = SnapshotPoller::attach_with(pid, CollectRequest::diagram())?;
    let mut out = std::io::stdout().lock();
    let start = Instant::now();

    // Hide cursor
    write!(out, "\x1b[?25l")?;
    out.flush()?;

    let mut frame: u64 = 0;
    let result = loop {
        let elapsed = start.elapsed();
        let data = match poller.poll() {
            Ok(d) => d,
            Err(e) => break Err(e),
        };

        let stats = &data.interpreter.gc.generation_stats;
        let slots = &stats.slots;
        let (rate_per_gen, avg_coll_time_per_gen) = (
            collections_rate_from_slots(slots, stats.has_timestamps),
            avg_collection_time_per_gen(slots, stats.has_duration),
        );
        let output = ascii::render_ascii(&data, rate_per_gen, avg_coll_time_per_gen);

        frame += 1;
        write!(out, "\x1b[2J\x1b[H")?;                  // clear entire screen + home
        writeln!(
            out,
            "[Frame {} @ {:.1}s]  Rate: {}ms  Collect: {}",
            frame,
            elapsed.as_secs_f64(),
            rate_ms,
            fmt_duration_ns(data.collect_duration)
        )?;
        write!(out, "{}", output)?;
        out.flush()?;

        if !running.load(Ordering::SeqCst) {
            break Ok(());
        }
        std::thread::sleep(Duration::from_millis(rate_ms));
    };

    drop(out);
    let mut out = std::io::stdout().lock();
    writeln!(out, "\x1b[?25h")?;
    out.flush()?;
    result
}

pub fn run_tui(pid: Option<u32>, rate_ms: u64, duration_secs: Option<u64>, glitch: bool) -> Result<()> {
    tui_v2::run_tui(pid, rate_ms, duration_secs, glitch)
}
