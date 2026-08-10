//! The Perfetto encoder: [`TraceEvent`]s in, a binary `.perfetto-trace` out.
//!
//! What a Collection looks like in a trace is decided in [`crate::monitor::convert`], as it is
//! for Chrome. This file knows only how Perfetto spells a trace: which field number carries
//! what, and that a track has to be declared before an event names it.
//!
//! ## What it writes today
//!
//! The root descriptor, one track per process, and the marker that keeps a process visible when
//! it never collects. Spans and counters arrive with the step that decides how a Collection
//! should look; until then a [`TraceEvent`] that is not a [`TraceEvent::ProcessMeta`] is dropped
//! rather than guessed at. The encoding machinery is checked against a decoder gcscope did not
//! write before any layout policy rides on it ([ADR
//! 0021](../../../docs/adr/0021-hand-rolled-perfetto-encoder.md)).
//!
//! ## Which clock
//!
//! Every timestamp here is the Observer's, and says so
//! ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)). The target's clock
//! arrives with the Collections, and the two cannot share a trace until one of them is declared
//! as a clock domain Perfetto can convert.
//!
//! ## The shape of the file
//!
//! A `Trace` is a repeated `TracePacket` and carries no header, length or index. The file is
//! therefore the concatenation of its packets, each written as one length-delimited `packet`
//! field, and an encoder can append as it goes and stop anywhere — a trace truncated by a
//! crash is the prefix that was written, still readable.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::perfetto_proto::{
    debug_annotation, process_descriptor, process_ordering, thread_ordering, trace, trace_packet,
    track_descriptor, track_event, track_event_type,
};
use super::protobuf::{put_int32, put_message, put_string, put_uint64};
use super::{EventsExporter, ProcessLifecycle};
use crate::monitor::trace_event::TraceEvent;

/// The extension a Perfetto trace takes, and what [`EventsExporter::output_path`] resolves an
/// operator's `--output` to. `ui.perfetto.dev` reads a trace by content rather than by name,
/// so this is for the operator: a `.json` holding protobuf is a file nobody can open twice.
pub const EXTENSION: &str = "perfetto-trace";

/// The track every top-level descriptor is ordered under. Perfetto reserves uuid 0 for it.
const ROOT_TRACK_UUID: u64 = 0;

/// The first uuid handed out to a real track. Anything but 0, which the root holds.
const FIRST_TRACK_UUID: u64 = 1;

/// Every packet gcscope writes belongs to one sequence, and a trace has exactly one producer,
/// so the id is a constant rather than a value drawn from the address of something. Perfetto
/// drops packets that name no sequence at all — interning and incremental state are
/// sequence-scoped — and a fixed value keeps two runs of the same input byte-identical.
const SEQUENCE_ID: u64 = 1;

/// What the marker that keeps a silent process visible is called.
const START_PROCESS: &str = "Start Process";

/// The annotation key naming the clock an event's timestamp is on, and the one value gcscope
/// writes today. Every timestamp gcscope generates itself is on this clock; the ones CPython
/// publishes are on the target's, and belong to a later step.
const CLOCK_ANNOTATION: &str = "clock";
const OBSERVER_CLOCK: &str = "gcscope observer, ns since the run began";

/// One `TracePacket`, framed as the `Trace.packet` field that holds it.
fn packet(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_message(&mut out, trace::PACKET, body);
    out
}

/// A packet carrying one `TrackDescriptor`.
///
/// Fields go out in ascending number, which is protobuf's canonical order and not something
/// the format requires. It is a decision so that one input produces one byte sequence.
fn descriptor_packet(descriptor: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    put_uint64(
        &mut body,
        trace_packet::TRUSTED_PACKET_SEQUENCE_ID,
        SEQUENCE_ID,
    );
    put_message(&mut body, trace_packet::TRACK_DESCRIPTOR, descriptor);
    packet(&body)
}

