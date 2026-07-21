use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use super::timing::ts_us;
use super::{EventsExporter, ProcessLifecycle};
use crate::remote_debugging::gc_stats::GcStat;

fn write_event(f: &mut File, first: &mut bool, json: &str) -> std::io::Result<()> {
    if *first {
        *first = false;
        write!(f, "[\n{}", json)
    } else {
        write!(f, ",\n{}", json)
    }
}

fn write_process_meta(f: &mut File, first: &mut bool, pid: u32) -> std::io::Result<()> {
    write_event(f, first, &format!(
        r#"{{"ph":"M","pid":{},"name":"process_name","args":{{"name":"python {}"}}}}"#,
        pid, pid
    ))
}

fn write_thread_meta(f: &mut File, first: &mut bool, pid: u32, tid: i64) -> std::io::Result<()> {
    write_event(f, first, &format!(
        r#"{{"ph":"M","pid":{},"tid":{},"name":"thread_name","args":{{"name":"{}:{}"}}}}"#,
        pid, tid, pid, tid
    ))
}

#[allow(clippy::too_many_arguments)]
fn write_begin(
    f: &mut File, first: &mut bool, pid: u32, tid: i64, ts: i64,
    name: &str, cat: &str, args_json: &str,
) -> std::io::Result<()> {
    write_event(f, first, &format!(
        r#"{{"ph":"B","pid":{},"tid":{},"ts":{},"name":"{}","cat":"{}","args":{{{}}}}}"#,
        pid, tid, ts, name, cat, args_json
    ))
}

fn write_end(f: &mut File, first: &mut bool, pid: u32, tid: i64, ts: i64, name: &str, cat: &str) -> std::io::Result<()> {
    write_event(f, first, &format!(
        r#"{{"ph":"E","pid":{},"tid":{},"ts":{},"name":"{}","cat":"{}"}}"#,
        pid, tid, ts, name, cat
    ))
}

#[allow(clippy::too_many_arguments)]
fn write_sub_step(
    f: &mut File, first: &mut bool, pid: u32, tid: i64,
    name: &str, cat: &str, ts_start: i64, ts_stop: i64,
    args_json: &str,
) -> std::io::Result<()> {
    if ts_stop <= ts_start {
        return Ok(());
    }
    write_begin(f, first, pid, tid, ts_us(ts_start), name, cat, args_json)?;
    write_end(f, first, pid, tid, ts_us(ts_stop), name, cat)
}

/// How a GC sub-step's start timestamp is found.
enum Start {
    /// An explicit start field; the phase emits only if both this and `stop` are present.
    Explicit(&'static str),
    /// No own start field — begin where a previous phase ended: the first present candidate
    /// (else the pause start). Emits whenever `stop` is present. Mirrors how CPython's later
    /// GC phases chain onto the preceding one.
    Chained(&'static [&'static str]),
}

/// One intra-pause GC sub-step, keyed entirely by layout field names so a build's presence or
/// absence of a phase falls out of whether its fields exist.
struct Phase {
    label: &'static str,
    cat: &'static str,
    start: Start,
    stop: &'static str,
    /// Extra JSON args beyond the always-present `generation`/`iid`: `(json_key, field_name)`.
    args: &'static [(&'static str, &'static str)],
}

