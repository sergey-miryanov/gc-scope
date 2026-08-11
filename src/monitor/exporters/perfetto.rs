//! The Perfetto encoder: [`TraceEvent`]s in, a binary `.perfetto-trace` out.
//!
//! What a Collection looks like in a trace is decided in [`crate::monitor::convert`], as it is
//! for Chrome. This file knows only how Perfetto spells a trace: which field number carries
//! what, and that a track has to be declared before an event names it.
//!
//! ## What it writes today
//!
//! The clock snapshot, the root descriptor, one track per process and one per interpreter under
//! it, the marker that keeps a process visible when it never collects, and a slice per
//! Collection and per sub-phase. Counters arrive with the step that groups them; until then a
//! [`TraceEvent::Counter`] is dropped rather than guessed at. The encoding machinery is checked
//! against a decoder gcscope did not write before any layout policy rides on it ([ADR
//! 0021](../../../docs/adr/0021-hand-rolled-perfetto-encoder.md)).
//!
//! ## Which clock
//!
//! One, and the trace says which ([ADR
//! 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)). A Collection's timestamps
//! are what CPython published, on the machine's monotonic clock; gcscope reads that same clock
//! for itself ([`crate::monitor::clock`]), so an Observer reading is a point in the same domain
//! rather than a domain of its own. Every packet carrying a timestamp names that domain, and one
//! [`ClockSnapshot`](clock_snapshot_packet) ties it to the clock Perfetto measures trace time in.
//! The numbers CPython published reach the file unchanged; the conversion is the reader's.
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
    builtin_clock, child_tracks_ordering, clock, clock_snapshot, debug_annotation,
    process_descriptor, process_ordering, thread_ordering, trace, trace_packet, track_descriptor,
    track_event, track_event_type,
};
use super::protobuf::{put_double, put_int32, put_int64, put_message, put_string, put_uint64};
use super::{EventsExporter, ProcessLifecycle};
use crate::monitor::clock::RunClock;
use crate::monitor::trace_event::{Arg, ArgValue, TraceEvent};

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

/// gcscope's clock domain: the machine's monotonic counter, the one CPython times a Collection
/// with. Perfetto keeps 1..=63 for the clocks it knows itself
/// ([`builtin_clock::MAX_ID`]) and reads 64..=127 as scoped to the sequence that declared them,
/// which is exactly what this is — gcscope writes one sequence, and the snapshot naming this id
/// travels on it.
const GCSCOPE_CLOCK_ID: u32 = 64;

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

/// A packet carrying one `TrackEvent` at `ts_ns` on gcscope's clock domain.
///
/// The timestamp goes on the **packet**, never inside the `TrackEvent`: field 1 there is
/// `timestamp_delta_us`, so an absolute nanosecond stamp written into the event decodes as a
/// microsecond delta and lands somewhere else entirely. That mistake cost gcmon a shipped bug
/// and is the reason `trace_packet::TIMESTAMP` carries the warning it does.
///
/// The domain is named on every one of these rather than left to the trace's default, which is
/// BOOTTIME — a claim gcscope cannot make about a counter it read out of CPython.
fn event_packet(ts_ns: u64, event: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    put_uint64(&mut body, trace_packet::TIMESTAMP, ts_ns);
    put_uint64(
        &mut body,
        trace_packet::TRUSTED_PACKET_SEQUENCE_ID,
        SEQUENCE_ID,
    );
    put_message(&mut body, trace_packet::TRACK_EVENT, event);
    put_uint64(
        &mut body,
        trace_packet::TIMESTAMP_CLOCK_ID,
        u64::from(GCSCOPE_CLOCK_ID),
    );
    packet(&body)
}

/// The packet that makes every other timestamp in the trace mean something.
///
/// It publishes one instant twice: on the clock gcscope stamps its packets with, and on the one
/// Perfetto measures trace time in. Without it a reader has no path from gcscope's domain to its
/// own, and trace-processor does not fall back to guessing — it reports
/// `clock_sync_failure_no_path` and **drops every event**. That is also why the second reading is
/// BOOTTIME and not the more obvious MONOTONIC or REALTIME: those were tried, and both leave the
/// same gap ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)).
///
/// `primary_trace_clock` is left unset, so trace time stays the default, which is the BOOTTIME
/// this anchors against.
fn clock_snapshot_packet(run: &RunClock) -> Vec<u8> {
    let mut boot = Vec::new();
    put_uint64(
        &mut boot,
        clock::CLOCK_ID,
        u64::from(builtin_clock::BOOTTIME),
    );
    put_uint64(&mut boot, clock::TIMESTAMP, run.boottime_ns());

    let mut monotonic = Vec::new();
    put_uint64(&mut monotonic, clock::CLOCK_ID, u64::from(GCSCOPE_CLOCK_ID));
    // A monotonic reading is never negative on any platform gcscope runs on, and a wrapped one
    // would place the whole trace 584 years out rather than fail — so it is clamped rather than
    // cast.
    put_uint64(
        &mut monotonic,
        clock::TIMESTAMP,
        u64::try_from(run.monotonic_ns()).unwrap_or(0),
    );

    let mut snapshot = Vec::new();
    put_message(&mut snapshot, clock_snapshot::CLOCKS, &boot);
    put_message(&mut snapshot, clock_snapshot::CLOCKS, &monotonic);

    let mut body = Vec::new();
    put_message(&mut body, trace_packet::CLOCK_SNAPSHOT, &snapshot);
    put_uint64(
        &mut body,
        trace_packet::TRUSTED_PACKET_SEQUENCE_ID,
        SEQUENCE_ID,
    );
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
    // A child's `sibling_order_rank` counts only where its **parent** says the ordering is
    // explicit, the same bargain the root descriptor strikes for the process tracks under it.
    // Without this the ranks on the interpreter tracks are inert and Perfetto falls back to its
    // own rule. Written unconditionally: a reader that does not understand the hint ignores it.
    put_uint64(
        &mut descriptor,
        track_descriptor::CHILD_ORDERING,
        u64::from(child_tracks_ordering::EXPLICIT),
    );
    put_int32(&mut descriptor, track_descriptor::SIBLING_ORDER_RANK, rank);
    descriptor_packet(&descriptor)
}