/// The uuid-0 root descriptor.
///
/// **This is what makes explicit process and thread ordering possible at all.** The
/// `sibling_order_rank` on a top-level process or thread track is honoured only when the root
/// track says the ordering is explicit, so every later descriptor that wants a place in the
/// tree hangs off this one packet. It carries no name and no `process`/`thread`/`counter`
/// sub-message, so trace-processor does not surface it as a track row.
///
/// The hints are written unconditionally, with no version gate: a reader that does not
/// understand them ignores them and falls back to its own ordering, so the trace is
/// forward-compatible and nothing is lost on an older one.
fn root_descriptor() -> Vec<u8> {
    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, ROOT_TRACK_UUID);
    put_uint64(
        &mut descriptor,
        track_descriptor::PROCESS_ORDERING,
        u64::from(process_ordering::EXPLICIT),
    );
    put_uint64(
        &mut descriptor,
        track_descriptor::THREAD_ORDERING,
        u64::from(thread_ordering::EXPLICIT),
    );
    descriptor_packet(&descriptor)
}

/// A top-level track for one process.
///
/// The `name` is the track's title and `process.process_name` is what the query engine reports
/// in its `process` table. Both, because they are separate fields answering separate questions
/// and a reader looking for one does not find the other.
///
/// `rank` orders this process against its siblings, honoured because the root descriptor said
/// the ordering is explicit. Without it the tree reads in whatever order gcscope happened to
/// see the processes in, which is a property of the poll rather than of the program.
///
/// **No `start_timestamp_ns`.** The field wants the moment the process began, on the target's
/// clock, and gcscope has no reading on that clock — it attached partway through a program
/// already running. Writing the attach instead would date a process to when it was noticed
/// ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)).
///
/// `ProcessDescriptor.pid` is an `int32`, so a pid that does not fit one cannot be spelled in
/// this format; the cast wraps rather than inventing a substitute. No OS gcscope attaches on
/// issues pids that large.
fn process_track(uuid: u64, pid: u32, name: Option<&str>, rank: i32) -> Vec<u8> {
    let mut process = Vec::new();
    put_int32(&mut process, process_descriptor::PID, pid as i32);
    if let Some(name) = name {
        put_string(&mut process, process_descriptor::PROCESS_NAME, name);
    }

    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, uuid);
    if let Some(name) = name {
        put_string(&mut descriptor, track_descriptor::NAME, name);
    }
    put_message(&mut descriptor, track_descriptor::PROCESS, &process);
    put_int32(&mut descriptor, track_descriptor::SIBLING_ORDER_RANK, rank);
    descriptor_packet(&descriptor)
}

/// The zero-duration instant that makes a process's track visible.
///
/// **Perfetto's UI hides a track with no events on it.** A process that never collects has no
/// events at all, so without this marker the operator's worker is missing from the trace they
/// opened to find out whether it collected — and "it did none" is a different answer from "it
/// is not there". One per process, on the process track itself.
///
/// The timestamp is the Observer's: the moment gcscope attached, not anything the target did.
/// The annotation says so, because this is the one event in the trace on that clock and a
/// number with no clock beside it invites being read against the ones around it
/// ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)).
fn start_process_instant(uuid: u64, ts_ns: u64) -> Vec<u8> {
    let mut clock = Vec::new();
    put_string(&mut clock, debug_annotation::NAME, CLOCK_ANNOTATION);
    put_string(&mut clock, debug_annotation::STRING_VALUE, OBSERVER_CLOCK);

    let mut event = Vec::new();
    put_message(&mut event, track_event::DEBUG_ANNOTATIONS, &clock);
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::INSTANT),
    );
    put_uint64(&mut event, track_event::TRACK_UUID, uuid);
    put_string(&mut event, track_event::NAME, START_PROCESS);

    let mut body = Vec::new();
    put_uint64(&mut body, trace_packet::TIMESTAMP, ts_ns);
    put_uint64(
        &mut body,
        trace_packet::TRUSTED_PACKET_SEQUENCE_ID,
        SEQUENCE_ID,
    );
    put_message(&mut body, trace_packet::TRACK_EVENT, &event);
    packet(&body)
}

