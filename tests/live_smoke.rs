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
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
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

/// Whether this interpreter publishes the pause timestamps a span needs, and so which tier
/// `monitor` should produce. Read from the target's own version, and it tracks
/// [`expected_shape`]: the builds that brought the timestamps brought the ring. `None` if the
/// version is unknown.
///
/// The monitor compares no version; it reads the fields off the Entry layout. This is the
/// outside view of that decision, and the only place a version belongs.
///
/// Split at the minor like [`expected_shape`], though the ring landed in 3.15.0a8: the offset
/// registry refuses an earlier 3.15 alpha before either check runs.
fn expects_spans(version: Option<(u8, u8)>) -> Option<bool> {
    Some(version? >= (3, 15))
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

/// The monitoring counterpart of the shape check above: `gcscope run` against a real
/// interpreter writes the tier that interpreter's Entry layout supports, and writes
/// *something* either way. A build below the ring builds used to produce an empty trace
/// against a process collecting constantly, and the fix must not swap that for a trace
/// reporting pause figures it does not have.
#[test]
#[ignore = "spawns and attaches to a live interpreter; needs ptrace/taskport — run with --ignored"]
fn live_monitor_writes_the_tier_the_build_supports() {
    let Some(python) = test_python() else {
        eprintln!("SKIP live_monitor: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let Some(expect_spans) = expects_spans(python_version(&python)) else {
        eprintln!("WARN: could not determine the target version; skipping the tier check");
        return;
    };

    let spin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("spin.py");
    let trace: PathBuf =
        std::env::temp_dir().join(format!("gcscope_live_tier_{}.json", std::process::id()));

    // `run` spawns the interpreter and returns when it exits, so the fixture's own lifetime
    // bounds the capture. Four seconds at a 50 ms poll is dozens of gen-0 Collections.
    let (rc, out) = gcscope(&[
        "run",
        "-p",
        &python.to_string_lossy(),
        "-s",
        &spin.to_string_lossy(),
        "-o",
        &trace.to_string_lossy(),
        "-r",
        "50",
        "4",
    ]);
    assert_eq!(rc, 0, "gcscope run exited {rc}\n{out}");

    let written = std::fs::read_to_string(&trace)
        .unwrap_or_else(|e| panic!("no trace at {}: {e}\n{out}", trace.display()));
    std::fs::remove_file(&trace).ok();
    let count = |needle: &str| written.matches(needle).count();
    let head = &written.chars().take(2_000).collect::<String>();

    // Whatever the tier, the trace must contain GC activity: an interpreter running spin.py
    // collects continuously.
    assert!(
        count(r#""ph":"C""#) > 0,
        "no GC activity in the trace\n{head}"
    );

    if expect_spans {
        assert!(
            count(r#""ph":"B""#) > 0 && written.contains("GC Pause"),
            "this build publishes pause timestamps, so its Collections are spans\n{head}"
        );
        return;
    }

    assert_eq!(
        count(r#""ph":"B""#),
        0,
        "this build publishes no timestamps, so it has no pause to draw\n{head}"
    );
    assert!(
        !written.contains("duration"),
        "a pause figure this build never published belongs absent, not at zero\n{head}"
    );
    assert!(
        written.contains(r#""collections""#),
        "the cumulative count is what this tier reports\n{head}"
    );

    // The counts read as a rate only when spread over the timeline; samples stamped alike
    // draw one point for the whole run.
    let stamps: HashSet<&str> = written
        .lines()
        .filter(|l| l.contains(r#""ph":"C""#))
        .filter_map(|l| l.split(r#""ts":"#).nth(1))
        .filter_map(|rest| rest.split(',').next())
        .collect();
    assert!(
        stamps.len() > 1,
        "counter samples share one timestamp, so they show no rate: {stamps:?}\n{head}"
    );
}

/// `--summary` prints what the run read, per interpreter per generation, in the tier the
/// build's Entry layout sits in.
///
/// Worth a live leg rather than only the poll seam: the summary is folded from accumulators
/// that the eviction path touches when the target exits, and a unit test driving the seam
/// never reaches that path. The second half asserts that adding no flag prints no table.
#[test]
#[ignore = "spawns and attaches to a live interpreter; needs ptrace/taskport — run with --ignored"]
fn live_monitor_summarizes_every_generation_in_the_builds_tier() {
    let Some(python) = test_python() else {
        eprintln!("SKIP live_monitor_summary: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let Some(expect_spans) = expects_spans(python_version(&python)) else {
        eprintln!("WARN: could not determine the target version; skipping the summary check");
        return;
    };

    let spin = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("spin.py");
    let trace: PathBuf =
        std::env::temp_dir().join(format!("gcscope_live_summary_{}.json", std::process::id()));

    let run = |extra: &[&str]| -> String {
        let mut args = vec![
            "run",
            "-p",
            &python.to_str().unwrap(),
            "-s",
            &spin.to_str().unwrap(),
            "-o",
            &trace.to_str().unwrap(),
            "-r",
            "50",
        ];
        args.extend_from_slice(extra);
        args.push("4");
        let (rc, out) = gcscope(&args);
        std::fs::remove_file(&trace).ok();
        assert_eq!(rc, 0, "gcscope run exited {rc}\n{out}");
        out
    };

    let out = run(&["--summary"]);
    let start = out
        .lines()
        .position(|l| l.starts_with("python ") && l.contains(", interpreter "))
        .unwrap_or_else(|| panic!("no summary block in the output\n{out}"));
    let block: Vec<&str> = out.lines().skip(start).collect();

    let header = block
        .iter()
        .find(|l| l.contains("collections"))
        .unwrap_or_else(|| panic!("the summary block has no header\n{out}"));

    // A row starts with its generation number, which is what tells it from the header, the
    // rules and the run's own log lines.
    let rows: Vec<Vec<&str>> = block
        .iter()
        .map(|l| l.split_whitespace().collect::<Vec<&str>>())
        .filter(|cols| cols.len() >= 4 && cols[0].parse::<u32>().is_ok())
        .collect();
    assert_eq!(
        rows.iter().map(|c| c[0]).collect::<Vec<&str>>(),
        ["0", "1", "2"],
        "every generation gets a row\n{out}"
    );

    for row in &rows {
        let collections: i64 = row[1]
            .parse()
            .unwrap_or_else(|e| panic!("collections {:?}: {e}\n{out}", row[1]));
        assert!(
            collections > 0 && collections < SANE_COUNTER_MAX as i64,
            "implausible collection count in {row:?}\n{out}"
        );
    }

    // Coverage rides both tiers: it is what says whether the counts beside it have a
    // distribution behind them.
    assert!(
        header.contains("coverage"),
        "every tier reports how much of what it counted it watched\n{out}"
    );

    if expect_spans {
        assert!(
            header.contains("pause total") && header.contains("records"),
            "this build publishes pause timestamps, so the summary reports them\n{out}"
        );
        // The last two columns are a scaled figure and its unit, so a row splits into ten.
        assert!(
            rows.iter().all(|r| r.len() == 10),
            "a timed row carries counts, records, coverage and both pause figures\n{out}"
        );
        // Every Collection in the span is either read or reconstructed, so Coverage is a
        // share. A build that lost none reports 1.000, and spin.py at this rate rarely does.
        for row in &rows {
            let coverage: f64 = row[5]
                .parse()
                .unwrap_or_else(|e| panic!("coverage {:?}: {e}\n{out}", row[5]));
            assert!(
                (0.0..=1.0).contains(&coverage),
                "implausible coverage in {row:?}\n{out}"
            );
        }
    } else {
        assert!(
            !header.contains("pause"),
            "a pause figure this build never published belongs absent, not at zero\n{out}"
        );
        assert!(
            rows.iter().all(|r| r.len() == 5),
            "an untimed row carries the counts and their coverage\n{out}"
        );
        // Nothing this build publishes describes a single Collection, so the counts stand
        // alone (ADR 0017).
        assert!(
            rows.iter().all(|r| r[4] == "0.000"),
            "this tier covers none of what it counts\n{out}"
        );
    }

    // The flag is the only thing that puts the table on the run's output.
    let quiet = run(&[]);
    assert!(
        !quiet.lines().any(|l| l.starts_with("python ")),
        "the summary printed without being asked for\n{quiet}"
    );
}