/// A track for one interpreter, hanging off its process's.
///
/// **No `thread` sub-message, and that is the whole decision here.** gcscope draws one track per
/// interpreter, and an interpreter id is not an OS thread id — a `ThreadDescriptor` would put it
/// in trace-processor's `thread` table as a thread with that tid, which is a claim about the
/// target that is not true and which collides with a real thread of that number the day this
/// trace is merged with anything else. A plain child track says only what gcscope knows: this
/// belongs to that process, and it is called this. Perfetto also ignores `sibling_order_rank` on
/// OS-scoped tracks, so the rank below would have been a no-op on one.
///
/// `rank` is the order gcscope met the interpreters within this process, so the tree reads the
/// way the program ran rather than the way a poll happened to return.
fn interpreter_track(uuid: u64, parent_uuid: u64, name: Option<&str>, rank: i32) -> Vec<u8> {
    let mut descriptor = Vec::new();
    put_uint64(&mut descriptor, track_descriptor::UUID, uuid);
    if let Some(name) = name {
        put_string(&mut descriptor, track_descriptor::NAME, name);
    }
    put_uint64(&mut descriptor, track_descriptor::PARENT_UUID, parent_uuid);
    put_int32(&mut descriptor, track_descriptor::SIBLING_ORDER_RANK, rank);
    descriptor_packet(&descriptor)
}

/// One event argument as a debug annotation.
///
/// The kinds are kept apart: an `Int` goes to `int_value` and a `Float` to `double_value`,
/// rather than both to `string_value`. That is what lets a reader sum a count or plot a duration
/// in trace-processor instead of parsing text back into a number — and what makes `duration`,
/// which is a float of seconds, survive the trip as one.
///
/// `name` is field **10**, not field 1. Field 1 of that `oneof` is the interned id, so a string
/// written there decodes as a garbage IID; see [`debug_annotation::NAME`].
fn annotation(&(key, value): &Arg) -> Vec<u8> {
    let mut out = Vec::new();
    match value {
        ArgValue::Int(v) => put_int64(&mut out, debug_annotation::INT_VALUE, v),
        ArgValue::Float(v) => put_double(&mut out, debug_annotation::DOUBLE_VALUE, v),
    }
    put_string(&mut out, debug_annotation::NAME, key);
    out
}

/// The opening half of a slice: what it is called, what it is categorised as, and everything the
/// conversion decided to carry with it.
///
/// The name, the category and the argument keys all come from
/// [`crate::monitor::convert`]. Nothing is renamed on the way out, so the two formats describe
/// one run the same way — which is the whole reason that conversion is not per format.
fn slice_begin(uuid: u64, ts_ns: u64, name: &str, cat: &str, args: &[Arg]) -> Vec<u8> {
    let mut event = Vec::new();
    for arg in args {
        put_message(&mut event, track_event::DEBUG_ANNOTATIONS, &annotation(arg));
    }
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::SLICE_BEGIN),
    );
    put_uint64(&mut event, track_event::TRACK_UUID, uuid);
    put_string(&mut event, track_event::CATEGORIES, cat);
    put_string(&mut event, track_event::NAME, name);
    event_packet(ts_ns, &event)
}

/// The closing half: the track, and that it closes.
///
/// **A `SLICE_END` carries no name and no category** — Perfetto closes whatever is innermost on
/// the track. So the name and category an [`TraceEvent::End`] repeats have nowhere to go on the
/// wire, and the only thing an encoder can do with them is check them, which is exactly why the
/// event model repeats them.
fn slice_end(uuid: u64, ts_ns: u64) -> Vec<u8> {
    let mut event = Vec::new();
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::SLICE_END),
    );
    put_uint64(&mut event, track_event::TRACK_UUID, uuid);
    event_packet(ts_ns, &event)
}

