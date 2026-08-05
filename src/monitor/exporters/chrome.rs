//! The Chrome Trace encoder: [`TraceEvent`]s in, `chrome://tracing` JSON out.
//!
//! What a Collection looks like in a trace is decided in [`crate::monitor::convert`]. This
//! file knows only how Chrome spells an event: which `ph` letter each kind takes, in which
//! key order, and that its timestamps are microseconds where the model's are nanoseconds.
//!
//! The JSON is hand-written rather than serialized. The shapes are fixed and tiny, the crate
//! has no serde dependency, and the regression gate at the bottom of this file pins the
//! output byte for byte.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use super::timing::ts_us;
use super::{EventsExporter, ProcessLifecycle};
use crate::monitor::trace_event::{Arg, TraceEvent};

fn write_event(f: &mut File, first: &mut bool, json: &str) -> std::io::Result<()> {
    if *first {
        *first = false;
        write!(f, "[\n{}", json)
    } else {
        write!(f, ",\n{}", json)
    }
}

/// The inside of an `args` object: `"key":value` pairs, comma-separated. Values are bare
/// numbers, so nothing here needs escaping.
fn args_json(args: &[Arg]) -> String {
    args.iter()
        .map(|(key, value)| format!(r#""{}":{}"#, key, value))
        .collect::<Vec<_>>()
        .join(",")
}

/// One event as its Chrome Trace JSON object.
///
/// Key order differs between the span events and the counter/instant events. That is what
/// the encoder emitted before this file was split, and operators have saved traces written
/// that way, so it stays.
fn encode(event: &TraceEvent) -> String {
    match event {
        TraceEvent::ProcessMeta { pid, name } => format!(
            r#"{{"ph":"M","pid":{},"name":"process_name","args":{{"name":"{}"}}}}"#,
            pid, name
        ),
        TraceEvent::ThreadMeta { pid, tid, name } => format!(
            r#"{{"ph":"M","pid":{},"tid":{},"name":"thread_name","args":{{"name":"{}"}}}}"#,
            pid, tid, name
        ),
        TraceEvent::Begin {
            pid,
            tid,
            ts_ns,
            name,
            cat,
            args,
        } => format!(
            r#"{{"ph":"B","pid":{},"tid":{},"ts":{},"name":"{}","cat":"{}","args":{{{}}}}}"#,
            pid,
            tid,
            ts_us(*ts_ns),
            name,
            cat,
            args_json(args)
        ),
        TraceEvent::End {
            pid,
            tid,
            ts_ns,
            name,
            cat,
        } => format!(
            r#"{{"ph":"E","pid":{},"tid":{},"ts":{},"name":"{}","cat":"{}"}}"#,
            pid,
            tid,
            ts_us(*ts_ns),
            name,
            cat
        ),
        // `"s":"p"` scopes the marker to the process: an instant message is the Observed
        // talking about itself, not about one interpreter.
        TraceEvent::Instant { pid, ts_ns, name } => format!(
            r#"{{"name":"{}","ph":"I","s":"p","ts":{},"pid":{}}}"#,
            name,
            ts_us(*ts_ns),
            pid
        ),
        TraceEvent::Counter {
            pid,
            tid,
            ts_ns,
            name,
            args,
        } => format!(
            r#"{{"name":"{}","ph":"C","ts":{},"pid":{},"tid":{},"args":{{{}}}}}"#,
            name,
            ts_us(*ts_ns),
            pid,
            tid,
            args_json(args)
        ),
    }
}

/// `Default` is derived rather than just `new()`-provided because the split to a
/// library makes this a public constructor, and clippy's `new_without_default`
/// applies to public API. All four fields are already `Default`, so the derive is
/// exactly what `new()` did.
#[derive(Default)]
pub struct ChromeTraceExporter {
    file: Option<File>,
    has_written: bool,
    /// Metadata already written into *this* stream. Dedup lives here, not in the conversion,
    /// because it is a property of one output file: two formats fanned out from one
    /// conversion each need their own copy, and reopening starts a file that needs metadata
    /// again.
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

    fn add_events(&mut self, events: &[TraceEvent]) {
        let Self {
            file,
            has_written,
            pid_meta_done,
            tid_meta_done,
        } = self;
        let Some(file) = file.as_mut() else {
            return;
        };

        let mut first = !*has_written;
        for event in events {
            // Repeated metadata is dropped. The thread set is keyed on the interpreter id
            // alone, so two processes whose interpreter ids collide share one `thread_name`
            // line; kept as-is, since the monitor reads one interpreter per process today.
            let skip = match event {
                TraceEvent::ProcessMeta { pid, .. } => !pid_meta_done.insert(*pid),
                TraceEvent::ThreadMeta { tid, .. } => !tid_meta_done.insert(*tid),
                _ => false,
            };
            if skip {
                continue;
            }
            write_event(file, &mut first, &encode(event)).ok();
            *has_written = true;
        }
        file.flush().ok();
    }

    fn mark_process_lifecycle(&mut self, _pid: u32, _kind: ProcessLifecycle, _ts_ns: i64) {}

    fn close(&mut self) -> std::io::Result<()> {
        // A never-opened (or already-closed) exporter closes cleanly: `run_loop` unwinds
        // through `close` on paths where `open` never ran.
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        if self.has_written {
            write!(file, "\n]")?;
        } else {
            writeln!(file, "[]")?;
        }
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::convert::convert_record;
    use crate::remote_debugging::gc_stats::GcStat;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A layout carrying every field the conversion knows about — so a test Record can set
    /// any phase's timestamps. Only the field *names* matter here (fields are read by name),
    /// so offsets are assigned sequentially. A real regular build's layout has only a subset;
    /// the conversion skips phases whose fields are absent (see `convert::PHASES`).
    static FULL_LAYOUT: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
            "candidates",
            "duration",
            "heap_size",
            "increment_size",
            "alive_size",
            "finalized_garbage_count",
            "clear_weakrefs_count",
            "deleted_garbage_count",
            "ts_mark_alive_start",
            "ts_mark_alive_stop",
            "ts_fill_increment_start",
            "ts_fill_increment_stop",
            "ts_deduce_unreachable_start",
            "ts_deduce_unreachable_stop",
            "ts_handle_weakref_callbacks_start",
            "ts_handle_weakref_callbacks_stop",
            "ts_finalize_garbage_stop",
            "ts_handle_resurrected_stop",
            "ts_clear_weakrefs_stop",
            "ts_delete_garbage_start",
            "ts_delete_garbage_stop",
        ])
    });

    /// A standard build's layout: the core counters + timestamps, but **none** of the `+inc`
    /// phase fields. A Record over this layout must make the conversion read every phase field
    /// as genuinely absent (`get(..) == None`), not as a zero-width span — the real
    /// regular-build path that `FULL_LAYOUT`-based Records never exercise.
    static REGULAR_LAYOUT: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
            "candidates",
            "duration",
            "heap_size",
        ])
    });

    /// A build carrying a chained phase's *stop* field but none of the fields it chains
    /// onto. Only such a layout reaches the `unwrap_or(ts_start)` fallback: with
    /// `FULL_LAYOUT` every candidate field exists, so the chain resolves to `Some(0)`.
    static CHAINED_ONLY_LAYOUT: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "heap_size",
            "ts_finalize_garbage_stop",
            "ts_handle_resurrected_stop",
        ])
    });

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// The exact bytes `golden_matrix` produced before the `TraceEvent` extraction.
    const GOLDEN_MATRIX_TRACE: &str = r#"[
{"ph":"M","pid":100,"name":"process_name","args":{"name":"python 100"}},
{"ph":"M","pid":100,"tid":3,"name":"thread_name","args":{"name":"100:3"}},
{"ph":"B","pid":100,"tid":3,"ts":1,"name":"GC Pause (gen=1)","cat":"gc.pause(gen=1)","args":{"generation":1,"iid":3,"collections":5,"heap_size":4096,"collected":42,"uncollectable":7,"candidates":100}},
{"ph":"B","pid":100,"tid":3,"ts":2,"name":"Mark Alive (gen=1)","cat":"gc.mark.alive(gen=1)","args":{"generation":1,"iid":3,"alive_size":22}},
{"ph":"E","pid":100,"tid":3,"ts":3,"name":"Mark Alive (gen=1)","cat":"gc.mark.alive(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":3,"name":"Fill increment (gen=1)","cat":"gc.increment(gen=1)","args":{"generation":1,"iid":3,"increment_size":11}},
{"ph":"E","pid":100,"tid":3,"ts":4,"name":"Fill increment (gen=1)","cat":"gc.increment(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":4,"name":"Deduce Unreachable (gen=1)","cat":"gc.deduce(gen=1)","args":{"generation":1,"iid":3,"candidates":100}},
{"ph":"E","pid":100,"tid":3,"ts":5,"name":"Deduce Unreachable (gen=1)","cat":"gc.deduce(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":5,"name":"Handle Weakrefs Callbacks (gen=1)","cat":"gc.weakrefs(gen=1)","args":{"generation":1,"iid":3}},
{"ph":"E","pid":100,"tid":3,"ts":6,"name":"Handle Weakrefs Callbacks (gen=1)","cat":"gc.weakrefs(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":6,"name":"Finalize Garbage (gen=1)","cat":"gc.finalize(gen=1)","args":{"generation":1,"iid":3,"finalized_garbage_count":3}},
{"ph":"E","pid":100,"tid":3,"ts":7,"name":"Finalize Garbage (gen=1)","cat":"gc.finalize(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":7,"name":"Handle Resurrected (gen=1)","cat":"gc.resurrect(gen=1)","args":{"generation":1,"iid":3}},
{"ph":"E","pid":100,"tid":3,"ts":8,"name":"Handle Resurrected (gen=1)","cat":"gc.resurrect(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":8,"name":"Clear Weakrefs (gen=1)","cat":"gc.clear_weakrefs(gen=1)","args":{"generation":1,"iid":3,"clear_weakrefs_count":4}},
{"ph":"E","pid":100,"tid":3,"ts":9,"name":"Clear Weakrefs (gen=1)","cat":"gc.clear_weakrefs(gen=1)"},
{"ph":"B","pid":100,"tid":3,"ts":9,"name":"Delete Garbage (gen=1)","cat":"gc.delete(gen=1)","args":{"generation":1,"iid":3,"deleted_garbage_count":9}},
{"ph":"E","pid":100,"tid":3,"ts":10,"name":"Delete Garbage (gen=1)","cat":"gc.delete(gen=1)"},
{"ph":"E","pid":100,"tid":3,"ts":11,"name":"GC Pause (gen=1)","cat":"gc.pause(gen=1)"},
{"name":"G1","ph":"C","ts":1,"pid":100,"tid":3,"args":{"collected":42,"candidates":100,"duration":0.00125,"uncollectable":7}},
{"name":"","ph":"C","ts":1,"pid":100,"tid":3,"args":{"heap_size":4096}},
{"ph":"B","pid":100,"tid":3,"ts":20,"name":"GC Pause (gen=0)","cat":"gc.pause(gen=0)","args":{"generation":0,"iid":3,"collections":6,"heap_size":0,"collected":1,"uncollectable":0,"candidates":2}},
{"ph":"E","pid":100,"tid":3,"ts":20,"name":"GC Pause (gen=0)","cat":"gc.pause(gen=0)"},
{"name":"G0","ph":"C","ts":20,"pid":100,"tid":3,"args":{"collected":1,"candidates":2,"duration":0}},
{"name":"","ph":"C","ts":20,"pid":100,"tid":3,"args":{"heap_size":0}},
{"ph":"M","pid":100,"tid":4,"name":"thread_name","args":{"name":"100:4"}},
{"ph":"B","pid":100,"tid":4,"ts":30,"name":"GC Pause (gen=2)","cat":"gc.pause(gen=2)","args":{"generation":2,"iid":4,"collections":2,"heap_size":0,"collected":0,"uncollectable":1,"candidates":0}},
{"ph":"B","pid":100,"tid":4,"ts":0,"name":"Finalize Garbage (gen=2)","cat":"gc.finalize(gen=2)","args":{"generation":2,"iid":4,"finalized_garbage_count":0}},
{"ph":"E","pid":100,"tid":4,"ts":32,"name":"Finalize Garbage (gen=2)","cat":"gc.finalize(gen=2)"},
{"ph":"E","pid":100,"tid":4,"ts":40,"name":"GC Pause (gen=2)","cat":"gc.pause(gen=2)"},
{"name":"G2","ph":"C","ts":30,"pid":100,"tid":4,"args":{"collected":0,"candidates":0,"duration":0,"uncollectable":1}},
{"name":"","ph":"C","ts":30,"pid":100,"tid":4,"args":{"heap_size":0}},
{"ph":"B","pid":100,"tid":3,"ts":60,"name":"GC Pause (gen=0)","cat":"gc.pause(gen=0)","args":{"generation":0,"iid":3,"collections":8,"heap_size":2048,"collected":0,"uncollectable":0,"candidates":0}},
{"ph":"B","pid":100,"tid":3,"ts":60,"name":"Finalize Garbage (gen=0)","cat":"gc.finalize(gen=0)","args":{"generation":0,"iid":3,"finalized_garbage_count":0}},
{"ph":"E","pid":100,"tid":3,"ts":65,"name":"Finalize Garbage (gen=0)","cat":"gc.finalize(gen=0)"},
{"ph":"B","pid":100,"tid":3,"ts":65,"name":"Handle Resurrected (gen=0)","cat":"gc.resurrect(gen=0)","args":{"generation":0,"iid":3}},
{"ph":"E","pid":100,"tid":3,"ts":66,"name":"Handle Resurrected (gen=0)","cat":"gc.resurrect(gen=0)"},
{"ph":"E","pid":100,"tid":3,"ts":70,"name":"GC Pause (gen=0)","cat":"gc.pause(gen=0)"},
{"name":"G0","ph":"C","ts":60,"pid":100,"tid":3,"args":{"collected":0,"candidates":0,"duration":0}},
{"name":"","ph":"C","ts":60,"pid":100,"tid":3,"args":{"heap_size":2048}},
{"ph":"M","pid":200,"name":"process_name","args":{"name":"python 200"}},
{"ph":"M","pid":200,"tid":9,"name":"thread_name","args":{"name":"200:9"}},
{"ph":"B","pid":200,"tid":9,"ts":50,"name":"GC Pause (gen=2)","cat":"gc.pause(gen=2)","args":{"generation":2,"iid":9,"collections":11,"heap_size":1048576,"collected":3,"uncollectable":0,"candidates":12}},
{"ph":"E","pid":200,"tid":9,"ts":55,"name":"GC Pause (gen=2)","cat":"gc.pause(gen=2)"},
{"name":"G2","ph":"C","ts":50,"pid":200,"tid":9,"args":{"collected":3,"candidates":12,"duration":0}},
{"name":"","ph":"C","ts":50,"pid":200,"tid":9,"args":{"heap_size":1048576}}
]"#;

    /// The digest of the bytes `random_matrix` produced before the same change.
    const RANDOM_MATRIX_DIGEST: u64 = 0x691ac7286d6f515d;

    /// A unique scratch path per test invocation. `Date.now()`-style entropy is
    /// avoided; a process-id + monotonic counter is enough for isolation within
    /// one `cargo test` run and lets tests run in parallel without colliding.
    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("gcscope_chrome_{}_{}.json", std::process::id(), n));
        p
    }

    /// Drive the whole path and return the file it produced: Records through
    /// `convert_record`, events through `open`/`add_events`/`close`, the same route the
    /// monitor loop takes. Not the private `encode`, so ordering and metadata are exercised
    /// too.
    fn export(records: &[(u32, GcStat)]) -> String {
        let path = temp_path();
        let mut ex = ChromeTraceExporter::new();
        ex.open(&path).unwrap();
        for (pid, record) in records {
            ex.add_events(&convert_record(*pid, record));
        }
        ex.close().unwrap();
        let s = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();
        s
    }

    /// The encoder seam on its own: events in, bytes out, with no Record anywhere. Anything
    /// no producer emits yet has to be reached this way.
    fn export_events(events: &[TraceEvent]) -> String {
        let path = temp_path();
        let mut ex = ChromeTraceExporter::new();
        ex.open(&path).unwrap();
        ex.add_events(events);
        ex.close().unwrap();
        let s = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();
        s
    }

    fn count(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    /// An instant message is scoped to the process (`"s":"p"`) and carries no `tid`. Nothing
    /// produces one yet, so this is its only coverage, and its timestamp still makes the
    /// nanosecond-to-microsecond trip.
    #[test]
    fn an_instant_message_encodes_as_a_process_scoped_marker() {
        let out = export_events(&[TraceEvent::Instant {
            pid: 7,
            ts_ns: 4_500,
            name: "checkpoint".to_string(),
        }]);
        assert_eq!(
            out,
            "[\n{\"name\":\"checkpoint\",\"ph\":\"I\",\"s\":\"p\",\"ts\":4,\"pid\":7}\n]"
        );
    }

    /// The encoder writes what it is handed, in the order it is handed it. A sub-span whose
    /// timestamp precedes its parent's is a real shape (a build publishing a phase's stop
    /// but not its start chains it onto a field reading zero), and sorting it back into
    /// place would move a slice the target never put there.
    #[test]
    fn events_are_written_in_the_order_given_even_when_timestamps_are_not() {
        let out = export_events(&[
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 9_000,
                name: "late".to_string(),
                cat: "c".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 1_000,
                name: "late".to_string(),
                cat: "c".to_string(),
            },
        ]);
        let ts: Vec<&str> = out
            .lines()
            .filter_map(|l| l.split("\"ts\":").nth(1))
            .collect();
        assert_eq!(ts.len(), 2, "output: {out}");
        assert!(ts[0].starts_with('9'), "output: {out}");
        assert!(ts[1].starts_with('1'), "output: {out}");
    }

    /// A minimally-populated GC pause: no sub-step timestamps set (all phase fields zero, so
    /// every sub-step is a zero-width span), so only the outer pause + the two counter events
    /// should be emitted.
    fn bare_stat() -> GcStat {
        GcStat::from_fields(
            0,
            0,
            1,
            *FULL_LAYOUT,
            &[("ts_start", 1_000), ("ts_stop", 2_000)],
        )
    }

    /// A pause with every sub-step's timestamps set to non-empty, monotonically increasing
    /// ranges — so every sub-step fires.
    fn full_stat() -> GcStat {
        GcStat::from_fields(
            1,
            0,
            3,
            *FULL_LAYOUT,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 11_000),
                ("collections", 5),
                ("collected", 42),
                ("uncollectable", 7),
                ("candidates", 100),
                ("heap_size", 4096),
                ("increment_size", 11),
                ("alive_size", 22),
                ("finalized_garbage_count", 3),
                ("clear_weakrefs_count", 4),
                ("deleted_garbage_count", 9),
                ("ts_mark_alive_start", 2_000),
                ("ts_mark_alive_stop", 3_000),
                ("ts_fill_increment_start", 3_000),
                ("ts_fill_increment_stop", 4_000),
                ("ts_deduce_unreachable_start", 4_000),
                ("ts_deduce_unreachable_stop", 5_000),
                ("ts_handle_weakref_callbacks_start", 5_000),
                ("ts_handle_weakref_callbacks_stop", 6_000),
                ("ts_finalize_garbage_stop", 7_000),
                ("ts_handle_resurrected_stop", 8_000),
                ("ts_clear_weakrefs_stop", 9_000),
                ("ts_delete_garbage_start", 9_000),
                ("ts_delete_garbage_stop", 10_000),
            ],
        )
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

    /// The scan has to honour JSON escapes: an escaped quote read as the end of its string
    /// would leave every brace after it counted from the wrong state, and the scan would
    /// then wave through the malformed traces it exists to catch. No trace gcscope writes
    /// carries an escape today, which is what makes this worth pinning rather than assuming.
    #[test]
    fn brackets_balanced_honours_escapes_inside_strings() {
        assert!(brackets_balanced(r#"[{"name":"a \" b"}]"#), "escaped quote");
        assert!(
            brackets_balanced(r#"[{"name":"c:\\dir"}]"#),
            "escaped backslash"
        );
        // A brace after an escaped quote is still inside the string, so it is text.
        assert!(
            brackets_balanced(r#"[{"name":"\"}"}]"#),
            "escape then brace"
        );
        // The scan still fails what it is for.
        assert!(!brackets_balanced(r#"[{"name":"x"}"#), "unclosed array");
    }

    /// Events handed to an exporter that was never opened are dropped, and the metadata
    /// dedup sets stay untouched so a later `open` still writes them. The monitor holds the
    /// exporter across a whole run, so a spurious poll before `open` must not swallow the
    /// process's `process_name` line for the rest of the capture.
    #[test]
    fn events_before_open_are_dropped_without_consuming_the_metadata() {
        let mut ex = ChromeTraceExporter::new();
        ex.add_events(&convert_record(100, &bare_stat()));

        let path = temp_path();
        ex.open(&path).unwrap();
        ex.add_events(&convert_record(100, &bare_stat()));
        ex.close().unwrap();
        let out = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
        assert_eq!(count(&out, r#""ph":"B""#), 1, "output: {out}");
    }

    /// `run_loop` unwinds through `close` on paths where `open` never ran: a target that
    /// never attached, or a Ctrl-C during startup. Closing twice has to be as harmless,
    /// since `close` is what takes the file out.
    #[test]
    fn closing_an_unopened_exporter_is_a_clean_no_op() {
        ChromeTraceExporter::new().close().unwrap();

        let path = temp_path();
        let mut ex = ChromeTraceExporter::new();
        ex.open(&path).unwrap();
        ex.add_events(&convert_record(1, &bare_stat()));
        ex.close().unwrap();
        ex.close().unwrap();
        let out = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();

        // One terminator, not two: the second close wrote nothing.
        assert_eq!(count(&out, "]"), 1, "output: {out}");
        assert!(brackets_balanced(&out), "output: {out}");
    }

    /// Chrome has no liveness track gcscope fills, so `mark_process_lifecycle` is swallowed.
    /// Pinned rather than assumed: stray events appearing on process start would shift every
    /// saved analysis, and the byte-identity gate never calls this method.
    #[test]
    fn process_lifecycle_marks_are_not_written_to_a_chrome_trace() {
        let path = temp_path();
        let mut ex = ChromeTraceExporter::new();
        ex.open(&path).unwrap();
        ex.mark_process_lifecycle(7, ProcessLifecycle::Started, 1_000);
        ex.mark_process_lifecycle(7, ProcessLifecycle::Died, 2_000);
        ex.close().unwrap();
        let out = fs::read_to_string(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(out.trim(), "[]", "output: {out}");
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
            assert_eq!(
                count(&out, name),
                2,
                "expected exactly one begin+end {name:?} in: {out}"
            );
        }
    }

    /// The conversion skips any range whose stop is not strictly after its
    /// start. A zero-width sub-step (start == stop) must not appear at all —
    /// emitting it would push a begin without a meaningful end into the trace.
    #[test]
    fn zero_width_sub_steps_are_skipped() {
        let stat = GcStat::from_fields(
            0,
            0,
            1,
            *FULL_LAYOUT,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 2_000),
                ("ts_mark_alive_start", 5_000),
                ("ts_mark_alive_stop", 5_000), // equal → skipped
            ],
        );
        let out = export(&[(1, stat)]);
        assert!(!out.contains("Mark Alive"), "output: {out}");
        // Only the outer pause survives.
        assert_eq!(count(&out, r#""ph":"B""#), 1, "output: {out}");
    }

    /// A stat from a **standard** build — whose layout lacks every `+inc` phase field — must
    /// emit only the outer GC Pause: the conversion reads each phase field as absent (`None`) and
    /// fabricates no sub-step. This exercises the `get(..) == None → skip` branch (both the
    /// Explicit and Chained phases), which the `FULL_LAYOUT` stats can't reach — there the
    /// fields exist and are merely zero-width. A regression that made a missing field decode to
    /// `Some(0)` would slip past every other exporter test but fail here (via the counter/pause
    /// still being present while no phase span is).
    #[test]
    fn a_standard_layout_stat_emits_no_phase_sub_steps() {
        // A real, non-zero pause so the outer span is genuine — only the phases are absent.
        let s = GcStat::from_fields(
            0,
            0,
            1,
            *REGULAR_LAYOUT,
            &[
                ("ts_start", 1_000),
                ("ts_stop", 9_000),
                ("collections", 3),
                ("heap_size", 4096),
            ],
        );
        let out = export(&[(1, s)]);

        // The outer pause and its two counters, and nothing else.
        assert_eq!(count(&out, r#""ph":"B""#), 1, "only the outer pause: {out}");
        assert_eq!(
            count(&out, r#""ph":"C""#),
            2,
            "still the two counters: {out}"
        );
        for phase in [
            "Mark Alive",
            "Fill increment",
            "Deduce Unreachable",
            "Handle Weakrefs Callbacks",
            "Finalize Garbage",
            "Handle Resurrected",
            "Clear Weakrefs",
            "Delete Garbage",
        ] {
            assert!(
                !out.contains(phase),
                "no {phase:?} span for a standard-set stat: {out}"
            );
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
        let a = GcStat::from_fields(
            0,
            0,
            1,
            *FULL_LAYOUT,
            &[("ts_start", 1_000), ("ts_stop", 2_000)],
        );
        let b = GcStat::from_fields(
            0,
            0,
            1,
            *FULL_LAYOUT,
            &[("ts_start", 3_000), ("ts_stop", 4_000)],
        );
        let out = export(&[(100, a), (100, b)]);
        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
        assert_eq!(count(&out, r#""name":"thread_name""#), 1, "output: {out}");
    }

    /// Distinct PIDs and distinct interpreter ids each get their own metadata
    /// line — otherwise a second process/thread would inherit the first's name.
    #[test]
    fn distinct_pids_and_tids_each_get_metadata() {
        let p1 = GcStat::from_fields(
            0,
            0,
            1,
            *FULL_LAYOUT,
            &[("ts_start", 1_000), ("ts_stop", 2_000)],
        );
        let p2 = GcStat::from_fields(
            0,
            0,
            2,
            *FULL_LAYOUT,
            &[("ts_start", 1_000), ("ts_stop", 2_000)],
        );
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

        let zero = counter_line(&export(&[(1, bare_stat())])); // uncollectable defaults to 0
        assert!(!zero.contains("uncollectable"), "counter line: {zero}");

        let nonzero = counter_line(&export(&[(1, full_stat())])); // uncollectable = 7
        assert!(
            nonzero.contains(r#""uncollectable":7"#),
            "counter line: {nonzero}"
        );
    }

    /// Counter (`"ph":"C"`) events drive Perfetto's numeric tracks. Every pause
    /// emits two — the per-generation metrics and the heap-size series.
    #[test]
    fn each_pause_emits_two_counter_events() {
        let out = export(&[(1, bare_stat())]);
        assert_eq!(count(&out, r#""ph":"C""#), 2, "output: {out}");
    }

    // ---------------------------------------------------------------------------------
    // Byte-for-byte regression gate.
    //
    // The `TraceEvent` extraction moved every decision about what a Collection is in a
    // trace out of this file, leaving the encoding. Nothing an operator sees was meant to
    // change, and these two tests are what checks that. Both expectations came from the
    // encoder as it stood before the extraction. Do not regenerate either one to make a
    // failing build pass: a format change rewrites them in the commit that changes the
    // format, never on its own.
    // ---------------------------------------------------------------------------------

    /// The curated half of the gate: inputs chosen so that every branch of the conversion
    /// is reached at least once, and small enough that the expected bytes stay readable.
    fn golden_matrix() -> Vec<(u32, GcStat)> {
        vec![
            // Every phase fires, every argument is non-zero, and `duration` carries real
            // float bits, so float formatting is pinned too.
            (
                100,
                GcStat::from_fields(
                    1,
                    0,
                    3,
                    *FULL_LAYOUT,
                    &[
                        ("ts_start", 1_000),
                        ("ts_stop", 11_000),
                        ("collections", 5),
                        ("collected", 42),
                        ("uncollectable", 7),
                        ("candidates", 100),
                        ("duration", f64::to_bits(0.001_25) as i64),
                        ("heap_size", 4096),
                        ("increment_size", 11),
                        ("alive_size", 22),
                        ("finalized_garbage_count", 3),
                        ("clear_weakrefs_count", 4),
                        ("deleted_garbage_count", 9),
                        ("ts_mark_alive_start", 2_000),
                        ("ts_mark_alive_stop", 3_000),
                        ("ts_fill_increment_start", 3_000),
                        ("ts_fill_increment_stop", 4_000),
                        ("ts_deduce_unreachable_start", 4_000),
                        ("ts_deduce_unreachable_stop", 5_000),
                        ("ts_handle_weakref_callbacks_start", 5_000),
                        ("ts_handle_weakref_callbacks_stop", 6_000),
                        ("ts_finalize_garbage_stop", 7_000),
                        ("ts_handle_resurrected_stop", 8_000),
                        ("ts_clear_weakrefs_stop", 9_000),
                        ("ts_delete_garbage_start", 9_000),
                        ("ts_delete_garbage_stop", 10_000),
                    ],
                ),
            ),
            // Same PID and interpreter, so neither metadata line repeats. A sub-µs pause,
            // `uncollectable` zero so the counter argument is omitted, and a zero-width
            // `Mark Alive` that must be dropped.
            (
                100,
                GcStat::from_fields(
                    0,
                    1,
                    3,
                    *FULL_LAYOUT,
                    &[
                        ("ts_start", 20_100),
                        ("ts_stop", 20_900),
                        ("collections", 6),
                        ("collected", 1),
                        ("candidates", 2),
                        ("ts_mark_alive_start", 20_200),
                        ("ts_mark_alive_stop", 20_200),
                    ],
                ),
            ),
            // A chained phase whose candidates exist in the layout but read zero: the chain
            // resolves to `Some(0)`, so the span starts at the epoch rather than at the
            // pause. Ugly, and what the encoder does.
            (
                100,
                GcStat::from_fields(
                    2,
                    0,
                    4,
                    *FULL_LAYOUT,
                    &[
                        ("ts_start", 30_000),
                        ("ts_stop", 40_000),
                        ("collections", 2),
                        ("uncollectable", 1),
                        ("ts_finalize_garbage_stop", 32_000),
                    ],
                ),
            ),
            // The other chained shape: the candidate fields are absent from the build, so
            // the phase begins at the pause start instead.
            (
                100,
                GcStat::from_fields(
                    0,
                    2,
                    3,
                    *CHAINED_ONLY_LAYOUT,
                    &[
                        ("ts_start", 60_000),
                        ("ts_stop", 70_000),
                        ("collections", 8),
                        ("heap_size", 2048),
                        ("ts_finalize_garbage_stop", 65_000),
                        ("ts_handle_resurrected_stop", 66_000),
                    ],
                ),
            ),
            // A second process on a standard build's layout: new process and thread
            // metadata, with the phase fields absent rather than zero.
            (
                200,
                GcStat::from_fields(
                    2,
                    2,
                    9,
                    *REGULAR_LAYOUT,
                    &[
                        ("ts_start", 50_000),
                        ("ts_stop", 55_000),
                        ("collections", 11),
                        ("collected", 3),
                        ("candidates", 12),
                        ("heap_size", 1_048_576),
                    ],
                ),
            ),
        ]
    }

    #[test]
    fn golden_matrix_bytes_are_unchanged() {
        assert_eq!(export(&golden_matrix()), GOLDEN_MATRIX_TRACE);
    }

    /// FNV-1a, 64-bit. It stands in for an expected string too large to read: any byte
    /// difference moves it, which is all it has to do.
    fn digest(s: &str) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    fn xorshift64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    /// The randomized half of the gate: more shapes than a curated matrix can hold, pinned
    /// by digest. Values come from a table of awkward ones (the extremes, the sign boundary,
    /// float bits printing as `inf`/`NaN`), since that is where two encoders diverge rather
    /// than in the middle of the range. Fixed seed, per `docs/testing-policy.md`.
    fn random_matrix() -> Vec<(u32, GcStat)> {
        const VALUES: [i64; 10] = [
            0,
            1,
            -1,
            7,
            1_000,
            1_000_000,
            i64::MAX,
            i64::MIN,
            0x7ff0_0000_0000_0000, // f64::INFINITY bits
            0x7ff8_0000_0000_0000, // f64::NAN bits
        ];
        let layouts = [*FULL_LAYOUT, *REGULAR_LAYOUT, *CHAINED_ONLY_LAYOUT];

        let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut out = Vec::new();
        for _ in 0..96 {
            let layout = layouts[(xorshift64(&mut rng) % layouts.len() as u64) as usize];
            let fields: Vec<(&str, i64)> = layout
                .fields
                .iter()
                .map(|&(name, _)| (name, VALUES[(xorshift64(&mut rng) % 10) as usize]))
                .collect();
            let generation = (xorshift64(&mut rng) % 3) as u32;
            let index = (xorshift64(&mut rng) % 11) as usize;
            let interpreter_id = (xorshift64(&mut rng) % 4) as i64;
            let pid = 1000 + (xorshift64(&mut rng) % 3) as u32;
            out.push((
                pid,
                GcStat::from_fields(generation, index, interpreter_id, layout, &fields),
            ));
        }
        out
    }

    #[test]
    fn randomized_matrix_digest_is_unchanged() {
        assert_eq!(digest(&export(&random_matrix())), RANDOM_MATRIX_DIGEST);
    }

    /// Reusing an exporter across `open` calls must reset the dedup sets, or the
    /// second capture would silently omit metadata for a PID seen in the first.
    #[test]
    fn reopen_resets_metadata_dedup() {
        let mut ex = ChromeTraceExporter::new();

        let path1 = temp_path();
        ex.open(&path1).unwrap();
        ex.add_events(&convert_record(100, &bare_stat()));
        ex.close().unwrap();
        fs::remove_file(&path1).ok();

        let path2 = temp_path();
        ex.open(&path2).unwrap();
        ex.add_events(&convert_record(100, &bare_stat()));
        ex.close().unwrap();
        let out = fs::read_to_string(&path2).unwrap();
        fs::remove_file(&path2).ok();

        assert_eq!(count(&out, r#""name":"process_name""#), 1, "output: {out}");
    }
}
