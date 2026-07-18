pub mod ascii;
pub mod collect;
pub mod pid_dialog;
pub mod render;
pub mod tui_v2;

use anyhow::Result;
use std::path::Path;

use collect::{avg_collection_time_per_gen, collections_rate_from_slots};
use crate::remote_debugging::version;

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

pub fn run(pid: u32, output: &Path) -> Result<()> {
    let ver = version::detect(pid)?;
    let data = collect::collect_data(pid, &ver)?;
    render::render_svg(&data, output)?;
    println!("Diagram saved to {}", output.display());
    Ok(())
}

pub fn run_ascii(pid: u32) -> Result<()> {
    let ver = version::detect(pid)?;
    let data = collect::collect_data(pid, &ver)?;
    let slots = &data.interpreter.gc.generation_stats.slots;
    let (rate_per_gen, avg_coll_time_per_gen) = (
        collections_rate_from_slots(slots),
        avg_collection_time_per_gen(slots),
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

    let ver = version::detect(pid)?;
    let mut out = std::io::stdout().lock();
    let start = Instant::now();

    // Hide cursor
    write!(out, "\x1b[?25l")?;
    out.flush()?;

    let mut frame: u64 = 0;
    let result = loop {
        let elapsed = start.elapsed();
        let data = match collect::collect_data(pid, &ver) {
            Ok(d) => d,
            Err(e) => break Err(e),
        };

        let slots = &data.interpreter.gc.generation_stats.slots;
        let (rate_per_gen, avg_coll_time_per_gen) = (
            collections_rate_from_slots(slots),
            avg_collection_time_per_gen(slots),
        );
        let output = ascii::render_ascii(&data, rate_per_gen, avg_coll_time_per_gen);

        frame += 1;
        write!(out, "\x1b[2J\x1b[H")?;                  // clear entire screen + home
        write!(
            out,
            "[Frame {} @ {:.1}s]  Rate: {}ms  Collect: {}\n",
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
    write!(out, "\x1b[?25h\n")?;
    out.flush()?;
    result
}

pub fn run_tui(pid: Option<u32>, rate_ms: u64, duration_secs: Option<u64>, glitch: bool) -> Result<()> {
    tui_v2::run_tui(pid, rate_ms, duration_secs, glitch)
}
