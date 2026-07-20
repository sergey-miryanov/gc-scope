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

pub struct ChromeTraceExporter {
    file: Option<File>,
    has_written: bool,
    pid_meta_done: HashSet<u32>,
    tid_meta_done: HashSet<i64>,
}

impl ChromeTraceExporter {
    pub fn new() -> Self {
        ChromeTraceExporter {
            file: None,
            has_written: false,
            pid_meta_done: HashSet::new(),
            tid_meta_done: HashSet::new(),
        }
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
