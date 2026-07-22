//! Rust live-smoke: spawn a real interpreter, attach with the gcscope *binary*, and assert
//! the decoded GC-stats **shape** — not merely a clean read. This is the correctness gate for
//! the attach+decode path across the CI matrix: a wrong struct offset emits a full table of
//! plausible garbage that a non-empty check waves through, so the shape is asserted instead.
//!
//! It shells out to `CARGO_BIN_EXE_gcscope` rather than calling `PySession`, so it exercises
//! the shipped CLI end-to-end (output formatting, exit codes) and gets matrix parity for
//! pre-3.13 and free-threaded builds without per-version library plumbing.
//!
//! `#[ignore]`d: it attaches to a live process (ptrace/taskport), so it runs only where CI
//! grants attach permission — `cargo test --test live_smoke -- --ignored`.
//!
//! Per-leg knobs (env): `GCSCOPE_TEST_PYTHON` selects the interpreter (see
//! `common::test_python`); `GCSCOPE_EXPECT_EXTENDED=1` requires the extended `+inc` GC columns
//! (proof the same-hex `+inc` candidate was decoded, not the clean layout it shares a hex with).

mod common;

use common::{SpawnedPython, is_free_threaded, python_version, test_python};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A hung pointer-walk on an unknown layout is a real failure mode, so bound each gcscope
/// invocation and report a timeout rather than letting CI stall (mirrors the Python driver).
const CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// Real counters stay far below this; garbage from a wrong address rarely does.
const SANE_COUNTER_MAX: i128 = 1_000_000_000_000; // 1e12

/// One decoded row of `gc-stats`. Only the columns the shape check needs are kept.
struct Row {
    generation: usize,
    entry: usize,
    collections: i128,
    collected: i128,
    uncollectable: i128,
    candidates: i128,
    heap_size: i128,
}

/// Run the gcscope binary, returning `(exit code, stdout+stderr merged)`. Bounded by
/// [`CMD_TIMEOUT`]: on timeout the child is killed and the code is reported as 124.
fn gcscope(args: &[&str]) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gcscope"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gcscope");

    // Drain both pipes on worker threads so a chatty child can't deadlock on a full pipe
    // while we poll for exit.
    let mut out = child.stdout.take().expect("stdout piped");
    let mut err = child.stderr.take().expect("stderr piped");
    let out_h = thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_h = thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + CMD_TIMEOUT;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break 124;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => break -1,
        }
    };

    let mut combined = out_h.join().unwrap_or_default();
    let e = err_h.join().unwrap_or_default();
    if !e.trim().is_empty() {
        combined.push_str(&e);
    }
    (code, combined)
}

/// `(kind, entries-per-generation)` gcscope should decode for this interpreter. Mirrors
/// `GcStatsKind` selection: one inline entry per generation through 3.14, ring buffers from
/// 3.15 — 11/3/3 on a GIL build, 1/1/1 free-threaded. `None` if the version is unknown.
fn expected_shape(
    version: Option<(u8, u8)>,
    free_threaded: bool,
) -> Option<(&'static str, [usize; 3])> {
    let v = version?;
    if v < (3, 15) {
        Some(("InlineArray", [1, 1, 1]))
    } else if free_threaded {
        Some(("RingBuffer", [1, 1, 1]))
    } else {
        Some(("RingBuffer", [11, 3, 3]))
    }
}

/// Rows of `gc-stats` output, skipping header and rule lines. Columns are fixed-width and
/// shared by the plain and extended layouts; the first nine identify a data row (the 9th, a
/// float duration, is what tells a data row from a header).
fn parse_rows(out: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 9 {
            continue;
        }
        let p = |i: usize| parts[i].parse::<i128>();
        // gen entry interp collections collected uncollectable candidates heap_size duration
        let (
            Ok(generation),
            Ok(entry),
            Ok(collections),
            Ok(collected),
            Ok(uncollectable),
            Ok(candidates),
            Ok(heap_size),
        ) = (p(0), p(1), p(3), p(4), p(5), p(6), p(7))
        else {
            continue;
        };
        if parts[8].parse::<f64>().is_err() {
            continue; // header / separator
        }
        rows.push(Row {
            generation: generation as usize,
            entry: entry as usize,
            collections,
            collected,
            uncollectable,
            candidates,
            heap_size,
        });
    }
    rows
}