pub struct PerfettoExporter {
    file: Option<File>,
    /// The track declared for each process, so a second event from one pid does not declare it
    /// again. Like Chrome's metadata dedup, this belongs to the output stream rather than to
    /// the conversion: a reopened file needs its descriptors written again.
    process_tracks: HashMap<u32, u64>,
    /// What each process is called, kept apart from the track it names. The announcement that
    /// carries a name arrives once per process, so a descriptor written *without* one — a start
    /// mark that beat the announcement, or a first write that failed and was retried — would be
    /// nameless for the rest of the run if the name were not held here. Survives `open`, since
    /// a reopened file re-declares the same tracks and wants the same names.
    process_names: HashMap<u32, String>,
    /// Processes whose visibility marker has gone out. Idempotent per process, and kept apart
    /// from `process_tracks` because the two are set by different signals: the announcement
    /// declares a track without marking it, and a second start mark for one process must not
    /// draw a second marker.
    marked_started: HashSet<u32>,
    next_uuid: u64,
}

/// Hand-written rather than derived, for one field: a derived `Default` starts `next_uuid` at
/// `0`, the uuid the root descriptor holds. `open` sets it too, so the derive would be wrong
/// only in a window nothing reaches today — which is exactly the kind of invariant that stops
/// holding when someone adds a call site.
impl Default for PerfettoExporter {
    fn default() -> Self {
        Self {
            file: None,
            process_tracks: HashMap::new(),
            process_names: HashMap::new(),
            marked_started: HashSet::new(),
            next_uuid: FIRST_TRACK_UUID,
        }
    }
}

impl PerfettoExporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a track for `pid` if it has none, and return its uuid.
    ///
    /// The name comes from whatever the announcement left in `process_names`, so a track
    /// declared by something that does not carry one — a lifecycle mark arriving first — still
    /// gets the name once it is known. A track declared before any announcement is nameless
    /// rather than named a guess, and the pid still identifies it.
    ///
    /// A pid is recorded as declared only *after* its descriptor reaches the file. Recording it
    /// first means a failed write is never retried: the run then finishes reporting a trace
    /// written, and every later event naming that uuid sits on a track nothing declared.
    fn track_for(&mut self, pid: u32) -> Option<u64> {
        if let Some(uuid) = self.process_tracks.get(&pid) {
            return Some(*uuid);
        }
        let uuid = self.next_uuid;
        // The rank is the order gcscope met the processes, which for a tree is the target and
        // then its workers as they appeared. Assigned as each is declared rather than sorted at
        // the end, because this encoder writes as it goes and never rewrites a descriptor.
        let rank = self.process_tracks.len() as i32;
        let descriptor = process_track(
            uuid,
            pid,
            self.process_names.get(&pid).map(String::as_str),
            rank,
        );

        self.file.as_mut()?.write_all(&descriptor).ok()?;
        self.next_uuid += 1;
        self.process_tracks.insert(pid, uuid);
        Some(uuid)
    }
}

