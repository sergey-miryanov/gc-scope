//! The Perfetto encoder: [`TraceEvent`]s in, a binary `.perfetto-trace` out.
//!
//! What a Collection looks like in a trace is decided in [`crate::monitor::convert`], as it is
//! for Chrome. This file knows only how Perfetto spells a trace: which field number carries
//! what, and that a track has to be declared before an event names it.
//!
//! ## What it writes today
//!
//! The root descriptor and one track per process, and nothing else. Spans, counters and
//! annotations arrive with the steps that decide how a Collection should look; until then a
//! [`TraceEvent`] that is not a [`TraceEvent::ProcessMeta`] is dropped rather than guessed at.
//! The point of this stage is the encoding machinery, checked against a decoder gcscope did
//! not write before any layout policy rides on it ([ADR
//! 0021](../../../docs/adr/0021-hand-rolled-perfetto-encoder.md)).
//!
//! ## The shape of the file
//!
//! A `Trace` is a repeated `TracePacket` and carries no header, length or index. The file is
//! therefore the concatenation of its packets, each written as one length-delimited `packet`
//! field, and an encoder can append as it goes and stop anywhere — a trace truncated by a
//! crash is the prefix that was written, still readable.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::perfetto_proto::{
    process_descriptor, process_ordering, thread_ordering, trace, trace_packet, track_descriptor,
};
use super::protobuf::{put_int32, put_message, put_uint64};
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
/// `ProcessDescriptor.pid` is an `int32`, so a pid that does not fit one cannot be spelled in
/// this format; the cast wraps rather than inventing a substitute. No OS gcscope attaches on
/// issues pids that large.
fn process_track(uuid: u64, pid: u32) -> Vec<u8> {
    let mut process = Vec::new();
    put_int32(&mut process, process_descriptor::PID, pid as i32);

    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, uuid);
    put_message(&mut descriptor, track_descriptor::PROCESS, &process);
    descriptor_packet(&descriptor)
}

pub struct PerfettoExporter {
    file: Option<File>,
    /// The track declared for each process, so a second event from one pid does not declare it
    /// again. Like Chrome's metadata dedup, this belongs to the output stream rather than to
    /// the conversion: a reopened file needs its descriptors written again.
    process_tracks: HashMap<u32, u64>,
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
    /// A pid is recorded as declared only *after* its descriptor reaches the file. Recording it
    /// first means a failed write is never retried: the run then finishes reporting a trace
    /// written, and every later event naming that uuid sits on a track nothing declared.
    fn track_for(&mut self, pid: u32) -> Option<u64> {
        if let Some(uuid) = self.process_tracks.get(&pid) {
            return Some(*uuid);
        }
        let uuid = self.next_uuid;
        self.file
            .as_mut()?
            .write_all(&process_track(uuid, pid))
            .ok()?;

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
        self.next_uuid = FIRST_TRACK_UUID;
        Ok(())
    }

    fn add_events(&mut self, events: &[TraceEvent]) {
        if self.file.is_none() {
            return;
        }
        for event in events {
            // Only the process tracks so far. The rest are dropped rather than encoded as
            // something plausible: a slice on a track nobody declared, or an annotation under
            // a field number nothing has checked, is worse than an absence a reader can see.
            if let TraceEvent::ProcessMeta { pid, .. } = event {
                self.track_for(*pid);
            }
        }
        if let Some(file) = self.file.as_mut() {
            file.flush().ok();
        }
    }

    fn mark_process_lifecycle(&mut self, _pid: u32, _kind: ProcessLifecycle, _ts_ns: i64) {}

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

    /// And one process track after it:
    ///   0a 0c                Trace.packet, 12 bytes
    ///     50 01              trusted_packet_sequence_id = 1
    ///     e2 03 07           track_descriptor, 7 bytes
    ///       08 01            uuid = 1
    ///       1a 03            .process (3), 3 bytes
    ///         08 92 21       ProcessDescriptor.pid (1) = 4242
    const ONE_PROCESS_BYTES: [u8; 14] = [
        0x0a, 0x0c, 0x50, 0x01, 0xe2, 0x03, 0x07, 0x08, 0x01, 0x1a, 0x03, 0x08, 0x92, 0x21,
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

    /// A Perfetto trace has no liveness track gcscope fills yet, so the mark is swallowed the
    /// way Chrome swallows it. Pinned rather than assumed: an unexpected packet here would
    /// carry a timestamp nothing supplies.
    #[test]
    fn process_lifecycle_marks_write_nothing() {
        let path = temp_path();
        let mut exporter = PerfettoExporter::new();
        exporter.open(&path).unwrap();
        exporter.mark_process_lifecycle(7, ProcessLifecycle::Started, 1_000);
        exporter.mark_process_lifecycle(7, ProcessLifecycle::Died, 2_000);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_lengths(&bytes).expect("whole packets").len(), 1);
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
