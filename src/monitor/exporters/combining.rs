//! Fan one stream of [`TraceEvent`]s out to several formats.
//!
//! This is where the spec's rule lives in code: **an exporter encodes what it is handed and
//! never converts from a Record itself**. Every member sees the same `&[TraceEvent]` slice,
//! in the same order, so two formats cannot describe one run differently. gcmon reached that
//! the expensive way — its Chrome and Perfetto paths each converted independently and drifted
//! into two disagreeing traces of one run (gcmon ADR-0007).
//!
//! Metadata dedup stays inside each member rather than here. It is a property of one output
//! stream: two formats fanned out from one conversion each need their own copy of a
//! `process_name`, and a combiner that dropped the second would leave the second format's
//! track unnamed.

use std::path::{Path, PathBuf};

use super::{EventsExporter, ProcessLifecycle};
use crate::monitor::trace_event::TraceEvent;

/// Several [`EventsExporter`]s driven as one.
///
/// ## What a failing member does to the others
///
/// The two fallible calls answer differently, because the cost of carrying on differs:
///
/// - **`open` is all-or-nothing.** An operator who asked for two formats and silently got one
///   would find out when they went looking for the file. On the first member that fails, the
///   members already opened are closed — each left a valid, if empty, trace rather than a
///   half-written one — and the error is returned, so the run never starts.
/// - **`close` closes every member even after one fails**, and reports the first error. A
///   skipped `close` is what leaves a format's file unterminated: Chrome's array is closed
///   there, so returning early on the first error would corrupt the very traces the run was
///   for.
///
/// ## One `--output`, one file per member
///
/// Every member is asked where it writes ([`EventsExporter::output_path`]) before it is
/// opened, so two formats given one `--output` land in two files rather than truncating each
/// other into one. The rule is applied here rather than inside each encoder, because it is a
/// property of the composition: an encoder used alone still writes exactly where it is told.
///
/// No `Default`. An empty fan-out opens, closes and swallows every batch without error, so a
/// run would report a trace written to a file that was never created. The only constructor
/// takes its members.
pub struct CombiningExporter {
    members: Vec<Box<dyn EventsExporter>>,
    /// What the last [`open`](EventsExporter::open) resolved to, in member order. Recorded on
    /// the way through rather than recomputed, so the run reports the files it opened.
    opened: Vec<PathBuf>,
}

impl CombiningExporter {
    pub fn new(members: Vec<Box<dyn EventsExporter>>) -> Self {
        Self {
            members,
            opened: Vec::new(),
        }
    }

    /// The files this fan-out opened, in member order. Empty before the first `open`.
    pub fn opened_paths(&self) -> &[PathBuf] {
        &self.opened
    }
}

impl EventsExporter for CombiningExporter {
    fn open(&mut self, path: &Path) -> std::io::Result<()> {
        self.opened.clear();
        for index in 0..self.members.len() {
            let resolved = self.members[index].output_path(path);
            if let Err(error) = self.members[index].open(&resolved) {
                for opened in &mut self.members[..index] {
                    let _ = opened.close();
                }
                self.opened.clear();
                return Err(error);
            }
            self.opened.push(resolved);
        }
        Ok(())
    }

    fn add_events(&mut self, events: &[TraceEvent]) {
        for member in &mut self.members {
            member.add_events(events);
        }
    }

    fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: i64) {
        for member in &mut self.members {
            member.mark_process_lifecycle(pid, kind, ts_ns);
        }
    }

    fn close(&mut self) -> std::io::Result<()> {
        let mut first_error = None;
        for member in &mut self.members {
            if let Err(error) = member.close()
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{Error, ErrorKind};
    use std::path::PathBuf;
    use std::rc::Rc;

    /// What one member was told to do, in the order it was told. Shared with the test, since
    /// the combiner owns the double once it is boxed.
    #[derive(Default, PartialEq, Debug)]
    struct Log {
        opened: Vec<PathBuf>,
        batches: Vec<Vec<TraceEvent>>,
        lifecycle: Vec<(u32, &'static str, i64)>,
        closed: usize,
    }

    /// A member that records every call. `open_error`/`close_error` arm the fallible ones;
    /// a close failure carries its own message, so a test with two of them can say *which*
    /// error came back.
    struct Double {
        log: Rc<RefCell<Log>>,
        open_error: bool,
        close_error: Option<&'static str>,
        /// Set to make this member resolve `--output` to its own extension, as a real second
        /// format does.
        extension: Option<&'static str>,
    }

    impl Double {
        fn new() -> (Self, Rc<RefCell<Log>>) {
            let log = Rc::new(RefCell::new(Log::default()));
            (
                Double {
                    log: log.clone(),
                    open_error: false,
                    close_error: None,
                    extension: None,
                },
                log,
            )
        }

        fn writing(extension: &'static str) -> (Self, Rc<RefCell<Log>>) {
            let (mut double, log) = Self::new();
            double.extension = Some(extension);
            (double, log)
        }

        fn failing_open() -> (Self, Rc<RefCell<Log>>) {
            let (mut double, log) = Self::new();
            double.open_error = true;
            (double, log)
        }

        fn failing_close(message: &'static str) -> (Self, Rc<RefCell<Log>>) {
            let (mut double, log) = Self::new();
            double.close_error = Some(message);
            (double, log)
        }
    }

    impl EventsExporter for Double {
        fn output_path(&self, requested: &Path) -> PathBuf {
            match self.extension {
                Some(extension) => {
                    let mut path = requested.to_path_buf();
                    path.set_extension(extension);
                    path
                }
                None => requested.to_path_buf(),
            }
        }

        fn open(&mut self, path: &Path) -> std::io::Result<()> {
            if self.open_error {
                return Err(Error::new(ErrorKind::PermissionDenied, "open refused"));
            }
            self.log.borrow_mut().opened.push(path.to_path_buf());
            Ok(())
        }

        fn add_events(&mut self, events: &[TraceEvent]) {
            self.log.borrow_mut().batches.push(events.to_vec());
        }

        fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: i64) {
            let kind = match kind {
                ProcessLifecycle::Started => "started",
                ProcessLifecycle::Died => "died",
            };
            self.log.borrow_mut().lifecycle.push((pid, kind, ts_ns));
        }

        fn close(&mut self) -> std::io::Result<()> {
            self.log.borrow_mut().closed += 1;
            match self.close_error {
                Some(message) => Err(Error::other(message)),
                None => Ok(()),
            }
        }
    }

    fn meta(pid: u32) -> TraceEvent {
        TraceEvent::ProcessMeta {
            pid,
            name: format!("Process {pid}"),
        }
    }

    fn instant(pid: u32, ts_ns: i64) -> TraceEvent {
        TraceEvent::Instant {
            pid,
            ts_ns,
            name: "Start Process".to_string(),
        }
    }

    /// The property the whole fan-out exists for: one conversion, one slice, and every format
    /// sees all of it. A combiner that filtered or reordered per member is how two traces of
    /// one run start disagreeing.
    #[test]
    fn both_members_see_every_event_of_every_batch_in_the_order_given() {
        let (first, first_log) = Double::new();
        let (second, second_log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        let batch_one = vec![meta(11), instant(11, 700)];
        let batch_two = vec![instant(11, 800)];
        combined.add_events(&batch_one);
        combined.add_events(&batch_two);

        let expected = vec![batch_one, batch_two];
        assert_eq!(first_log.borrow().batches, expected);
        assert_eq!(second_log.borrow().batches, expected);
    }

    /// Repeated metadata is not deduped here — each member keeps its own set, because each
    /// writes its own file and both need the `process_name`.
    #[test]
    fn repeated_metadata_reaches_every_member_undeduped() {
        let (first, first_log) = Double::new();
        let (second, second_log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        combined.add_events(&[meta(11)]);
        combined.add_events(&[meta(11)]);

        assert_eq!(first_log.borrow().batches.len(), 2);
        assert_eq!(second_log.borrow().batches.len(), 2);
    }

    #[test]
    fn both_members_are_opened_on_the_same_path_and_both_are_closed() {
        let (first, first_log) = Double::new();
        let (second, second_log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        let path = PathBuf::from("gcmon_trace.json");
        combined.open(&path).expect("both doubles open cleanly");
        combined.close().expect("both doubles close cleanly");

        assert_eq!(first_log.borrow().opened, vec![path.clone()]);
        assert_eq!(second_log.borrow().opened, vec![path]);
        assert_eq!(first_log.borrow().closed, 1);
        assert_eq!(second_log.borrow().closed, 1);
    }

    /// One `--output`, two formats, two files. A member that resolves its own path is opened
    /// on that path, and the one that does not is opened on what the operator typed — so a
    /// second format cannot truncate the first's file and interleave into it.
    #[test]
    fn each_member_is_opened_on_the_path_it_resolved_for_itself() {
        let (first, first_log) = Double::new();
        let (second, second_log) = Double::writing("perfetto-trace");
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        combined
            .open(Path::new("gcmon_trace.json"))
            .expect("both doubles open cleanly");

        assert_eq!(
            first_log.borrow().opened,
            vec![PathBuf::from("gcmon_trace.json")]
        );
        assert_eq!(
            second_log.borrow().opened,
            vec![PathBuf::from("gcmon_trace.perfetto-trace")]
        );
        assert_eq!(
            combined.opened_paths(),
            [
                PathBuf::from("gcmon_trace.json"),
                PathBuf::from("gcmon_trace.perfetto-trace")
            ]
        );
    }

    /// The run reports the files it opened, so a failed `open` must not leave it claiming any.
    /// Reporting a path for a trace that was never created is what this clears.
    #[test]
    fn a_failed_open_leaves_no_path_claimed() {
        let (first, _) = Double::new();
        let (second, _) = Double::failing_open();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        combined
            .open(Path::new("gcmon_trace.json"))
            .expect_err("the second member refuses to open");

        assert!(combined.opened_paths().is_empty());
    }

    #[test]
    fn lifecycle_marks_reach_every_member_with_their_arguments_intact() {
        let (first, first_log) = Double::new();
        let (second, second_log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        combined.mark_process_lifecycle(11, ProcessLifecycle::Started, 700);
        combined.mark_process_lifecycle(11, ProcessLifecycle::Died, 900);

        let expected = vec![(11, "started", 700), (11, "died", 900)];
        assert_eq!(first_log.borrow().lifecycle, expected);
        assert_eq!(second_log.borrow().lifecycle, expected);
    }

    /// `open` is all-or-nothing: the operator asked for both formats, so getting one is a
    /// failure, not a degraded success. The member that did open is closed on the way out,
    /// leaving a valid empty trace rather than an open handle.
    #[test]
    fn a_member_that_cannot_open_stops_the_run_and_closes_the_ones_already_open() {
        let (first, first_log) = Double::new();
        let (second, _second_log) = Double::failing_open();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        let error = combined
            .open(Path::new("gcmon_trace.json"))
            .expect_err("the second member refuses to open");

        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(first_log.borrow().opened.len(), 1);
        assert_eq!(
            first_log.borrow().closed,
            1,
            "the member that opened is closed rather than left open"
        );
    }

    /// The reverse rule for `close`: returning on the first error would leave the later
    /// members' files unterminated, which is exactly what `close` exists to prevent.
    #[test]
    fn a_member_that_cannot_close_does_not_stop_the_others_closing() {
        let (first, first_log) = Double::failing_close("close refused");
        let (second, second_log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        let error = combined.close().expect_err("the first member refuses");

        assert_eq!(format!("{error}"), "close refused");
        assert_eq!(first_log.borrow().closed, 1);
        assert_eq!(
            second_log.borrow().closed,
            1,
            "a later member still terminates its own file"
        );
    }

    /// Two failures, and the operator is told about the first. Reporting the last instead
    /// would hand back whichever member happened to be registered later — an ordering the
    /// caller never chose and cannot see.
    #[test]
    fn the_first_close_failure_is_the_one_reported() {
        let (first, _) = Double::failing_close("the first refusal");
        let (second, _) = Double::failing_close("the second refusal");
        let mut combined = CombiningExporter::new(vec![Box::new(first), Box::new(second)]);

        let error = combined.close().expect_err("both members refuse");

        assert_eq!(format!("{error}"), "the first refusal");
    }

    /// It ships holding one, so the single-member case is the one that runs in production
    /// today: nothing is dropped and nothing is duplicated on the way through.
    #[test]
    fn holding_one_member_forwards_exactly_what_it_was_given() {
        let (only, log) = Double::new();
        let mut combined = CombiningExporter::new(vec![Box::new(only)]);

        combined.open(Path::new("gcmon_trace.json")).unwrap();
        combined.add_events(&[meta(11), instant(11, 700)]);
        combined.close().unwrap();

        assert_eq!(log.borrow().opened.len(), 1);
        assert_eq!(log.borrow().batches, vec![vec![meta(11), instant(11, 700)]]);
        assert_eq!(log.borrow().closed, 1);
    }

    /// The same events through a fan-out of one and through the encoder alone, compared as
    /// bytes. Every run now goes through the combiner, so an operator's trace must be the one
    /// they got before the flag existed — and a double cannot say that, only the real encoder
    /// can. `chrome`'s own golden test pins what those bytes are; this pins that the wrapper
    /// does not disturb them.
    #[test]
    fn a_fan_out_of_one_writes_what_the_encoder_alone_writes() {
        use crate::monitor::exporters::chrome::ChromeTraceExporter;
        use crate::monitor::trace_event::ArgValue;

        let events = vec![
            meta(11),
            TraceEvent::ThreadMeta {
                pid: 11,
                tid: 0,
                name: "Interpreter 0".to_string(),
            },
            TraceEvent::Begin {
                pid: 11,
                tid: 0,
                ts_ns: 20_100,
                name: "GC Pause".to_string(),
                cat: "gc".to_string(),
                args: vec![("collected", ArgValue::Int(3))],
            },
            TraceEvent::End {
                pid: 11,
                tid: 0,
                ts_ns: 20_900,
                name: "GC Pause".to_string(),
                cat: "gc".to_string(),
            },
            TraceEvent::Counter {
                pid: 11,
                tid: 0,
                ts_ns: 20_900,
                name: String::new(),
                args: vec![("heap_size", ArgValue::Int(2048))],
            },
            // Repeated metadata: dedup belongs to the member, so wrapping it must not move
            // where that happens.
            meta(11),
        ];

        let alone = write_and_read(&mut ChromeTraceExporter::new(), &events, "alone");
        let wrapped = write_and_read(
            &mut CombiningExporter::new(vec![Box::new(ChromeTraceExporter::new())]),
            &events,
            "wrapped",
        );

        assert_eq!(alone, wrapped);
    }

    /// Drive one exporter over a batch and give back the file it wrote. The tag keeps two
    /// exporters in one test off each other's path.
    fn write_and_read(
        exporter: &mut dyn EventsExporter,
        events: &[TraceEvent],
        tag: &str,
    ) -> String {
        let path = std::env::temp_dir().join(format!(
            "gcscope_combining_{}_{}.json",
            std::process::id(),
            tag
        ));
        exporter.open(&path).unwrap();
        exporter.add_events(events);
        exporter.close().unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        written
    }
}
