"""Read a Perfetto trace and print what is in it, as JSON, for another test to assert on.

This is a translator, not a judge of content. It knows the Perfetto *format* — that a
`TrackEvent` must name a track someone declared, that a slice that opens must close — and
nothing about what any particular producer writes. What tracks should exist, what a slice
should be called, which counters should share an axis: those are expectations, and they belong
in the test suite of whatever wrote the trace. Nothing in this file should reveal what gcscope
emits.

Two decoders, because they answer different questions:

* **the wire view** (`perfetto_trace_pb2`) is the only thing that catches a field number wrong
  from the first commit — a hand-rolled encoder that writes a string into the wrong field
  produces bytes that parse *to something else*, not bytes that fail to parse.
* **the trace-processor view** is the same binary the Perfetto UI runs, so it settles whether a
  wire-valid trace *means* what it should.

One document carries both, and the caller picks the one its question needs.

Usage:
    python perfetto_dump.py TRACE [--skip-trace-processor]

Exit codes: 0 the trace is structurally valid, 1 it is not, 2 the harness could not run (bad
arguments, unreadable file, missing dependency). A rejection is data either way — the document
is printed on stdout with its problems listed, so a caller reads *what* was wrong rather than
inferring it from a number.
"""

from __future__ import annotations

import argparse
import json
import sys
import traceback
from pathlib import Path

SCHEMA = "gcscope.perfetto-dump/1"

# ---------------------------------------------------------------------------
# How the wire view is rendered
# ---------------------------------------------------------------------------

# `MessageToDict` takes the grammar from the `.proto` rather than from us, which is the point:
# a rendering this file invented would be one more thing to keep in step with the schema. Three
# of its behaviours are decisions, though, and each is silent when wrong.
RENDERING = {
    # Names arrive as the schema spells them. Without this every name is camelCased, and stops
    # matching the field table an encoder was written against.
    "preserving_proto_field_name": True,
    # A field nobody wrote stays absent. The alternative prints every unwritten field at its
    # default, which would drown the one signal a reader wants from this view — that a producer
    # wrote something nobody expected — in a hundred zeros it did not write.
    #
    # This is safe here in a way it would not be under proto3 defaults: Perfetto's fields carry
    # explicit presence, so a `0` that was *written* is rendered and a field that was never
    # touched is omitted. The two are distinguishable, and `test_perfetto_dump.py` pins that.
    "always_print_fields_with_no_presence": False,
}

# 64-bit fields render as JSON *strings*, which is protobuf's rule rather than a choice, and it
# is the correct one: a nanosecond timestamp is around 1.7e18 and an IEEE double holds integers
# exactly only to 9e15. A caller parses them from strings. Pinned by a test, since a future
# protobuf that "helpfully" emitted numbers would silently start rounding timestamps.

# ---------------------------------------------------------------------------
# The trace-processor view
# ---------------------------------------------------------------------------

# Every query is ordered, so two runs over one trace produce the same rows in the same
# sequence. Without an ORDER BY, SQLite is free to return them however it likes.
QUERIES = {
    "slice": """
        select id, ts, dur, track_id, depth, parent_id, category, name, arg_set_id
        from slice order by ts, track_id, depth, id
    """,
    "slice_args": """
        select s.id as slice_id, a.key, a.value_type, a.int_value, a.string_value, a.real_value
        from slice s join args a on s.arg_set_id = a.arg_set_id
        order by s.id, a.key
    """,
    "track": "select id, name, parent_id from track order by id",
    "counter_track": "select id, name, parent_id, unit from counter_track order by id",
    "counter": "select id, ts, track_id, value from counter order by ts, track_id, id",
    "process": "select upid, pid, name from process order by pid, upid",
    "thread": "select utid, tid, name, upid from thread order by tid, utid",
}


