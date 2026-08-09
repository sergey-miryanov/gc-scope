"""Perfetto traces authored with the protobuf bindings, for checking a decoder against.

A decoder whose only input has ever been the encoder it is meant to check is a weak oracle: the
two agree by construction and neither has been shown a third opinion. Everything here is built
from `perfetto_trace_pb2` — at call time rather than committed as bytes, so the reference and
the schema cannot drift apart on disk — and the harness has to report exactly what was put in.

Nothing here is a GC trace. It is a process that runs a slice called `Work` with a `Detail`
inside it and a counter that samples twice: enough shape to exercise nesting, annotations,
counters and ordering rank, with no domain in it. A reader of this file should not be able to
tell what gcscope writes.

Usage:
    python reference_traces.py KIND OUT    # KIND is one of the names in TRACES
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from perfetto.protos.perfetto.trace.perfetto_trace_pb2 import Trace, TrackEvent

PROCESS_TRACK = 1
THREAD_TRACK = 2
COUNTER_TRACK = 3

PROCESS_PID = 4242
THREAD_TID = 7

# The opening timestamp. Chosen with more significant digits than an IEEE double holds exactly,
# so a renderer that turned 64-bit fields into JSON numbers would move it.
FIRST_TS = 1_690_000_000_000_000_037
LAST_TS = 1_690_000_000_000_000_900


def _descriptor_packet(trace: Trace, uuid: int, name: str, sequence: int = 1):
    packet = trace.packet.add()
    packet.trusted_packet_sequence_id = sequence
    packet.track_descriptor.uuid = uuid
    packet.track_descriptor.name = name
    return packet.track_descriptor


def _event_packet(trace: Trace, uuid: int, ts: int, sequence: int = 1):
    packet = trace.packet.add()
    packet.trusted_packet_sequence_id = sequence
    packet.timestamp = ts
    packet.track_event.track_uuid = uuid
    return packet.track_event


def reference() -> Trace:
    """One process, a thread track under it, a nested slice pair, and a counter."""
    trace = Trace()

    process = _descriptor_packet(trace, PROCESS_TRACK, f"Process {PROCESS_PID}")
    process.process.pid = PROCESS_PID
    # Written, and the harness must not confuse this with a field nobody touched.
    process.sibling_order_rank = 0

    thread = _descriptor_packet(trace, THREAD_TRACK, "Worker")
    thread.parent_uuid = PROCESS_TRACK
    thread.thread.pid = PROCESS_PID
    thread.thread.tid = THREAD_TID

    counter = _descriptor_packet(trace, COUNTER_TRACK, "queue_depth")
    counter.parent_uuid = PROCESS_TRACK
    counter.counter.unit_name = "items"

    outer = _event_packet(trace, THREAD_TRACK, FIRST_TS)
    outer.type = TrackEvent.TYPE_SLICE_BEGIN
    outer.name = "Work"
    outer.categories.append("demo")
    annotation = outer.debug_annotations.add()
    annotation.name = "attempt"
    annotation.int_value = 3

    inner = _event_packet(trace, THREAD_TRACK, 1_690_000_000_000_000_100)
    inner.type = TrackEvent.TYPE_SLICE_BEGIN
    inner.name = "Detail"
    inner.categories.append("demo")

    closing_inner = _event_packet(trace, THREAD_TRACK, 1_690_000_000_000_000_200)
    closing_inner.type = TrackEvent.TYPE_SLICE_END
    closing_inner.name = "Detail"

    closing_outer = _event_packet(trace, THREAD_TRACK, LAST_TS)
    closing_outer.type = TrackEvent.TYPE_SLICE_END
    closing_outer.name = "Work"

    for ts, value in ((1_690_000_000_000_000_100, 5), (LAST_TS, 2)):
        sample = _event_packet(trace, COUNTER_TRACK, ts)
        sample.type = TrackEvent.TYPE_COUNTER
        sample.counter_value = value

    return trace


def track_from_defaults() -> Trace:
    """A well-formed trace that names its track once, in the sequence's defaults.

    Perfetto's standard way of not repeating a uuid on thousands of packets: state
    `track_event_defaults.track_uuid` once and leave the field off every event. A checker that
    reads only the explicit field calls this malformed — and files every event under track `0`,
    where a begin on one track pairs with an end on another.
    """
    trace = Trace()
    _descriptor_packet(trace, THREAD_TRACK, "Worker")

    defaults = trace.packet.add()
    defaults.trusted_packet_sequence_id = 1
    defaults.trace_packet_defaults.track_event_defaults.track_uuid = THREAD_TRACK

    for ts, kind, name in (
        (FIRST_TS, TrackEvent.TYPE_SLICE_BEGIN, "Work"),
        (LAST_TS, TrackEvent.TYPE_SLICE_END, "Work"),
    ):
        packet = trace.packet.add()
        packet.trusted_packet_sequence_id = 1
        packet.timestamp = ts
        packet.track_event.type = kind  # no track_uuid: the sequence already said which
        packet.track_event.name = name

    return trace


def undeclared_track() -> Trace:
    """An event on a track no descriptor ever announced."""
    trace = Trace()
    _descriptor_packet(trace, PROCESS_TRACK, f"Process {PROCESS_PID}")
    event = _event_packet(trace, 99, 1_000)
    event.type = TrackEvent.TYPE_INSTANT
    event.name = "orphan"
    return trace


def unmatched_begin() -> Trace:
    """A slice that opens and never closes."""
    trace = Trace()
    _descriptor_packet(trace, THREAD_TRACK, "Worker")
    begin = _event_packet(trace, THREAD_TRACK, 1_000)
    begin.type = TrackEvent.TYPE_SLICE_BEGIN
    begin.name = "never ends"
    return trace


def unopened_end() -> Trace:
    """A slice that closes without having opened."""
    trace = Trace()
    _descriptor_packet(trace, THREAD_TRACK, "Worker")
    end = _event_packet(trace, THREAD_TRACK, 1_000)
    end.type = TrackEvent.TYPE_SLICE_END
    end.name = "never began"
    return trace


def no_sequence_id() -> Trace:
    """A packet belonging to no sequence."""
    trace = Trace()
    packet = trace.packet.add()
    packet.track_descriptor.uuid = PROCESS_TRACK
    packet.track_descriptor.name = f"Process {PROCESS_PID}"
    return trace


TRACES = {
    "reference": reference,
    "track-from-defaults": track_from_defaults,
    "undeclared-track": undeclared_track,
    "unmatched-begin": unmatched_begin,
    "unopened-end": unopened_end,
    "no-sequence-id": no_sequence_id,
}


def write(kind: str, path: Path) -> Path:
    path.write_bytes(TRACES[kind]().SerializeToString())
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("kind", choices=sorted(TRACES))
    parser.add_argument("out", type=Path)
    args = parser.parse_args(argv)
    write(args.kind, args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