/// The zero-duration instant that makes a process's track visible.
///
/// **Perfetto's UI hides a track with no events on it.** A process that never collects has no
/// events at all, so without this marker the operator's worker is missing from the trace they
/// opened to find out whether it collected — and "it did none" is a different answer from "it
/// is not there". One per process, on the process track itself.
///
/// The timestamp is when gcscope attached, not anything the target did — but it is on the same
/// clock a Collection's is, so the marker and the first slice after it sit where they belong
/// against each other. It used to carry a `clock` annotation saying which clock in prose; the
/// domain on the packet says it in a form a reader can convert, and a second statement that
/// cannot go stale in step with the first is worse than none
/// ([ADR 0022](../../../docs/adr/0022-every-timestamp-names-its-clock.md)).
fn start_process_instant(uuid: u64, ts_ns: u64) -> Vec<u8> {
    let mut event = Vec::new();
    put_uint64(
        &mut event,
        track_event::TYPE,
        u64::from(track_event_type::INSTANT),
    );
    put_uint64(&mut event, track_event::TRACK_UUID, uuid);
    put_string(&mut event, track_event::NAME, START_PROCESS);
    event_packet(ts_ns, &event)
}

/// A slice this exporter has opened on a track and not yet closed.
///
/// `name` and `cat` are kept only to check the [`TraceEvent::End`] that closes it against them;
/// neither reaches the wire, because a `SLICE_END` carries no name.
struct OpenSlice {
    name: String,
    cat: String,
    /// Whether the opening half reached the file. A begin gcscope could not write must not be
    /// closed by an end that does, or the end closes whatever is under it instead.
    written: bool,
}

pub struct PerfettoExporter {
    file: Option<File>,
    /// Where the run began on the machine's clock, and the same instant on Perfetto's. Held
    /// rather than read per event: an anchor that moved would be a second, disagreeing claim
    /// about where the trace sits.
    run: RunClock,
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
    /// The track declared for each interpreter, keyed by the process it belongs to and the id a
    /// [`TraceEvent`] carries as `tid`.
    interpreter_tracks: HashMap<(u32, i64), u64>,
    /// What each interpreter's track is called, kept apart from the track for the same reason
    /// the process names are: the announcement naming it arrives once, and a track declared
    /// before it — or after a write that failed — would otherwise stay nameless for the run.
    interpreter_names: HashMap<(u32, i64), String>,
    /// The slices open on each track, innermost last. Keyed by track uuid rather than by
    /// `(pid, tid)`, because nesting is a property of the track Perfetto closes against.
    open_slices: HashMap<u64, Vec<OpenSlice>>,
    /// How many `End`s did not match what was open. A count rather than a flag, so a run says
    /// how much of its trace it distrusts rather than only that something was wrong: the first
    /// is named as it happens and the total reported at `close`.
    mismatched_ends: usize,
    next_uuid: u64,
}

/// Hand-written rather than derived, for two fields. A derived `Default` starts `next_uuid` at
/// `0`, the uuid the root descriptor holds — `open` sets it too, so the derive would be wrong
/// only in a window nothing reaches today, which is exactly the kind of invariant that stops
/// holding when someone adds a call site. And the anchor is a reading of the machine, which
/// nothing can derive.
impl Default for PerfettoExporter {
    fn default() -> Self {
        Self::with_clock(crate::monitor::clock::run_clock())
    }
}