def trace_processor_view(path: Path) -> dict:
    """Run every query and return `{name: [row, ...]}`.

    A query that fails becomes `{"error": ...}` under its own name rather than taking the whole
    dump down. Trace-processor's columns move between releases, and a caller that asked about
    slices should still get them on the day the `track` schema changes underneath it. The
    failure is in the document, so nobody mistakes a missing view for an empty one.
    """
    from perfetto.trace_processor import TraceProcessor

    view: dict[str, object] = {}
    processor = TraceProcessor(trace=str(path))
    try:
        for name, sql in QUERIES.items():
            try:
                view[name] = [_row_as_dict(row) for row in processor.query(sql)]
            except Exception as failure:  # noqa: BLE001 - reported, not handled
                view[name] = {"error": f"{type(failure).__name__}: {failure}"}
    finally:
        processor.close()
    return view


def _row_as_dict(row) -> dict:
    """One query result row as plain JSON values.

    Trace-processor hands back a row object whose attributes are the selected columns; a NULL
    arrives as `None` and stays `None`, since "this column had no value" is exactly what a
    caller needs to tell apart from zero.
    """
    return {name: _json_safe(value) for name, value in vars(row).items()}


def _json_safe(value):
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    return str(value)


# ---------------------------------------------------------------------------
# Structural validity
# ---------------------------------------------------------------------------


def _problem(check: str, detail: str, packet: int | None = None) -> dict:
    return {"check": check, "detail": detail, "packet": packet}


def _effective_track_uuids(trace) -> list[int | None]:
    """The track each packet's `TrackEvent` sits on, one entry per packet.

    `None` for a packet that carries no event, and for an event whose track cannot be resolved.

    A producer may state `trace_packet_defaults.track_event_defaults.track_uuid` once and leave
    the field off every event after it — that is Perfetto's standard way of not repeating a
    uuid on thousands of packets, and both spellings mean the same track. A checker reading
    only the explicit field calls that pattern malformed, and worse, files every such event
    under track `0`, so a begin on one track pairs with an end on another.

    Defaults are per sequence and hold until that sequence states new ones. This does not model
    `previous_packet_dropped` resetting them; a trace that needs it can say so here.
    """
    defaults: dict[int, int] = {}
    resolved: list[int | None] = []
    for packet in trace.packet:
        sequence = packet.trusted_packet_sequence_id
        if packet.HasField("trace_packet_defaults"):
            event_defaults = packet.trace_packet_defaults.track_event_defaults
            if event_defaults.HasField("track_uuid"):
                defaults[sequence] = event_defaults.track_uuid

        if packet.WhichOneof("data") != "track_event":
            resolved.append(None)
        elif packet.track_event.HasField("track_uuid"):
            resolved.append(packet.track_event.track_uuid)
        else:
            resolved.append(defaults.get(sequence))
    return resolved


def check_validity(trace) -> list[dict]:
    """Everything that can be wrong with a trace without knowing what it is *about*.

    Each check is named in the problem it reports, so a caller asserts on the name rather than
    on the wording of a message.
    """
    tracks = _effective_track_uuids(trace)
    problems: list[dict] = []
    problems += _check_sequence_ids(trace)
    problems += _check_track_uuids_declared(trace, tracks)
    problems += _check_slices_are_paired(trace, tracks)
    return problems


def _check_sequence_ids(trace) -> list[dict]:
    """Every packet names the sequence it belongs to.

    Stricter than the format demands — Perfetto tolerates packets outside any sequence — and
    deliberately so. Interning and incremental state are sequence-scoped, so a packet that
    escapes its sequence is far more likely a producer bug than a design. A producer that
    genuinely needs one should be made to say so here.
    """
    return [
        _problem("sequence_id", "packet carries no trusted_packet_sequence_id", index)
        for index, packet in enumerate(trace.packet)
        if not packet.HasField("trusted_packet_sequence_id")
    ]


def _check_track_uuids_declared(trace, tracks: list[int | None]) -> list[dict]:
    """Every track a `TrackEvent` names was declared by some `TrackDescriptor`.

    Declared *anywhere* in the trace, not necessarily earlier: descriptors and events may be
    interleaved, and a reader that consumed the whole file resolves either order.
    """
    declared = {
        packet.track_descriptor.uuid
        for packet in trace.packet
        if packet.WhichOneof("data") == "track_descriptor"
    }
    problems = []
    for index, packet in enumerate(trace.packet):
        if packet.WhichOneof("data") != "track_event":
            continue
        uuid = tracks[index]
        if uuid is None:
            problems.append(
                _problem(
                    "track_uuid",
                    "track_event names no track, and its sequence declared no default",
                    index,
                )
            )
        elif uuid not in declared:
            problems.append(
                _problem("track_uuid", f"track {uuid} was never declared", index)
            )
    return problems