/// The sub-steps in emission order. Adding a well-behaved phase (own start+stop) is a data-only
/// change; the irregular chained-start phases are expressed as [`Start::Chained`].
static PHASES: &[Phase] = &[
    Phase { label: "Mark Alive", cat: "gc.mark.alive",
        start: Start::Explicit("ts_mark_alive_start"), stop: "ts_mark_alive_stop",
        args: &[("alive_size", "alive_size")] },
    Phase { label: "Fill increment", cat: "gc.increment",
        start: Start::Explicit("ts_fill_increment_start"), stop: "ts_fill_increment_stop",
        args: &[("increment_size", "increment_size")] },
    Phase { label: "Deduce Unreachable", cat: "gc.deduce",
        start: Start::Explicit("ts_deduce_unreachable_start"), stop: "ts_deduce_unreachable_stop",
        args: &[("candidates", "candidates")] },
    Phase { label: "Handle Weakrefs Callbacks", cat: "gc.weakrefs",
        start: Start::Explicit("ts_handle_weakref_callbacks_start"), stop: "ts_handle_weakref_callbacks_stop",
        args: &[] },
    Phase { label: "Finalize Garbage", cat: "gc.finalize",
        start: Start::Chained(&["ts_handle_weakref_callbacks_stop", "ts_deduce_unreachable_stop"]),
        stop: "ts_finalize_garbage_stop",
        args: &[("finalized_garbage_count", "finalized_garbage_count")] },
    Phase { label: "Handle Resurrected", cat: "gc.resurrect",
        start: Start::Chained(&["ts_finalize_garbage_stop"]), stop: "ts_handle_resurrected_stop",
        args: &[] },
    Phase { label: "Clear Weakrefs", cat: "gc.clear_weakrefs",
        start: Start::Chained(&["ts_handle_resurrected_stop"]), stop: "ts_clear_weakrefs_stop",
        args: &[("clear_weakrefs_count", "clear_weakrefs_count")] },
    Phase { label: "Delete Garbage", cat: "gc.delete",
        start: Start::Explicit("ts_delete_garbage_start"), stop: "ts_delete_garbage_stop",
        args: &[("deleted_garbage_count", "deleted_garbage_count")] },
];

