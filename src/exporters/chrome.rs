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

fn write_gc_stat_events(
    f: &mut File, first: &mut bool, pid: u32, s: &GcStat,
) -> std::io::Result<()> {
    let tid = s.interpreter_id;
    let ts_s = s.ts_start;
    let ts_e = s.ts_stop;
    let g = s.generation;

    let pause_name = format!("GC Pause (gen={})", g);
    let pause_cat = format!("gc.pause(gen={})", g);

    // Helper to build JSON args string
    let a = |pairs: &[(&str, &str)]| -> String {
        pairs.iter().map(|(k,v)| format!(r#""{}":{}"#, k, v)).collect::<Vec<_>>().join(",")
    };

    // Begin GC Pause
    write_begin(f, first, pid, tid, ts_us(ts_s), &pause_name, &pause_cat,
        &a(&[("generation", &format!("{}", g)),
             ("iid", &format!("{}", s.interpreter_id)),
             ("collections", &format!("{}", s.collections)),
             ("heap_size", &format!("{}", s.heap_size)),
             ("collected", &format!("{}", s.collected)),
             ("uncollectable", &format!("{}", s.uncollectable)),
             ("candidates", &format!("{}", s.candidates))]))?;

    // Mark Alive sub-step
    if let (Some(ss), Some(se)) = (s.ts_mark_alive_start, s.ts_mark_alive_stop) {
        write_sub_step(f, first, pid, tid,
            &format!("Mark Alive (gen={})", g),
            &format!("gc.mark.alive(gen={})", g), ss, se,
            &a(&[("generation", &format!("{}", g)),
                 ("iid", &format!("{}", s.interpreter_id)),
                 ("alive_size", &format!("{}", s.alive_size.unwrap_or(0)))]))?;
    }

    // Fill Increment sub-step
    if let (Some(ss), Some(se)) = (s.ts_fill_increment_start, s.ts_fill_increment_stop) {
        write_sub_step(f, first, pid, tid,
            &format!("Fill increment (gen={})", g),
            &format!("gc.increment(gen={})", g), ss, se,
            &a(&[("generation", &format!("{}", g)),
                 ("iid", &format!("{}", s.interpreter_id)),
                 ("increment_size", &format!("{}", s.increment_size.unwrap_or(0)))]))?;
    }

    // Deduce Unreachable sub-step
    if let (Some(ss), Some(se)) = (s.ts_deduce_unreachable_start, s.ts_deduce_unreachable_stop) {
        write_sub_step(f, first, pid, tid,
            &format!("Deduce Unreachable (gen={})", g),
            &format!("gc.deduce(gen={})", g), ss, se,
            &a(&[("generation", &format!("{}", g)),
                 ("iid", &format!("{}", s.interpreter_id)),
                 ("candidates", &format!("{}", s.candidates))]))?;
    }

    // Handle Weakrefs Callbacks sub-step
    if let (Some(ss), Some(se)) = (s.ts_handle_weakref_callbacks_start, s.ts_handle_weakref_callbacks_stop) {
        write_sub_step(f, first, pid, tid,
            &format!("Handle Weakrefs Callbacks (gen={})", g),
            &format!("gc.weakrefs(gen={})", g), ss, se,
            &a(&[("generation", &format!("{}", g)),
                 ("iid", &format!("{}", s.interpreter_id))]))?;
    }

    // Finalize Garbage — start = weakrefs_stop, end = ts_finalize_garbage_stop
    if let Some(fg_stop) = s.ts_finalize_garbage_stop {
        let fg_start = s.ts_handle_weakref_callbacks_stop
            .or(s.ts_deduce_unreachable_stop)
            .unwrap_or(ts_s);
        if fg_stop > fg_start {
            write_sub_step(f, first, pid, tid,
                &format!("Finalize Garbage (gen={})", g),
                &format!("gc.finalize(gen={})", g), fg_start, fg_stop,
                &a(&[("generation", &format!("{}", g)),
                     ("iid", &format!("{}", s.interpreter_id)),
                     ("finalized_garbage_count", &format!("{}", s.finalized_garbage_count.unwrap_or(0)))]))?;
        }
    }

    // Handle Resurrected — start = finalize_garbage_stop, end = ts_handle_resurrected_stop
    if let Some(hr_stop) = s.ts_handle_resurrected_stop {
        let hr_start = s.ts_finalize_garbage_stop.unwrap_or(ts_s);
        if hr_stop > hr_start {
            write_sub_step(f, first, pid, tid,
                &format!("Handle Resurrected (gen={})", g),
                &format!("gc.resurrect(gen={})", g), hr_start, hr_stop,
                &a(&[("generation", &format!("{}", g)),
                     ("iid", &format!("{}", s.interpreter_id))]))?;
        }
    }

    // Clear Weakrefs — start = handle_resurrected_stop, end = ts_clear_weakrefs_stop
    if let Some(cw_stop) = s.ts_clear_weakrefs_stop {
        let cw_start = s.ts_handle_resurrected_stop.unwrap_or(ts_s);
        if cw_stop > cw_start {
            write_sub_step(f, first, pid, tid,
                &format!("Clear Weakrefs (gen={})", g),
                &format!("gc.clear_weakrefs(gen={})", g), cw_start, cw_stop,
                &a(&[("generation", &format!("{}", g)),
                     ("iid", &format!("{}", s.interpreter_id)),
                     ("clear_weakrefs_count", &format!("{}", s.clear_weakrefs_count.unwrap_or(0)))]))?;
        }
    }

    // Delete Garbage sub-step
    if let (Some(ss), Some(se)) = (s.ts_delete_garbage_start, s.ts_delete_garbage_stop) {
        write_sub_step(f, first, pid, tid,
            &format!("Delete Garbage (gen={})", g),
            &format!("gc.delete(gen={})", g), ss, se,
            &a(&[("generation", &format!("{}", g)),
                 ("iid", &format!("{}", s.interpreter_id)),
                 ("deleted_garbage_count", &format!("{}", s.deleted_garbage_count.unwrap_or(0)))]))?;
    }

    // End GC Pause
    write_end(f, first, pid, tid, ts_us(ts_e), &pause_name, &pause_cat)?;

    // Counter event for generation metrics
    let other = if s.uncollectable > 0 { format!(r#","uncollectable":{}"#, s.uncollectable) } else { String::new() };
    write_event(f, first, &format!(
        r#"{{"name":"G{}","ph":"C","ts":{},"pid":{},"tid":{},"args":{{"collected":{},"candidates":{},"duration":{}{}}}}}"#,
        g, ts_us(ts_s), pid, tid, s.collected, s.candidates, s.duration, other
    ))?;

    // Counter event for heap_size (single arg → encoder sets name to "")
    write_event(f, first, &format!(
        r#"{{"name":"","ph":"C","ts":{},"pid":{},"tid":{},"args":{{"heap_size":{}}}}}"#,
        ts_us(ts_s), pid, tid, s.heap_size
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    /// A minimally-populated GC pause: no sub-step timestamps set, so only the
    /// outer pause + the two counter events should be emitted.
    fn bare_stat() -> GcStat {
        GcStat {
            generation: 0,
            interpreter_id: 1,
            ts_start: 1_000,
            ts_stop: 2_000,
            ..Default::default()
        }
    }

    /// A pause with every optional sub-step's timestamps set to non-empty,
    /// monotonically increasing ranges — so every sub-step fires.
    fn full_stat() -> GcStat {
        GcStat {
            generation: 1,
            interpreter_id: 3,
            ts_start: 1_000,
            ts_stop: 11_000,
            collections: 5,
            collected: 42,
            uncollectable: 7,
            candidates: 100,
            duration: 0.5,
            heap_size: 4096,
            increment_size: Some(11),
            alive_size: Some(22),
            finalized_garbage_count: Some(3),
            clear_weakrefs_count: Some(4),
            deleted_garbage_count: Some(9),
            ts_mark_alive_start: Some(2_000),
            ts_mark_alive_stop: Some(3_000),
            ts_fill_increment_start: Some(3_000),
            ts_fill_increment_stop: Some(4_000),
            ts_deduce_unreachable_start: Some(4_000),
            ts_deduce_unreachable_stop: Some(5_000),
            ts_handle_weakref_callbacks_start: Some(5_000),
            ts_handle_weakref_callbacks_stop: Some(6_000),
            ts_finalize_garbage_stop: Some(7_000),
            ts_handle_resurrected_stop: Some(8_000),
            ts_clear_weakrefs_stop: Some(9_000),
            ts_delete_garbage_start: Some(9_000),
            ts_delete_garbage_stop: Some(10_000),
            slot: 0,
        }
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
        let mut stat = bare_stat();
        stat.ts_mark_alive_start = Some(5_000);
        stat.ts_mark_alive_stop = Some(5_000); // equal → skipped
        let out = export(&[(1, stat)]);
        assert!(!out.contains("Mark Alive"), "output: {out}");
        // Only the outer pause survives.
        assert_eq!(count(&out, r#""ph":"B""#), 1, "output: {out}");
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
        let a = GcStat { interpreter_id: 1, ts_start: 1_000, ts_stop: 2_000, ..Default::default() };
        let b = GcStat { interpreter_id: 1, ts_start: 3_000, ts_stop: 4_000, ..Default::default() };
        let out = export(&[(100, a), (100, b)]);
        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
        assert_eq!(count(&out, r#""name":"thread_name""#), 1, "output: {out}");
    }

    /// Distinct PIDs and distinct interpreter ids each get their own metadata
    /// line — otherwise a second process/thread would inherit the first's name.
    #[test]
    fn distinct_pids_and_tids_each_get_metadata() {
        let p1 = GcStat { interpreter_id: 1, ts_start: 1_000, ts_stop: 2_000, ..Default::default() };
        let p2 = GcStat { interpreter_id: 2, ts_start: 1_000, ts_stop: 2_000, ..Default::default() };
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