def _check_slices_are_paired(trace, tracks: list[int | None]) -> list[dict]:
    """Slices nest: on each track, every begin closes and no end arrives unopened."""
    from perfetto.protos.perfetto.trace.perfetto_trace_pb2 import TrackEvent

    open_slices: dict[int | None, list[int]] = {}
    problems = []
    for index, packet in enumerate(trace.packet):
        if packet.WhichOneof("data") != "track_event":
            continue
        event = packet.track_event
        track = tracks[index]
        if event.type == TrackEvent.TYPE_SLICE_BEGIN:
            open_slices.setdefault(track, []).append(index)
        elif event.type == TrackEvent.TYPE_SLICE_END:
            if not open_slices.get(track):
                problems.append(
                    _problem("slice_pairing", f"slice end on track {track} opened nothing", index)
                )
            else:
                open_slices[track].pop()

    for track, still_open in sorted(open_slices.items()):
        for index in still_open:
            problems.append(
                _problem("slice_pairing", f"slice begin on track {track} never ends", index)
            )
    return problems


# ---------------------------------------------------------------------------
# The document
# ---------------------------------------------------------------------------


def dump(path: Path, with_trace_processor: bool = True) -> dict:
    """The whole document for one trace: what it says, and whether it holds together."""
    from google.protobuf.json_format import MessageToDict
    from google.protobuf.message import DecodeError
    from perfetto.protos.perfetto.trace.perfetto_trace_pb2 import Trace

    raw = path.read_bytes()
    trace = Trace()
    try:
        trace.ParseFromString(raw)
    except DecodeError as failure:
        return {
            "schema": SCHEMA,
            "bytes": len(raw),
            "validity": {
                "ok": False,
                "problems": [_problem("parses", f"not a Perfetto trace: {failure}")],
            },
            "wire": None,
            "trace_processor": None,
        }

    problems = check_validity(trace)
    return {
        "schema": SCHEMA,
        "bytes": len(raw),
        "validity": {"ok": not problems, "problems": problems},
        "wire": MessageToDict(trace, **RENDERING),
        "trace_processor": trace_processor_view(path) if with_trace_processor else None,
    }


def render(document: dict) -> str:
    """The document as bytes on stdout.

    `sort_keys` so one trace always renders one way regardless of how a protobuf release
    happens to order a message's fields; arrays keep the order they were built in, which is the
    order the trace has.
    """
    return json.dumps(document, sort_keys=True, indent=2)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("trace", type=Path, help="path to a .perfetto-trace file")
    parser.add_argument(
        "--skip-trace-processor",
        action="store_true",
        help="wire view only; skips the query engine and the binary it downloads",
    )
    args = parser.parse_args(argv)

    if not args.trace.is_file():
        print(f"no trace at {args.trace}", file=sys.stderr)
        return 2

    # Anything that goes wrong *here* is the harness failing, not the trace being bad, and the
    # two must not share an exit code — 1 tells a caller the file it handed over is malformed.
    # The engine alone can fail half a dozen ways that are nothing to do with the trace: no
    # network for its first download, a corrupt cached binary, a port it cannot bind, a load
    # that times out. None of them is an `ImportError`, which is all this used to catch.
    try:
        document = dump(args.trace, with_trace_processor=not args.skip_trace_processor)
    except ImportError as missing:
        print(f"the harness needs the `perfetto` package: {missing}", file=sys.stderr)
        return 2
    except Exception:  # noqa: BLE001 - the harness failing is its own exit code
        traceback.print_exc()
        print("the harness could not read the trace; this is not a verdict on it", file=sys.stderr)
        return 2

    print(render(document))
    return 0 if document["validity"]["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