fn write_gc_stat_events(
    f: &mut File, first: &mut bool, pid: u32, s: &GcStat,
) -> std::io::Result<()> {
    let tid = s.interpreter_id;
    let ts_s = s.ts_start();
    let ts_e = s.ts_stop();
    let g = s.generation;

    let pause_name = format!("GC Pause (gen={})", g);
    let pause_cat = format!("gc.pause(gen={})", g);

    // Helper to build JSON args string
    let a = |pairs: &[(&str, String)]| -> String {
        pairs.iter().map(|(k, v)| format!(r#""{}":{}"#, k, v)).collect::<Vec<_>>().join(",")
    };

    // Begin GC Pause
    write_begin(f, first, pid, tid, ts_us(ts_s), &pause_name, &pause_cat,
        &a(&[("generation", g.to_string()),
             ("iid", s.interpreter_id.to_string()),
             ("collections", s.collections().to_string()),
             ("heap_size", s.heap_size().to_string()),
             ("collected", s.collected().to_string()),
             ("uncollectable", s.uncollectable().to_string()),
             ("candidates", s.candidates().to_string())]))?;

    // Intra-pause sub-steps, driven by the phase table. A phase whose fields this build lacks
    // resolves to `None` and is skipped; a zero-width span is dropped by `write_sub_step`.
    for ph in PHASES {
        let stop = match s.get(ph.stop) {
            Some(v) => v,
            None => continue,
        };
        let start = match &ph.start {
            Start::Explicit(field) => match s.get(field) {
                Some(v) => v,
                None => continue,
            },
            Start::Chained(cands) => cands.iter().find_map(|&c| s.get(c)).unwrap_or(ts_s),
        };
        let mut arg_pairs = vec![
            ("generation", g.to_string()),
            ("iid", s.interpreter_id.to_string()),
        ];
        for &(key, field) in ph.args {
            arg_pairs.push((key, s.get(field).unwrap_or(0).to_string()));
        }
        write_sub_step(f, first, pid, tid,
            &format!("{} (gen={})", ph.label, g),
            &format!("{}(gen={})", ph.cat, g),
            start, stop, &a(&arg_pairs))?;
    }

    // End GC Pause
    write_end(f, first, pid, tid, ts_us(ts_e), &pause_name, &pause_cat)?;

    // Counter event for generation metrics
    let other = if s.uncollectable() > 0 { format!(r#","uncollectable":{}"#, s.uncollectable()) } else { String::new() };
    write_event(f, first, &format!(
        r#"{{"name":"G{}","ph":"C","ts":{},"pid":{},"tid":{},"args":{{"collected":{},"candidates":{},"duration":{}{}}}}}"#,
        g, ts_us(ts_s), pid, tid, s.collected(), s.candidates(), s.duration(), other
    ))?;

    // Counter event for heap_size (single arg → encoder sets name to "")
    write_event(f, first, &format!(
        r#"{{"name":"","ph":"C","ts":{},"pid":{},"tid":{},"args":{{"heap_size":{}}}}}"#,
        ts_us(ts_s), pid, tid, s.heap_size()
    ))?;

    Ok(())
}

/// `Default` is derived rather than just `new()`-provided because the split to a
/// library makes this a public constructor, and clippy's `new_without_default`
/// applies to public API. All four fields are already `Default`, so the derive is
/// exactly what `new()` did.
#[derive(Default)]
pub struct ChromeTraceExporter {
    file: Option<File>,
    has_written: bool,
    pid_meta_done: HashSet<u32>,
    tid_meta_done: HashSet<i64>,
}

impl ChromeTraceExporter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl EventsExporter for ChromeTraceExporter {
    fn open(&mut self, path: &Path) -> std::io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        self.file = Some(file);
        self.has_written = false;
        self.pid_meta_done.clear();
        self.tid_meta_done.clear();
        Ok(())
    }

    fn add_event(&mut self, pid: u32, event: &GcStat) {
        let file = match self.file.as_mut() {
            Some(f) => f,
            None => return,
        };

        let mut first = !self.has_written;
        self.has_written = true;

        if self.pid_meta_done.insert(pid) {
            write_process_meta(file, &mut first, pid).ok();
        }

        let tid = event.interpreter_id;
        if self.tid_meta_done.insert(tid) {
            write_thread_meta(file, &mut first, pid, tid).ok();
        }

        write_gc_stat_events(file, &mut first, pid, event).ok();
        file.flush().ok();
    }

    fn mark_process_lifecycle(&mut self, _pid: u32, _kind: ProcessLifecycle, _ts_ns: i64) {}

    fn close(&mut self) -> std::io::Result<()> {
        if let Some(file) = self.file.take() {
            let mut file = file;
            if self.has_written {
                write!(file, "\n]")?;
            } else {
                writeln!(file, "[]")?;
            }
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::offsets::offset_table::{seq_layout, GcItemLayout};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::LazyLock;

    /// A layout carrying every field the exporter knows about — so a test stat can set any
    /// phase's timestamps. Only the field *names* matter here (the exporter reads by name), so
    /// offsets are assigned sequentially. A real regular build's layout has only a subset; the
    /// exporter skips phases whose fields are absent (see `Start`/`PHASES`).
    static FULL_LAYOUT: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start", "ts_stop", "collections", "collected", "uncollectable", "candidates",
            "duration", "heap_size", "increment_size", "alive_size", "finalized_garbage_count",
            "clear_weakrefs_count", "deleted_garbage_count", "ts_mark_alive_start",
            "ts_mark_alive_stop", "ts_fill_increment_start", "ts_fill_increment_stop",
            "ts_deduce_unreachable_start", "ts_deduce_unreachable_stop",
            "ts_handle_weakref_callbacks_start", "ts_handle_weakref_callbacks_stop",
            "ts_finalize_garbage_stop", "ts_handle_resurrected_stop", "ts_clear_weakrefs_stop",
            "ts_delete_garbage_start", "ts_delete_garbage_stop",
        ])
    });

    /// A standard build's layout: the core counters + timestamps, but **none** of the `+inc`
    /// phase fields. A stat over this layout must make the exporter read every phase field as
    /// genuinely absent (`get(..) == None`), not as a zero-width span — the real regular-build
    /// path that `FULL_LAYOUT`-based stats never exercise.
    static REGULAR_LAYOUT: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start", "ts_stop", "collections", "collected", "uncollectable", "candidates",
            "duration", "heap_size",
        ])
    });

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique scratch path per test invocation. `Date.now()`-style entropy is
    /// avoided; a process-id + monotonic counter is enough for isolation within
    /// one `cargo test` run and lets tests run in parallel without colliding.
    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("gcscope_chrome_{}_{}.json", std::process::id(), n));
        p
    }

    /// Drive the public exporter API end-to-end and return the file it produced.
    /// This deliberately exercises `open`/`add_event`/`close` — the same path the
    /// monitor loop uses — rather than the private `write_*` helpers.
    fn export(events: &[(u32, GcStat)]) -> String {
        let path = temp_path();
        let mut ex = ChromeTraceExporter::new();
        ex.open(&path).unwrap();
        for (pid, ev) in events {
            ex.add_event(*pid, ev);
        }
        ex.close().unwrap();
        let s = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();
        s
    }

    fn count(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    /// A minimally-populated GC pause: no sub-step timestamps set (all phase fields zero, so
    /// every sub-step is a zero-width span), so only the outer pause + the two counter events
    /// should be emitted.
    fn bare_stat() -> GcStat {
        GcStat::from_fields(0, 0, 1, *FULL_LAYOUT, &[("ts_start", 1_000), ("ts_stop", 2_000)])
    }

    /// A pause with every sub-step's timestamps set to non-empty, monotonically increasing
    /// ranges — so every sub-step fires.
    fn full_stat() -> GcStat {
        GcStat::from_fields(1, 0, 3, *FULL_LAYOUT, &[
            ("ts_start", 1_000), ("ts_stop", 11_000),
            ("collections", 5), ("collected", 42), ("uncollectable", 7), ("candidates", 100),
            ("heap_size", 4096),
            ("increment_size", 11), ("alive_size", 22),
            ("finalized_garbage_count", 3), ("clear_weakrefs_count", 4), ("deleted_garbage_count", 9),
            ("ts_mark_alive_start", 2_000), ("ts_mark_alive_stop", 3_000),
            ("ts_fill_increment_start", 3_000), ("ts_fill_increment_stop", 4_000),
            ("ts_deduce_unreachable_start", 4_000), ("ts_deduce_unreachable_stop", 5_000),
            ("ts_handle_weakref_callbacks_start", 5_000), ("ts_handle_weakref_callbacks_stop", 6_000),
            ("ts_finalize_garbage_stop", 7_000),
            ("ts_handle_resurrected_stop", 8_000),
            ("ts_clear_weakrefs_stop", 9_000),
            ("ts_delete_garbage_start", 9_000), ("ts_delete_garbage_stop", 10_000),
        ])
    }

    /// Scan for balanced `{}`/`[]` outside of JSON string literals. A cheap
    /// well-formedness proxy that catches an unclosed object/array — the classic
    /// failure when a `write_*` helper forgets its closing brace.
    fn brackets_balanced(s: &str) -> bool {
        let mut stack = Vec::new();
        let mut in_str = false;
        let mut escaped = false;
        for c in s.chars() {
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' if stack.pop() != Some(c) => return false,
                _ => {}
            }
        }
        stack.is_empty() && !in_str
    }

    /// With no events, the trace must still be a valid, empty JSON array — an
    /// empty file or a lone `[` would make Perfetto reject the whole capture.
    #[test]
    fn empty_trace_is_a_valid_empty_array() {
        let out = export(&[]);
        assert_eq!(out.trim(), "[]");
        assert!(brackets_balanced(&out));
    }

    /// A non-empty trace is one JSON array: opens with `[`, closes with `]`, and
    /// every brace it opens it closes.
    #[test]
    fn non_empty_trace_is_a_single_balanced_array() {
        let out = export(&[(42, bare_stat())]);
        assert!(out.trim_start().starts_with('['), "output: {out}");
        assert!(out.trim_end().ends_with(']'), "output: {out}");
        assert!(brackets_balanced(&out), "output: {out}");
    }

    /// Every `"ph":"B"` (begin) must be matched by a `"ph":"E"` (end), no matter
    /// which optional sub-steps fired. An orphaned begin renders as a slice that
    /// never closes in Perfetto and corrupts the flame graph.
    #[test]
    fn begin_and_end_events_are_balanced() {
        for stat in [bare_stat(), full_stat()] {
            let out = export(&[(1, stat)]);
            assert_eq!(
                count(&out, r#""ph":"B""#),
                count(&out, r#""ph":"E""#),
                "unbalanced begin/end in: {out}"
            );
        }
    }

    /// The fully-populated pause fires the outer pause plus all eight sub-steps,
    /// so exactly nine begin/end pairs. This pins the sub-step wiring: dropping
    /// one (or double-emitting) changes the count.
    #[test]
    fn full_pause_emits_every_sub_step_once() {
        let out = export(&[(1, full_stat())]);
        assert_eq!(count(&out, r#""ph":"B""#), 9, "output: {out}");
        // Each phase name appears exactly twice — once in its begin line and once
        // in its end line. A dropped sub-step drops to 0; a double-emit goes to 4.
        for name in [
            "GC Pause (gen=1)",
            "Mark Alive (gen=1)",
            "Fill increment (gen=1)",
            "Deduce Unreachable (gen=1)",
            "Handle Weakrefs Callbacks (gen=1)",
            "Finalize Garbage (gen=1)",
            "Handle Resurrected (gen=1)",
            "Clear Weakrefs (gen=1)",
            "Delete Garbage (gen=1)",
        ] {
            assert_eq!(count(&out, name), 2, "expected exactly one begin+end {name:?} in: {out}");
        }
    }

    /// `write_sub_step` skips any range whose stop is not strictly after its
    /// start. A zero-width sub-step (start == stop) must not appear at all —
    /// emitting it would push a begin without a meaningful end into the trace.
    #[test]
    fn zero_width_sub_steps_are_skipped() {
        let stat = GcStat::from_fields(0, 0, 1, *FULL_LAYOUT, &[
            ("ts_start", 1_000), ("ts_stop", 2_000),
            ("ts_mark_alive_start", 5_000), ("ts_mark_alive_stop", 5_000), // equal → skipped
        ]);
        let out = export(&[(1, stat)]);
        assert!(!out.contains("Mark Alive"), "output: {out}");
        // Only the outer pause survives.
        assert_eq!(count(&out, r#""ph":"B""#), 1, "output: {out}");
    }

    /// A stat from a **standard** build — whose layout lacks every `+inc` phase field — must
    /// emit only the outer GC Pause: the exporter reads each phase field as absent (`None`) and
    /// fabricates no sub-step. This exercises the `get(..) == None → skip` branch (both the
    /// Explicit and Chained phases), which the `FULL_LAYOUT` stats can't reach — there the
    /// fields exist and are merely zero-width. A regression that made a missing field decode to
    /// `Some(0)` would slip past every other exporter test but fail here (via the counter/pause
    /// still being present while no phase span is).
    #[test]
    fn a_standard_layout_stat_emits_no_phase_sub_steps() {
        // A real, non-zero pause so the outer span is genuine — only the phases are absent.
        let s = GcStat::from_fields(0, 0, 1, *REGULAR_LAYOUT, &[
            ("ts_start", 1_000), ("ts_stop", 9_000), ("collections", 3), ("heap_size", 4096),
        ]);
        let out = export(&[(1, s)]);

        // The outer pause and its two counters, and nothing else.
        assert_eq!(count(&out, r#""ph":"B""#), 1, "only the outer pause: {out}");
        assert_eq!(count(&out, r#""ph":"C""#), 2, "still the two counters: {out}");
        for phase in [
            "Mark Alive", "Fill increment", "Deduce Unreachable", "Handle Weakrefs Callbacks",
            "Finalize Garbage", "Handle Resurrected", "Clear Weakrefs", "Delete Garbage",
        ] {
            assert!(!out.contains(phase), "no {phase:?} span for a standard-set stat: {out}");
        }
    }

    /// CPython hands us nanoseconds; the trace format is microseconds. The pause
    /// begin timestamp must be divided by 1000 — a missed conversion inflates
    /// every duration 1000× and desyncs the timeline.
    #[test]
    fn timestamps_are_converted_nanoseconds_to_microseconds() {
        let out = export(&[(1, bare_stat())]); // ts_start = 1_000 ns → 1 µs
        assert!(
            out.contains(r#""ts":1,"name":"GC Pause (gen=0)""#),
            "expected µs-converted pause ts in: {out}"
        );
    }

    /// Process metadata is emitted once per PID and thread metadata once per
    /// interpreter id, regardless of how many events arrive — the `HashSet`
    /// dedup guards against a metadata line per event.
    #[test]
    fn process_and_thread_metadata_are_deduped() {
        let a = GcStat::from_fields(0, 0, 1, *FULL_LAYOUT, &[("ts_start", 1_000), ("ts_stop", 2_000)]);
        let b = GcStat::from_fields(0, 0, 1, *FULL_LAYOUT, &[("ts_start", 3_000), ("ts_stop", 4_000)]);
        let out = export(&[(100, a), (100, b)]);
        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
        assert_eq!(count(&out, r#""name":"thread_name""#), 1, "output: {out}");
    }

    /// Distinct PIDs and distinct interpreter ids each get their own metadata
    /// line — otherwise a second process/thread would inherit the first's name.
    #[test]
    fn distinct_pids_and_tids_each_get_metadata() {
        let p1 = GcStat::from_fields(0, 0, 1, *FULL_LAYOUT, &[("ts_start", 1_000), ("ts_stop", 2_000)]);
        let p2 = GcStat::from_fields(0, 0, 2, *FULL_LAYOUT, &[("ts_start", 1_000), ("ts_stop", 2_000)]);
        let out = export(&[(100, p1), (200, p2)]);
        assert_eq!(count(&out, r#""name":"process_name""#), 2, "output: {out}");
        assert_eq!(count(&out, r#""name":"thread_name""#), 2, "output: {out}");
    }

    /// The generation counter event only carries `uncollectable` when it is
    /// non-zero, to keep the common (zero) case terse. Presence/absence must
    /// track the value. (The *pause begin* always reports `uncollectable`, so
    /// this asserts against the `"G{gen}"` counter line specifically.)
    #[test]
    fn uncollectable_counter_arg_appears_only_when_non_zero() {
        let counter_line = |out: &str| -> String {
            out.lines()
                .find(|l| l.contains(r#""ph":"C""#) && l.contains(r#""collected""#))
                .expect("a generation counter event")
                .to_string()
        };

        let zero = export(&[(1, bare_stat())]); // uncollectable defaults to 0
        assert!(
            !counter_line(&zero).contains("uncollectable"),
            "counter line: {}",
            counter_line(&zero)
        );

        let nonzero = export(&[(1, full_stat())]); // uncollectable = 7
        assert!(
            counter_line(&nonzero).contains(r#""uncollectable":7"#),
            "counter line: {}",
            counter_line(&nonzero)
        );
    }

    /// Counter (`"ph":"C"`) events drive Perfetto's numeric tracks. Every pause
    /// emits two — the per-generation metrics and the heap-size series.
    #[test]
    fn each_pause_emits_two_counter_events() {
        let out = export(&[(1, bare_stat())]);
        assert_eq!(count(&out, r#""ph":"C""#), 2, "output: {out}");
    }

    /// Reusing an exporter across `open` calls must reset the dedup sets, or the
    /// second capture would silently omit metadata for a PID seen in the first.
    #[test]
    fn reopen_resets_metadata_dedup() {
        let mut ex = ChromeTraceExporter::new();

        let path1 = temp_path();
        ex.open(&path1).unwrap();
        ex.add_event(100, &bare_stat());
        ex.close().unwrap();
        fs::remove_file(&path1).ok();

        let path2 = temp_path();
        ex.open(&path2).unwrap();
        ex.add_event(100, &bare_stat());
        ex.close().unwrap();
        let out = fs::read_to_string(&path2).unwrap();
        fs::remove_file(&path2).ok();

        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
    }
}