impl EventsExporter for PerfettoExporter {
    /// `.json` is the one extension this format must not keep — a file holding protobuf under
    /// it is one nobody opens twice — so that one is replaced. Anything else is **appended**
    /// to rather than replaced: `-o capture.2026-08-09` is a name an operator chose, not a
    /// claim about the format, and replacing it would fold two days' captures into one file.
    fn output_path(&self, requested: &Path) -> PathBuf {
        let extension = requested
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();

        let mut path = requested.to_path_buf();
        if extension.eq_ignore_ascii_case(EXTENSION) {
            return path;
        }
        if extension.eq_ignore_ascii_case("json") {
            path.set_extension(EXTENSION);
            return path;
        }

        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".");
        name.push(EXTENSION);
        path.set_file_name(name);
        path
    }

    fn open(&mut self, path: &Path) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;

        // Written here rather than before the first event, so a run that saw nothing still
        // produces a trace a reader opens instead of a zero-byte file.
        file.write_all(&root_descriptor())?;
        file.flush()?;

        self.file = Some(file);
        self.process_tracks.clear();
        self.marked_started.clear();
        self.next_uuid = FIRST_TRACK_UUID;
        Ok(())
    }

    /// No guard on the file here. `track_for` writes nothing and consumes nothing without one,
    /// and the name below has to be recorded either way: a batch that arrives before `open` is
    /// dropped, but the announcement it carried is the only one this process gets.
    fn add_events(&mut self, events: &[TraceEvent]) {
        for event in events {
            // Only the process tracks so far. The rest are dropped rather than encoded as
            // something plausible: a slice on a track nobody declared, or an annotation under
            // a field number nothing has checked, is worse than an absence a reader can see.
            if let TraceEvent::ProcessMeta { pid, name } = event {
                // Remembered before the write and kept whether or not it succeeds. A descriptor
                // that has to be written again — no file yet, or a write that failed — must not
                // lose the name with it, because no second announcement is coming.
                self.process_names.insert(*pid, name.clone());
                self.track_for(*pid);
            }
        }
        if let Some(file) = self.file.as_mut() {
            file.flush().ok();
        }
    }

    /// `Started` draws the marker that keeps the process visible; `Died` draws nothing yet.
    ///
    /// Without a reading there is no marker. An instant has to sit somewhere on the timeline,
    /// and a zero puts it at the origin — decades from the rest of the trace, and read as a
    /// measurement by anyone who does not know it was a placeholder.
    fn mark_process_lifecycle(&mut self, pid: u32, kind: ProcessLifecycle, ts_ns: Option<i64>) {
        let (ProcessLifecycle::Started, Some(ts_ns)) = (kind, ts_ns) else {
            return;
        };
        // `TracePacket.timestamp` is a `uint64`. A negative reading is not a point on this
        // timeline — written out it sign-extends to roughly 584 years — so it is refused here
        // rather than placed somewhere absurd, which is the whole of ADR 0022.
        let Ok(ts_ns) = u64::try_from(ts_ns) else {
            return;
        };
        // The track first: the mark can arrive before the announcement that names the process,
        // and an event on a track nothing declared is what the harness's checks reject.
        let Some(uuid) = self.track_for(pid) else {
            return;
        };
        if self.marked_started.contains(&pid) {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(&start_process_instant(uuid, ts_ns)).is_ok() {
            self.marked_started.insert(pid);
        }
        file.flush().ok();
    }

    fn close(&mut self) -> std::io::Result<()> {
        // Nothing terminates a Perfetto trace — the file is its packets — so closing is
        // flushing and letting the handle go. `run_loop` unwinds through `close` on paths
        // where `open` never ran, and closing twice has to be as harmless.
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "gcscope_perfetto_{}_{}.perfetto-trace",
            std::process::id(),
            n
        ));
        path
    }

    fn export(events: &[TraceEvent]) -> Vec<u8> {
        let path = temp_path();
        let mut exporter = PerfettoExporter::new();
        exporter.open(&path).unwrap();
        exporter.add_events(events);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();
        bytes
    }

    fn process_meta(pid: u32) -> TraceEvent {
        TraceEvent::ProcessMeta {
            pid,
            name: format!("python {pid}"),
        }
    }

    /// Whether these bytes are a sequence of complete `Trace.packet` fields and nothing else.
    /// A cheap structural proxy for the real decoder in `tests/perfetto_encoder.rs`: it catches
    /// a length prefix that disagrees with what follows it, which is how a truncated or
    /// double-counted submessage shows up.
    fn packet_lengths(bytes: &[u8]) -> Option<Vec<usize>> {
        let mut lengths = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            // Field 1, wire type 2 — the only field a `Trace` has.
            if rest[0] != 0x0a {
                return None;
            }
            let mut length = 0usize;
            let mut shift = 0;
            let mut index = 1;
            loop {
                let byte = *rest.get(index)?;
                length |= usize::from(byte & 0x7f) << shift;
                index += 1;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            if rest.len() < index + length {
                return None;
            }
            lengths.push(length);
            rest = &rest[index + length..];
        }
        Some(lengths)
    }

    /// A run that observed nothing still writes the root descriptor, so the file is a trace
    /// rather than zero bytes. Chrome's equivalent is the empty `[]`.
    #[test]
    fn a_trace_with_no_events_is_still_the_root_descriptor() {
        let bytes = export(&[]);
        assert_eq!(packet_lengths(&bytes), Some(vec![bytes.len() - 2]));
    }

    #[test]
    fn each_process_is_declared_once_however_often_it_reports() {
        let bytes = export(&[process_meta(4242), process_meta(4242), process_meta(7)]);
        // The root, and one descriptor per distinct pid.
        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 3);
    }

    /// Uuid 0 belongs to the root, so a process track must never be handed it: a descriptor
    /// claiming 0 overwrites the ordering hints and every event pointing at track 0 becomes
    /// ambiguous.
    #[test]
    fn no_process_track_claims_the_root_uuid() {
        let mut exporter = PerfettoExporter::new();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[process_meta(1), process_meta(2)]);
        exporter.close().unwrap();
        fs::remove_file(&path).ok();

        let mut uuids: Vec<u64> = exporter.process_tracks.values().copied().collect();
        uuids.sort_unstable();
        assert_eq!(uuids, [1, 2]);
    }

    /// Events arriving before `open` are dropped, and must not consume the pid's declaration:
    /// the monitor holds one exporter across a run, and a poll landing early would otherwise
    /// leave that process with no track for the rest of the capture.
    #[test]
    fn events_before_open_are_dropped_without_consuming_the_declaration() {
        let mut exporter = PerfettoExporter::new();
        exporter.add_events(&[process_meta(4242)]);

        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 2);
    }

    /// Reopening starts a new file, so every descriptor has to be written into it again.
    #[test]
    fn reopening_declares_the_processes_again() {
        let mut exporter = PerfettoExporter::new();

        let first = temp_path();
        exporter.open(&first).unwrap();
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        fs::remove_file(&first).ok();

        let second = temp_path();
        exporter.open(&second).unwrap();
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        let bytes = fs::read(&second).unwrap();
        fs::remove_file(&second).ok();

        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 2);
    }

    #[test]
    fn closing_an_unopened_exporter_is_a_clean_no_op() {
        PerfettoExporter::new().close().unwrap();

        let path = temp_path();
        let mut exporter = PerfettoExporter::new();
        exporter.open(&path).unwrap();
        exporter.close().unwrap();
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 1);
    }

    fn resolve(input: &str) -> String {
        PerfettoExporter::new()
            .output_path(Path::new(input))
            .to_string_lossy()
            .into_owned()
    }

    /// The `--output` rule, stated once here and in `--help`.
    #[test]
    fn the_output_path_takes_the_formats_own_extension() {
        assert_eq!(resolve("gcmon_trace.json"), "gcmon_trace.perfetto-trace");
        assert_eq!(resolve("capture"), "capture.perfetto-trace");
        assert_eq!(resolve("run.perfetto-trace"), "run.perfetto-trace");
        // Only the last extension is replaced, so a dotted name keeps the rest of itself.
        assert_eq!(resolve("run.2.json"), "run.2.perfetto-trace");
        // Whichever way the operator typed it.
        assert_eq!(resolve("RUN.JSON"), "RUN.perfetto-trace");
    }

    /// A suffix that is not a format claim is kept, because replacing it turns two captures an
    /// operator deliberately named apart into one file that silently overwrites the other.
    #[test]
    fn a_suffix_that_is_not_json_is_appended_to_rather_than_replaced() {
        assert_eq!(
            resolve("capture.2026-08-09"),
            "capture.2026-08-09.perfetto-trace"
        );
        assert_ne!(resolve("capture.2026-08-09"), resolve("capture.2026-08-10"));
        assert_eq!(resolve("run.pid1234"), "run.pid1234.perfetto-trace");
    }

    /// A pid is declared only once its descriptor is in the file. Recorded first, a write that
    /// failed would never be retried and the run would still report a trace written — with
    /// every later event naming a track nothing declared.
    ///
    /// Reached through the branch a test can reach: no file. A real write failure needs a full
    /// volume, and the ordering it turns on is the same one.
    #[test]
    fn a_process_is_recorded_only_once_its_descriptor_is_written() {
        let mut exporter = PerfettoExporter::new();

        assert_eq!(exporter.track_for(4242), None);

        assert!(exporter.process_tracks.is_empty(), "the pid was consumed");
        assert_eq!(
            exporter.next_uuid, FIRST_TRACK_UUID,
            "a uuid was spent on a descriptor nobody wrote"
        );
    }

    /// A fresh exporter must not have the root's uuid queued up as the first one it hands out.
    #[test]
    fn a_new_exporter_does_not_start_at_the_root_uuid() {
        assert_ne!(PerfettoExporter::new().next_uuid, ROOT_TRACK_UUID);
    }

    // ---------------------------------------------------------------------------------
    // Byte-for-byte gate.
    //
    // Ascending field order and a fixed sequence id are decisions this file states, and they
    // are what makes one input produce one byte sequence. Nothing else pins them: the tests
    // above count packets and check length prefixes, both of which survive a reordering. This
    // is the Perfetto counterpart of the gate at the bottom of `chrome.rs` — do not regenerate
    // either one to make a build pass. A format change rewrites it in the commit that changes
    // the format.
    //
    // What the bytes say, read left to right:
    //   0a 0d                Trace.packet (field 1, length-delimited), 13 bytes
    //     50 01              TracePacket.trusted_packet_sequence_id (10) = 1
    //     e2 03 08           TracePacket.track_descriptor (60), 8 bytes
    //       08 00            TrackDescriptor.uuid (1) = 0, the root
    //       98 01 01         .process_ordering (19) = PROCESS_ORDERING_EXPLICIT
    //       a0 01 01         .thread_ordering (20) = THREAD_ORDERING_EXPLICIT
    // ---------------------------------------------------------------------------------

    const ROOT_DESCRIPTOR_BYTES: [u8; 15] = [
        0x0a, 0x0d, 0x50, 0x01, 0xe2, 0x03, 0x08, 0x08, 0x00, 0x98, 0x01, 0x01, 0xa0, 0x01, 0x01,
    ];

    /// And one process track after it. `python 4242` is 11 bytes and appears twice, once as
    /// the track's title and once as the name the query engine reports:
    ///   0a 28                Trace.packet, 40 bytes
    ///     50 01              trusted_packet_sequence_id = 1
    ///     e2 03 23           track_descriptor, 35 bytes
    ///       08 01            uuid = 1
    ///       12 0b …          .name (2) = "python 4242"
    ///       1a 10            .process (3), 16 bytes
    ///         08 92 21       ProcessDescriptor.pid (1) = 4242
    ///         32 0b …        .process_name (6) = "python 4242"
    ///       60 00            .sibling_order_rank (12) = 0, the first process met
    const ONE_PROCESS_BYTES: [u8; 42] = [
        0x0a, 0x28, 0x50, 0x01, 0xe2, 0x03, 0x23, 0x08, 0x01, 0x12, 0x0b, b'p', b'y', b't', b'h',
        b'o', b'n', b' ', b'4', b'2', b'4', b'2', 0x1a, 0x10, 0x08, 0x92, 0x21, 0x32, 0x0b, b'p',
        b'y', b't', b'h', b'o', b'n', b' ', b'4', b'2', b'4', b'2', 0x60, 0x00,
    ];

    #[test]
    fn an_empty_traces_bytes_are_unchanged() {
        assert_eq!(export(&[]), ROOT_DESCRIPTOR_BYTES);
    }

    #[test]
    fn a_one_process_traces_bytes_are_unchanged() {
        let expected: Vec<u8> = ROOT_DESCRIPTOR_BYTES
            .iter()
            .chain(&ONE_PROCESS_BYTES)
            .copied()
            .collect();
        assert_eq!(export(&[process_meta(4242)]), expected);
    }

    /// Drive the lifecycle marks alone and give back the file. What a process gcscope attached
    /// to but never read a Record from produces — the silent worker of user story 4.
    fn export_lifecycle(marks: &[(u32, ProcessLifecycle, Option<i64>)]) -> Vec<u8> {
        let path = temp_path();
        let mut exporter = PerfettoExporter::new();
        exporter.open(&path).unwrap();
        for (pid, kind, ts_ns) in marks {
            exporter.mark_process_lifecycle(*pid, *kind, *ts_ns);
        }
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();
        bytes
    }

    /// A process that produced no Record still reaches the trace: its track is declared and the
    /// marker that stops the UI hiding an empty track goes on it. Without this, the answer to
    /// "did my worker collect anything" is a missing process rather than a quiet one.
    #[test]
    fn a_process_that_only_ever_started_is_still_drawn() {
        let bytes = export_lifecycle(&[(7, ProcessLifecycle::Started, Some(1_000))]);
        // The root, the track, and the marker on it.
        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 3);
    }

    /// Once per process, however many marks arrive, and `Died` draws nothing of its own yet.
    #[test]
    fn a_second_mark_for_one_process_draws_nothing_more() {
        let bytes = export_lifecycle(&[
            (7, ProcessLifecycle::Started, Some(1_000)),
            (7, ProcessLifecycle::Started, Some(1_500)),
            (7, ProcessLifecycle::Died, Some(2_000)),
        ]);
        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 3);
    }

    /// No reading, no marker. An instant has to sit somewhere on the timeline, and a zero puts
    /// it at the origin — decades from everything else, and indistinguishable from a
    /// measurement. The track is still declared, so the process is not lost.
    #[test]
    fn a_start_with_no_reading_draws_no_marker() {
        let bytes = export_lifecycle(&[(7, ProcessLifecycle::Started, None)]);
        assert_eq!(
            packet_lengths(&bytes).expect("whole packets").len(),
            1,
            "the root alone"
        );
    }

    /// A reading that cannot be a point on this timeline draws no marker. `TracePacket.timestamp`
    /// is a `uint64`, so a negative sign-extends to roughly 584 years out — a number that looks
    /// like a measurement and is not one.
    #[test]
    fn a_negative_reading_draws_no_marker() {
        let bytes = export_lifecycle(&[(7, ProcessLifecycle::Started, Some(-1))]);
        assert_eq!(
            packet_lengths(&bytes).expect("whole packets").len(),
            1,
            "the root alone"
        );
    }

    /// The announcement carries the name once per process. A descriptor written before it, or
    /// written again after a failure, takes the name from what the announcement left behind —
    /// otherwise a track that missed it stays nameless for the whole run.
    #[test]
    fn a_track_written_after_the_announcement_still_carries_the_name() {
        let mut exporter = PerfettoExporter::new();
        // The announcement lands while there is no file, so the descriptor is not written and
        // the pid stays undeclared — the retry path, reached without a full disk.
        exporter.add_events(&[process_meta(4242)]);
        assert!(exporter.process_tracks.is_empty());

        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.mark_process_lifecycle(4242, ProcessLifecycle::Started, Some(1_000));
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert!(
            bytes.windows(11).any(|w| w == b"python 4242"),
            "the track lost its name: {bytes:?}"
        );
    }

    /// The announcement names the process; a mark arriving first does not know the name. Either
    /// way the track is declared once, and the pid identifies it.
    #[test]
    fn a_mark_before_the_announcement_declares_the_track_once() {
        let path = temp_path();
        let mut exporter = PerfettoExporter::new();
        exporter.open(&path).unwrap();
        exporter.mark_process_lifecycle(4242, ProcessLifecycle::Started, Some(1_000));
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        // Root, the nameless track, the marker. The announcement adds nothing: the track it
        // would declare already exists, and this encoder does not rewrite a descriptor.
        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 3);
        assert_eq!(exporter.process_tracks.len(), 1);
    }

    /// Nothing a producer can put in a `TraceEvent` reaches the file yet except a process, and
    /// a later step must not turn that into a half-encoded span by accident. The assertion is
    /// on the packet count, so an event that starts being written fails here first.
    #[test]
    fn events_this_stage_does_not_encode_write_nothing() {
        let bytes = export(&[
            TraceEvent::ThreadMeta {
                pid: 1,
                tid: 0,
                name: "1:0".to_string(),
            },
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 1_000,
                name: "GC Pause".to_string(),
                cat: "gc".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 2_000,
                name: "GC Pause".to_string(),
                cat: "gc".to_string(),
            },
            TraceEvent::Instant {
                pid: 1,
                ts_ns: 3_000,
                name: "checkpoint".to_string(),
            },
            TraceEvent::Counter {
                pid: 1,
                tid: 0,
                ts_ns: 4_000,
                name: "G0".to_string(),
                args: vec![],
            },
        ]);
        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 1);
    }
}