/// Assert the decoded table has the right shape and plausible values. Shape is the point:
/// without it a mis-keyed decode that emits the right number of garbage rows passes as
/// readily as a correct one.
fn check_stats(rows: &[Row], kind: &str, entries: [usize; 3]) -> Result<(), String> {
    let want: usize = entries.iter().sum();
    if rows.len() != want {
        return Err(format!(
            "expected {want} {kind} rows (entries {entries:?}), decoded {}",
            rows.len()
        ));
    }

    // Every (generation, entry) pair exactly once — catches a base offset that aliases two
    // generations onto the same entry range.
    let mut got: Vec<(usize, usize)> = rows.iter().map(|r| (r.generation, r.entry)).collect();
    got.sort_unstable();
    let mut expect: Vec<(usize, usize)> = Vec::with_capacity(want);
    for (g, &n) in entries.iter().enumerate() {
        for s in 0..n {
            expect.push((g, s));
        }
    }
    expect.sort_unstable();
    if got != expect {
        return Err(format!("wrong (generation, entry) set for {kind}: {got:?}"));
    }

    for r in rows {
        for (name, v) in [
            ("collections", r.collections),
            ("collected", r.collected),
            ("uncollectable", r.uncollectable),
            ("candidates", r.candidates),
            ("heap_size", r.heap_size),
        ] {
            if !(0..=SANE_COUNTER_MAX).contains(&v) {
                return Err(format!(
                    "gen {} entry {}: implausible {name}={v} (reading the wrong address?)",
                    r.generation, r.entry
                ));
            }
        }
        // Objects freed cannot exceed objects examined. `candidates` is 0 pre-3.13 (no field).
        if r.candidates != 0 && r.collected > r.candidates {
            return Err(format!(
                "gen {} entry {}: collected={} exceeds candidates={}",
                r.generation, r.entry, r.collected, r.candidates
            ));
        }
    }

    // spin.py collects every generation before READY, so each must show progress. Zeros
    // across a whole generation mean a live-looking but wrong region.
    let mut peak = [0i128; 3];
    for (g, slot) in peak.iter_mut().enumerate() {
        let m = rows
            .iter()
            .filter(|r| r.generation == g)
            .map(|r| r.collections)
            .max()
            .unwrap_or(0);
        if m <= 0 {
            return Err(format!(
                "generation {g} shows no collections; spin.py collects all three before READY"
            ));
        }
        *slot = m;
    }

    // The pyramid. spin.py seeds 20/5/1 into generations 0/1/2 and keeps that weighting, so
    // this is deterministic — and it catches a right-shaped table carrying another
    // generation's data (e.g. gen-2's base aliasing gen 1), which the checks above cannot.
    if !(peak[0] > peak[1] && peak[1] > peak[2]) {
        return Err(format!(
            "generation collections {peak:?} are not a strict pyramid; generations may be aliased"
        ));
    }
    Ok(())
}

#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn live_smoke_attaches_and_decodes_shape() {
    let Some(python) = test_python() else {
        eprintln!("SKIP live_smoke: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let version = python_version(&python);
    let free_threaded = is_free_threaded(&python);
    let expect_extended = std::env::var("GCSCOPE_EXPECT_EXTENDED").ok().as_deref() == Some("1");

    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid().to_string();

    // read-runtime first (its own finder, not attach) — stashed so any failure below carries
    // the selected layout and geometry that produced the bad numbers.
    let (_, runtime_out) = gcscope(&["read-runtime", &pid]);
    let diag = |msg: String| format!("{msg}\n----- read-runtime (diagnostic) -----\n{runtime_out}");

    // find-runtime shares the attach path with gc-stats, so a failure here isolates *finding*.
    let (rc, find_out) = gcscope(&["find-runtime", &pid]);
    if rc != 0 {
        let (_, regions) = gcscope(&["list", &pid]);
        let mapped: Vec<&str> = regions
            .lines()
            .filter(|l| l.contains("ython"))
            .take(25)
            .collect();
        panic!(
            "{}",
            diag(format!(
                "could not locate _PyRuntime (find-runtime rc={rc})\n{find_out}\n\
                 ----- mapped python regions -----\n{}",
                mapped.join("\n")
            ))
        );
    }

    let (rc, stats_out) = gcscope(&["gc-stats", &pid]);
    if rc != 0 {
        panic!("{}", diag(format!("gc-stats exited {rc}\n{stats_out}")));
    }
    if stats_out.contains("No GC stats found.") {
        panic!("{}", diag(format!("stats decoded empty\n{stats_out}")));
    }
    if !stats_out.contains("Collections") {
        panic!("{}", diag(format!("no stats table in output\n{stats_out}")));
    }

    match expected_shape(version, free_threaded) {
        None => eprintln!("WARN: could not determine the target version; skipping the shape check"),
        Some((kind, entries)) => {
            let rows = parse_rows(&stats_out);
            if let Err(e) = check_stats(&rows, kind, entries) {
                panic!(
                    "{}",
                    diag(format!("{e}\n----- gc-stats -----\n{stats_out}"))
                );
            }
        }
    }

    // Same-hex collision build (gc-gen-3.15+inc shares 0x030f00b1 with clean 3.15.0b1): a
    // correct decode is not enough, it must go through the +inc candidate, whose extra fields
    // surface as these columns. A wrong candidate already hard-errors on the ring-size guard.
    if expect_extended && !stats_out.contains("IncrSize") {
        panic!(
            "{}",
            diag(format!(
                "expected extended GC columns (IncrSize/AliveSize); the +inc candidate was not \
                 selected — decoded through the base layout\n{stats_out}"
            ))
        );
    }
}