impl PerfettoExporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// An exporter anchored to a stated instant rather than to now.
    ///
    /// For the tests that pin bytes: a real anchor is two live readings, so a trace built on one
    /// is never twice the same and a byte-for-byte gate over it could only ever be regenerated.
    pub fn with_clock(run: RunClock) -> Self {
        Self {
            file: None,
            run,
            process_tracks: HashMap::new(),
            process_names: HashMap::new(),
            marked_started: HashSet::new(),
            interpreter_tracks: HashMap::new(),
            interpreter_names: HashMap::new(),
            open_slices: HashMap::new(),
            mismatched_ends: 0,
            next_uuid: FIRST_TRACK_UUID,
        }
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

    /// Declare a track for one interpreter of `pid` if it has none, and return its uuid.
    ///
    /// The process's track comes first, because this one hangs off it — a `parent_uuid` naming a
    /// track nobody declared is what the harness's checks reject.
    ///
    /// Recorded only once its descriptor reaches the file, for the same reason as
    /// [`track_for`](Self::track_for).
    fn interpreter_track_for(&mut self, pid: u32, tid: i64) -> Option<u64> {
        if let Some(uuid) = self.interpreter_tracks.get(&(pid, tid)) {
            return Some(*uuid);
        }
        let parent = self.track_for(pid)?;

        let uuid = self.next_uuid;
        // Ranked within the process, so two processes each start at zero and neither is ordered
        // against the other's interpreters.
        let rank = self
            .interpreter_tracks
            .keys()
            .filter(|(owner, _)| *owner == pid)
            .count() as i32;
        let descriptor = interpreter_track(
            uuid,
            parent,
            self.interpreter_names.get(&(pid, tid)).map(String::as_str),
            rank,
        );

        self.file.as_mut()?.write_all(&descriptor).ok()?;
        self.next_uuid += 1;
        self.interpreter_tracks.insert((pid, tid), uuid);
        Some(uuid)
    }

    /// Open a slice on the interpreter's track.
    ///
    /// A begin whose timestamp cannot be a point on this timeline is remembered as unwritten
    /// rather than skipped outright: its `End` is still coming, and an end that closed something
    /// it did not open would take a slice above it down with it.
    fn begin_slice(&mut self, pid: u32, tid: i64, ts_ns: i64, name: &str, cat: &str, args: &[Arg]) {
        let Some(uuid) = self.interpreter_track_for(pid, tid) else {
            return;
        };
        let written = match u64::try_from(ts_ns) {
            Ok(ts_ns) => {
                let bytes = slice_begin(uuid, ts_ns, name, cat, args);
                self.file
                    .as_mut()
                    .map(|file| file.write_all(&bytes).is_ok())
                    .unwrap_or(false)
            }
            // `TracePacket.timestamp` is a `uint64`, so a negative sign-extends to roughly 584
            // years out. A Collection that landed there would look like a measurement.
            Err(_) => false,
        };
        self.open_slices.entry(uuid).or_default().push(OpenSlice {
            name: name.to_string(),
            cat: cat.to_string(),
            written,
        });
    }

    /// Close the innermost slice on the interpreter's track, if it is the one being closed.
    ///
    /// The name and category never reach the wire — a `SLICE_END` carries neither — so checking
    /// them here is the only use they have, and it is the use the event model documents.
    fn end_slice(&mut self, pid: u32, tid: i64, ts_ns: i64, name: &str, cat: &str) {
        let Some(uuid) = self.interpreter_tracks.get(&(pid, tid)).copied() else {
            // Nothing has ever opened on a track that was never declared. Declaring one here to
            // close a slice on it would draw an empty track for a pairing that is already wrong.
            //
            // A write that failed while declaring the track reaches here too, which is why the
            // warning names that cause as well. Telling the two apart would mean counting the
            // begins dropped per track, for a disk-full path no test reaches; saying both is
            // honest and costs nothing. The reachable case — events arriving before `open` — is
            // dropped whole before this point.
            self.report_mismatch(name, "no track");
            return;
        };
        let Some(open) = self.open_slices.get_mut(&uuid).and_then(Vec::pop) else {
            self.report_mismatch(name, "nothing open");
            return;
        };
        if open.name != name || open.cat != cat {
            // Still closed, and deliberately: Perfetto closes whatever is innermost whatever
            // this end says, so refusing to write it would leave the slice open and the trace
            // structurally invalid — a second fault on top of the producer's.
            self.report_mismatch(name, &open.name);
        }
        if !open.written {
            return;
        }
        let Ok(ts_ns) = u64::try_from(ts_ns) else {
            return;
        };
        let bytes = slice_end(uuid, ts_ns);
        if let Some(file) = self.file.as_mut() {
            file.write_all(&bytes).ok();
        }
    }

    /// Say once that a slice did not close what it named, and keep counting.
    ///
    /// The likeliest cause is the producer, since everything a [`TraceEvent`] pair says is decided
    /// in one place — but a descriptor whose write failed reaches here too, and the message names
    /// both rather than accusing the one it cannot tell apart. Silence would leave a trace that
    /// reads perfectly well and nests wrongly.
    fn report_mismatch(&mut self, closing: &str, open: &str) {
        self.mismatched_ends += 1;
        if self.mismatched_ends == 1 {
            eprintln!(
                "warning: a trace event closed \"{closing}\" while \"{open}\" was what the track \
                 had open — either the events do not pair or a track could not be written. The \
                 Perfetto trace nests by what was open, so its slices may not be the ones the \
                 events described. The rest are counted and reported at the end."
            );
        }
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
        // produces a trace a reader opens instead of a zero-byte file. The snapshot leads,
        // because it is what every timestamp after it is read through.
        file.write_all(&clock_snapshot_packet(&self.run))?;
        file.write_all(&root_descriptor())?;
        file.flush()?;

        self.file = Some(file);
        self.process_tracks.clear();
        self.marked_started.clear();
        self.interpreter_tracks.clear();
        // The open slices go with them. A reopened file is a new trace, and a slice opened into
        // the old one has no end coming that this file could write — carried over, it would let
        // the next end close something this file never opened.
        self.open_slices.clear();
        // The tally belongs to a trace, not to the exporter. Carried over, the second file's
        // first mismatch goes unreported — the "say it once" guard has already fired — and its
        // closing count is another file's.
        self.mismatched_ends = 0;
        self.next_uuid = FIRST_TRACK_UUID;
        Ok(())
    }

    /// The metadata arms run whether or not there is a file to write to. `track_for` writes
    /// nothing and consumes nothing without one, and the names have to be recorded either way: a
    /// batch that arrives before `open` is dropped, but the announcement it carried is the only
    /// one that process or interpreter gets.
    ///
    /// The slice arms are guarded, because a pair is only meaningful whole.
    fn add_events(&mut self, events: &[TraceEvent]) {
        for event in events {
            match event {
                // Remembered before the write and kept whether or not it succeeds. A descriptor
                // that has to be written again — no file yet, or a write that failed — must not
                // lose the name with it, because no second announcement is coming.
                TraceEvent::ProcessMeta { pid, name } => {
                    self.process_names.insert(*pid, name.clone());
                    self.track_for(*pid);
                }
                TraceEvent::ThreadMeta { pid, tid, name } => {
                    self.interpreter_names.insert((*pid, *tid), name.clone());
                    self.interpreter_track_for(*pid, *tid);
                }
                // Both halves are skipped together while there is no file. A `Begin` dropped for
                // want of one is not a pairing fault, and letting its `End` through would report
                // the batch that arrived early as a producer bug.
                TraceEvent::Begin {
                    pid,
                    tid,
                    ts_ns,
                    name,
                    cat,
                    args,
                } if self.file.is_some() => self.begin_slice(*pid, *tid, *ts_ns, name, cat, args),
                TraceEvent::End {
                    pid,
                    tid,
                    ts_ns,
                    name,
                    cat,
                } if self.file.is_some() => self.end_slice(*pid, *tid, *ts_ns, name, cat),
                TraceEvent::Begin { .. } | TraceEvent::End { .. } => {}
                // Counters group and share axes, which is a layout decision of its own, and an
                // Instant has no producer yet. Both are dropped rather than encoded as something
                // plausible: a counter on a track nobody grouped is worse than an absence a
                // reader can see.
                TraceEvent::Counter { .. } | TraceEvent::Instant { .. } => {}
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
        let ProcessLifecycle::Started = kind else {
            return;
        };
        // The track first, and **before the readings are examined**. The mark can arrive before
        // the announcement that names the process — an event on a track nothing declared is what
        // the harness's checks reject — and a process gcscope has no reading for still belongs in
        // the trace. A descriptor alone reaches trace-processor's `process` table, so an operator
        // sees the worker listed even where there is no marker to draw on it.
        let Some(uuid) = self.track_for(pid) else {
            return;
        };
        let Some(ts_ns) = ts_ns else {
            return;
        };
        // The reading is nanoseconds since the run began, and the trace is on the machine's
        // monotonic clock — the one CPython published its own timestamps on. Placing it there is
        // what puts this marker and the Collections that follow on one timeline.
        let Some(ts_ns) = self.run.observer_to_monotonic(ts_ns) else {
            return;
        };
        // `TracePacket.timestamp` is a `uint64`. A negative reading is not a point on this
        // timeline — written out it sign-extends to roughly 584 years — so it is refused here
        // rather than placed somewhere absurd, which is the whole of ADR 0022.
        let Ok(ts_ns) = u64::try_from(ts_ns) else {
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
        if self.mismatched_ends > 1 {
            eprintln!(
                "warning: {} trace events in all closed a slice other than the one their track \
                 had open.",
                self.mismatched_ends
            );
        }
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

    /// A stated anchor instead of two live readings, so one input gives one byte sequence.
    ///
    /// Implausibly small — a real monotonic clock is around 7×10¹⁴ after a week of uptime — and
    /// deliberately: the byte gates below are read by hand, and a five-byte varint in the middle
    /// of them teaches nothing that two bytes do not.
    fn fixed_clock() -> RunClock {
        RunClock::at(5_000, 9_000)
    }

    fn exporter() -> PerfettoExporter {
        PerfettoExporter::with_clock(fixed_clock())
    }

    fn export(events: &[TraceEvent]) -> Vec<u8> {
        let path = temp_path();
        let mut exporter = exporter();
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

    fn thread_meta(pid: u32, tid: i64) -> TraceEvent {
        TraceEvent::ThreadMeta {
            pid,
            tid,
            name: format!("{pid}:{tid}"),
        }
    }

    /// A Collection as the conversion emits one: metadata, then the pause, then its close.
    fn pause(pid: u32, tid: i64, start: i64, stop: i64) -> Vec<TraceEvent> {
        vec![
            process_meta(pid),
            thread_meta(pid, tid),
            TraceEvent::Begin {
                pid,
                tid,
                ts_ns: start,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
                args: vec![("generation", 0i64.into()), ("duration", 0.00125f64.into())],
            },
            TraceEvent::End {
                pid,
                tid,
                ts_ns: stop,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
            },
        ]
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

    /// What every trace opens with, before a producer has said anything: the clock snapshot
    /// that gives its timestamps a meaning, and the root descriptor that makes the ordering
    /// hints below it count. Named so a test can say how many packets its *events* produced.
    const PREAMBLE: usize = 2;

    fn packet_count(bytes: &[u8]) -> usize {
        packet_lengths(bytes).expect("whole packets").len()
    }

    /// A run that observed nothing still writes the preamble, so the file is a trace rather
    /// than zero bytes. Chrome's equivalent is the empty `[]`.
    #[test]
    fn a_trace_with_no_events_is_still_the_preamble() {
        let bytes = export(&[]);
        assert_eq!(packet_count(&bytes), PREAMBLE);
    }

    #[test]
    fn each_process_is_declared_once_however_often_it_reports() {
        let bytes = export(&[process_meta(4242), process_meta(4242), process_meta(7)]);
        // One descriptor per distinct pid, and no second one for the repeat.
        assert_eq!(packet_count(&bytes), PREAMBLE + 2);
    }

    /// Uuid 0 belongs to the root, so a process track must never be handed it: a descriptor
    /// claiming 0 overwrites the ordering hints and every event pointing at track 0 becomes
    /// ambiguous.
    #[test]
    fn no_process_track_claims_the_root_uuid() {
        let mut exporter = exporter();
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
        let mut exporter = exporter();
        exporter.add_events(&[process_meta(4242)]);

        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_count(&bytes), PREAMBLE + 1);
    }

    /// Reopening starts a new file, so every descriptor has to be written into it again.
    #[test]
    fn reopening_declares_the_processes_again() {
        let mut exporter = exporter();

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

        assert_eq!(packet_count(&bytes), PREAMBLE + 1);
    }

    #[test]
    fn closing_an_unopened_exporter_is_a_clean_no_op() {
        exporter().close().unwrap();

        let path = temp_path();
        let mut exporter = exporter();
        exporter.open(&path).unwrap();
        exporter.close().unwrap();
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_count(&bytes), PREAMBLE);
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
    // What the bytes say, read left to right. The clock snapshot first, on [`fixed_clock`]:
    //   0a 12                Trace.packet (field 1, length-delimited), 18 bytes
    //     32 0e              TracePacket.clock_snapshot (6), 14 bytes
    //       0a 05            ClockSnapshot.clocks (1), 5 bytes
    //         08 06          Clock.clock_id (1) = 6, BUILTIN_CLOCK_BOOTTIME
    //         10 a8 46       Clock.timestamp (2) = 9000
    //       0a 05            .clocks again, 5 bytes
    //         08 40          Clock.clock_id = 64, gcscope's own domain
    //         10 88 27       Clock.timestamp = 5000
    //     50 01              TracePacket.trusted_packet_sequence_id (10) = 1
    // and the root descriptor after it:
    //   0a 0d                Trace.packet, 13 bytes
    //     50 01              trusted_packet_sequence_id = 1
    //     e2 03 08           TracePacket.track_descriptor (60), 8 bytes
    //       08 00            TrackDescriptor.uuid (1) = 0, the root
    //       98 01 01         .process_ordering (19) = PROCESS_ORDERING_EXPLICIT
    //       a0 01 01         .thread_ordering (20) = THREAD_ORDERING_EXPLICIT
    // ---------------------------------------------------------------------------------

    const CLOCK_SNAPSHOT_BYTES: [u8; 20] = [
        0x0a, 0x12, 0x32, 0x0e, 0x0a, 0x05, 0x08, 0x06, 0x10, 0xa8, 0x46, 0x0a, 0x05, 0x08, 0x40,
        0x10, 0x88, 0x27, 0x50, 0x01,
    ];

    const ROOT_DESCRIPTOR_BYTES: [u8; 15] = [
        0x0a, 0x0d, 0x50, 0x01, 0xe2, 0x03, 0x08, 0x08, 0x00, 0x98, 0x01, 0x01, 0xa0, 0x01, 0x01,
    ];

    /// The two packets every trace opens with, in the order it opens with them.
    fn preamble_bytes() -> Vec<u8> {
        CLOCK_SNAPSHOT_BYTES
            .iter()
            .chain(&ROOT_DESCRIPTOR_BYTES)
            .copied()
            .collect()
    }

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
    ///       58 03            .child_ordering (11) = EXPLICIT, so the ranks below it count
    ///       60 00            .sibling_order_rank (12) = 0, the first process met
    const ONE_PROCESS_BYTES: [u8; 44] = [
        0x0a, 0x2a, 0x50, 0x01, 0xe2, 0x03, 0x25, 0x08, 0x01, 0x12, 0x0b, b'p', b'y', b't', b'h',
        b'o', b'n', b' ', b'4', b'2', b'4', b'2', 0x1a, 0x10, 0x08, 0x92, 0x21, 0x32, 0x0b, b'p',
        b'y', b't', b'h', b'o', b'n', b' ', b'4', b'2', b'4', b'2', 0x58, 0x03, 0x60, 0x00,
    ];

    #[test]
    fn an_empty_traces_bytes_are_unchanged() {
        assert_eq!(export(&[]), preamble_bytes());
    }

    #[test]
    fn a_one_process_traces_bytes_are_unchanged() {
        let expected: Vec<u8> = preamble_bytes()
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
        let mut exporter = exporter();
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
        // The track, and the marker on it.
        assert_eq!(packet_count(&bytes), PREAMBLE + 2);
    }

    /// Once per process, however many marks arrive, and `Died` draws nothing of its own yet.
    #[test]
    fn a_second_mark_for_one_process_draws_nothing_more() {
        let bytes = export_lifecycle(&[
            (7, ProcessLifecycle::Started, Some(1_000)),
            (7, ProcessLifecycle::Started, Some(1_500)),
            (7, ProcessLifecycle::Died, Some(2_000)),
        ]);
        assert_eq!(packet_count(&bytes), PREAMBLE + 2);
    }

    /// No reading, no marker. An instant has to sit somewhere on the timeline, and a zero puts
    /// it at the origin — decades from everything else, and indistinguishable from a
    /// measurement. The track is still declared, so the process is not lost: a descriptor on its
    /// own reaches the query engine's `process` table, which is the difference between an
    /// operator seeing a worker they cannot expand and not seeing it at all.
    #[test]
    fn a_start_with_no_reading_draws_no_marker() {
        let bytes = export_lifecycle(&[(7, ProcessLifecycle::Started, None)]);
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 1,
            "the track and no marker"
        );
    }

    /// A reading that cannot be a point on this timeline draws no marker. `TracePacket.timestamp`
    /// is a `uint64`, so a negative sign-extends to roughly 584 years out — a number that looks
    /// like a measurement and is not one. The track is declared regardless, as above.
    #[test]
    fn a_negative_reading_draws_no_marker() {
        let bytes = export_lifecycle(&[(7, ProcessLifecycle::Started, Some(-1))]);
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 1,
            "the track and no marker"
        );
    }

    /// The announcement carries the name once per process. A descriptor written before it, or
    /// written again after a failure, takes the name from what the announcement left behind —
    /// otherwise a track that missed it stays nameless for the whole run.
    #[test]
    fn a_track_written_after_the_announcement_still_carries_the_name() {
        let mut exporter = exporter();
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
        let mut exporter = exporter();
        exporter.open(&path).unwrap();
        exporter.mark_process_lifecycle(4242, ProcessLifecycle::Started, Some(1_000));
        exporter.add_events(&[process_meta(4242)]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        // The nameless track and the marker. The announcement adds nothing: the track it would
        // declare already exists, and this encoder does not rewrite a descriptor.
        assert_eq!(packet_count(&bytes), PREAMBLE + 2);
        assert_eq!(exporter.process_tracks.len(), 1);
    }

    /// Counters group and share axes, which is a decision of its own, and nothing produces an
    /// `Instant` yet. Neither is half-encoded in the meantime — the assertion is on the packet
    /// count, so an event that starts being written fails here first.
    #[test]
    fn events_this_stage_does_not_encode_write_nothing() {
        let bytes = export(&[
            process_meta(1),
            thread_meta(1, 0),
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
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 2,
            "the process and interpreter tracks, and neither of the other two"
        );
    }

    // ---------------------------------------------------------------------------------
    // Interpreter tracks and Collection slices.
    // ---------------------------------------------------------------------------------

    /// Each interpreter gets a track of its own under its process, and a Collection on it is a
    /// begin and an end. What the slices *mean* is asserted through the query engine in
    /// `tests/perfetto_encoder.rs`; this counts what was written.
    #[test]
    fn a_collection_becomes_a_slice_on_its_interpreters_track() {
        let bytes = export(&pause(1, 0, 1_000, 2_000));
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 4,
            "the process track, the interpreter's, and the pair"
        );
    }

    /// Two interpreters of one process are two tracks under one, so a Collection in each stays
    /// on its own row rather than nesting inside the other's.
    #[test]
    fn each_interpreter_gets_a_track_and_a_process_gets_one() {
        let mut events = pause(1, 0, 1_000, 2_000);
        events.extend(pause(1, 3, 1_500, 2_500));
        let bytes = export(&events);

        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 7,
            "one process track, two interpreter tracks, two pairs"
        );
    }

    /// Nesting is Perfetto's: a sub-phase opened inside a pause closes before it, so the
    /// innermost end matches the innermost begin. Four packets, one stack.
    #[test]
    fn a_sub_phase_nests_inside_the_pause_that_encloses_it() {
        let mut exporter = exporter();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[
            process_meta(1),
            thread_meta(1, 0),
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 1_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 1_200,
                name: "Mark Alive (gen=0)".to_string(),
                cat: "gc.mark.alive(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 1_800,
                name: "Mark Alive (gen=0)".to_string(),
                cat: "gc.mark.alive(gen=0)".to_string(),
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 2_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
            },
        ]);
        exporter.close().unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(exporter.mismatched_ends, 0, "the pairs matched");
        assert!(
            exporter.open_slices.values().all(Vec::is_empty),
            "a slice was left open: {:?}",
            exporter.open_slices.keys().collect::<Vec<_>>()
        );
    }

    /// **Emission order is the contract; timestamp order is not.** A build that publishes a
    /// phase's stop but not its start chains that phase onto whichever earlier field it has, and
    /// a field that is present and reads zero puts the sub-span's start before its parent's. The
    /// encoder writes what it was handed rather than sorting it back into shape: the raw numbers
    /// are what the target published, and a reordering here would invent a pause that never ran.
    #[test]
    fn a_sub_span_before_its_parent_is_written_in_the_order_it_arrived() {
        let mut exporter = exporter();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[
            process_meta(1),
            thread_meta(1, 0),
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 60_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
                args: vec![],
            },
            // Chained onto a `ts_handle_weakref_callbacks_stop` its build defines and never
            // wrote, so it begins at the epoch — before the pause enclosing it.
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 0,
                name: "Finalize Garbage (gen=0)".to_string(),
                cat: "gc.finalize(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 65_000,
                name: "Finalize Garbage (gen=0)".to_string(),
                cat: "gc.finalize(gen=0)".to_string(),
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 70_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
            },
        ]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(packet_count(&bytes), PREAMBLE + 6);
        assert_eq!(exporter.mismatched_ends, 0, "the pair still matches");
    }

    /// A `SLICE_END` carries no name, so the name an `End` repeats can only ever be checked.
    /// Checked, then: an end that closes something else says so, and still closes what is open —
    /// refusing to write it would leave the slice open and the trace structurally invalid, which
    /// is a second fault on top of the producer's.
    #[test]
    fn an_end_that_closes_the_wrong_slice_is_reported() {
        let mut exporter = exporter();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[
            process_meta(1),
            thread_meta(1, 0),
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 1_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 2_000,
                name: "Mark Alive (gen=0)".to_string(),
                cat: "gc.mark.alive(gen=0)".to_string(),
            },
        ]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(exporter.mismatched_ends, 1);
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 4,
            "the slice is still closed, so the trace still holds together"
        );
    }

    /// An end for a track that never opened anything writes nothing. Written, it would be an end
    /// that opens nothing — which is exactly what the harness's structural check rejects.
    #[test]
    fn an_end_that_opens_nothing_is_refused() {
        let mut exporter = exporter();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[
            process_meta(1),
            thread_meta(1, 0),
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 2_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
            },
        ]);
        // And one on a track nothing ever declared.
        exporter.add_events(&[TraceEvent::End {
            pid: 9,
            tid: 9,
            ts_ns: 2_000,
            name: "GC Pause (gen=0)".to_string(),
            cat: "gc.pause(gen=0)".to_string(),
        }]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(exporter.mismatched_ends, 2);
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 2,
            "the two tracks, and neither end"
        );
    }

    /// A begin gcscope could not place is remembered as unwritten, so the end that follows it
    /// does not close the slice above it instead. `TracePacket.timestamp` is a `uint64`, and a
    /// negative written into one sign-extends to roughly 584 years out.
    #[test]
    fn an_end_never_closes_a_begin_that_was_not_written() {
        let mut exporter = exporter();
        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&[process_meta(1), thread_meta(1, 0)]);
        exporter.add_events(&[
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: 1_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::Begin {
                pid: 1,
                tid: 0,
                ts_ns: -1,
                name: "Mark Alive (gen=0)".to_string(),
                cat: "gc.mark.alive(gen=0)".to_string(),
                args: vec![],
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 1_800,
                name: "Mark Alive (gen=0)".to_string(),
                cat: "gc.mark.alive(gen=0)".to_string(),
            },
            TraceEvent::End {
                pid: 1,
                tid: 0,
                ts_ns: 2_000,
                name: "GC Pause (gen=0)".to_string(),
                cat: "gc.pause(gen=0)".to_string(),
            },
        ]);
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(exporter.mismatched_ends, 0, "every pair matched");
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 4,
            "the two tracks and the pause's own pair; the unplaceable phase wrote neither half"
        );
    }

    /// A batch that arrives before `open` is dropped whole, both halves of a pair together. Let
    /// the `End` through and gcscope reports the early batch as a producer that pairs wrongly —
    /// a warning about a bug that is not there, on the one path where dropping is correct.
    #[test]
    fn a_pair_that_arrives_before_open_is_dropped_without_being_called_a_mismatch() {
        let mut exporter = exporter();
        exporter.add_events(&pause(1, 0, 1_000, 2_000));
        assert_eq!(exporter.mismatched_ends, 0, "nothing was mispaired");

        let path = temp_path();
        exporter.open(&path).unwrap();
        exporter.add_events(&pause(1, 0, 3_000, 4_000));
        exporter.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(exporter.mismatched_ends, 0);
        assert_eq!(
            packet_count(&bytes),
            PREAMBLE + 4,
            "the two tracks and the pair that arrived once there was somewhere to write it"
        );
    }

    /// A trace that has to be reopened starts again. A slice opened into the old file has no end
    /// coming that this one could write, so carrying the stack over would close something the
    /// new file never opened.
    #[test]
    fn reopening_forgets_the_slices_the_old_file_had_open() {
        let mut exporter = exporter();
        let first = temp_path();
        exporter.open(&first).unwrap();
        exporter.add_events(&pause(1, 0, 1_000, 2_000)[..3]);
        assert!(exporter.open_slices.values().any(|open| !open.is_empty()));

        let second = temp_path();
        exporter.open(&second).unwrap();
        exporter.close().unwrap();
        fs::remove_file(&first).ok();
        fs::remove_file(&second).ok();

        assert!(exporter.open_slices.values().all(Vec::is_empty));
    }
}
