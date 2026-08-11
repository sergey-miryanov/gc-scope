"""Tests for the dump harness itself, against the traces in `reference_traces`.

Those are authored with the protobuf bindings rather than by anything in this repo, which is
the point: a decoder whose only input has ever been the encoder it is meant to check is a weak
oracle, since the two agree by construction and neither has been shown a third opinion.

The malformed cases matter as much as the well-formed one. A validity check nobody has watched
reject something is a check that might only ever say yes.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

import perfetto_dump  # noqa: E402
import reference_traces  # noqa: E402
from reference_traces import FIRST_TS, THREAD_TRACK, reference as reference_trace  # noqa: E402
from perfetto.protos.perfetto.trace.perfetto_trace_pb2 import Trace  # noqa: E402

HARNESS = Path(__file__).parent / "perfetto_dump.py"


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def write_trace(tmp_path):
    def write(trace: Trace, name: str = "reference") -> Path:
        path = tmp_path / f"{name}.perfetto-trace"
        path.write_bytes(trace.SerializeToString())
        return path

    return write


@pytest.fixture
def reference(write_trace) -> Path:
    return write_trace(reference_trace())


def wire_dump(path: Path) -> dict:
    """The wire half only. Nothing that asks about the format needs the query engine."""
    return perfetto_dump.dump(path, with_trace_processor=False)


# ---------------------------------------------------------------------------
# The wire view reports what was put in
# ---------------------------------------------------------------------------


def test_every_packet_of_the_reference_trace_comes_back(reference):
    document = wire_dump(reference)
    packets = document["wire"]["packet"]

    assert len(packets) == len(reference_trace().packet)
    kinds = [next(k for k in p if k in ("track_descriptor", "track_event")) for p in packets]
    assert kinds == ["track_descriptor"] * 3 + ["track_event"] * 6


def test_a_slice_arrives_with_its_track_name_category_and_annotations(reference):
    packets = wire_dump(reference)["wire"]["packet"]
    opening = next(
        p["track_event"]
        for p in packets
        if p.get("track_event", {}).get("name") == "Work"
        and p["track_event"]["type"] == "TYPE_SLICE_BEGIN"
    )

    assert opening["track_uuid"] == str(THREAD_TRACK)
    assert opening["categories"] == ["demo"]
    assert opening["debug_annotations"] == [{"name": "attempt", "int_value": "3"}]


def test_the_counter_samples_come_back_in_order(reference):
    packets = wire_dump(reference)["wire"]["packet"]
    samples = [
        (p["timestamp"], p["track_event"]["counter_value"])
        for p in packets
        if p.get("track_event", {}).get("type") == "TYPE_COUNTER"
    ]

    assert samples == [
        ("1690000000000000100", "5"),
        ("1690000000000000900", "2"),
    ]


def test_a_field_nobody_expected_arrives_as_its_own_key(write_trace):
    """The property that makes JSON worth the trouble.

    Against a flat text dump, "the producer wrote something nobody asked for" is invisible to a
    substring assertion — the unexpected token simply goes unread. Here the caller compares key
    sets and the surprise fails a test.
    """
    trace = reference_trace()
    trace.packet[0].track_descriptor.child_ordering = 2  # nobody asked for this
    surprising = write_trace(trace, "surprising")

    expected_keys = set(wire_dump(write_trace(reference_trace()))["wire"]["packet"][0]["track_descriptor"])
    actual_keys = set(wire_dump(surprising)["wire"]["packet"][0]["track_descriptor"])

    assert actual_keys - expected_keys == {"child_ordering"}


# ---------------------------------------------------------------------------
# The three rendering decisions
# ---------------------------------------------------------------------------


def test_field_names_are_the_ones_the_schema_uses(reference):
    descriptor = wire_dump(reference)["wire"]["packet"][0]["track_descriptor"]

    assert "sibling_order_rank" in descriptor
    assert "siblingOrderRank" not in descriptor


def test_a_written_zero_is_not_the_same_as_a_field_nobody_wrote(write_trace):
    """The decision `always_print_fields_with_no_presence=False` turns on.

    Perfetto's fields carry explicit presence, so an encoder that deliberately writes rank 0
    says something a reader can see, and one that never touched the field says nothing. Print
    the no-presence defaults instead and the two become the same document.
    """
    written = wire_dump(write_trace(reference_trace(), "written"))
    first = written["wire"]["packet"][0]["track_descriptor"]
    assert first["sibling_order_rank"] == 0

    silent = reference_trace()
    silent.packet[0].track_descriptor.ClearField("sibling_order_rank")
    absent = wire_dump(write_trace(silent, "silent"))
    assert "sibling_order_rank" not in absent["wire"]["packet"][0]["track_descriptor"]


def test_64_bit_fields_are_strings_so_no_timestamp_is_rounded(reference):
    """A nanosecond timestamp near 1.7e18 has more significant digits than a double holds.

    The value below differs from its double-rounded self, so a renderer that emitted a JSON
    number here would fail this test rather than quietly move an event.
    """
    packets = wire_dump(reference)["wire"]["packet"]
    timestamps = [p["timestamp"] for p in packets if "timestamp" in p]

    assert all(isinstance(t, str) for t in timestamps)
    exact = FIRST_TS
    assert str(exact) in timestamps
    assert int(float(exact)) != exact, "the rounding this decision avoids"


# ---------------------------------------------------------------------------
# Structural validity, and the traces it must reject
# ---------------------------------------------------------------------------


def test_the_reference_trace_is_valid(reference):
    validity = wire_dump(reference)["validity"]

    assert validity["ok"] is True
    assert validity["problems"] == []


@pytest.mark.parametrize(
    "kind,check",
    [
        ("undeclared-track", "track_uuid"),
        ("unmatched-begin", "slice_pairing"),
        ("unopened-end", "slice_pairing"),
        ("no-sequence-id", "sequence_id"),
    ],
)
def test_a_malformed_trace_is_rejected_by_the_check_that_owns_it(write_trace, kind, check):
    validity = wire_dump(write_trace(reference_traces.TRACES[kind](), kind))["validity"]

    assert validity["ok"] is False
    assert [p["check"] for p in validity["problems"]] == [check]
    assert validity["problems"][0]["packet"] is not None


def test_a_track_named_once_in_the_sequence_defaults_is_still_named(write_trace):
    """The standard producer pattern, which a naive checker rejects.

    Two things go wrong at once when defaults are ignored: every event looks like it named no
    track, and every event lands in bucket `0` — so a begin on one track would pair with an end
    on another and the pairing check would go quiet exactly where it matters.
    """
    path = write_trace(reference_traces.track_from_defaults(), "defaults")
    validity = wire_dump(path)["validity"]

    assert validity["ok"] is True, validity["problems"]


def test_an_event_on_a_sequence_with_no_default_and_no_track_is_still_caught(write_trace):
    """Resolving defaults must not turn the check off for a trace that really is missing one.

    `track_uuid` fires once per event and `slice_pairing` stays quiet, which is the intended
    division: an event whose track cannot be resolved is already reported by the check that
    owns that question, and the unresolvable ones share one bucket rather than being scattered
    into track `0` where they would pair against real tracks.
    """
    trace = reference_traces.track_from_defaults()
    del trace.packet[1]  # the defaults packet
    validity = wire_dump(write_trace(trace, "no-defaults"))["validity"]

    assert validity["ok"] is False
    assert [p["check"] for p in validity["problems"]] == ["track_uuid", "track_uuid"]


def test_unclosed_slices_on_resolvable_and_unresolvable_tracks_are_both_reported(write_trace):
    """A verdict, not a crash.

    Unresolvable events are filed under `None`, so a trace holding both kinds asks the reporter
    to order `None` against an integer. Raised, that would leave the caller with exit 2 — the
    harness saying it could not run — and would throw away the pairing problems it had already
    found. An oracle that reports an infrastructure failure in place of the defect it detected is
    worse than one that never looked.
    """
    trace = reference_traces.unclosed_on_a_named_and_an_unresolvable_track()
    validity = wire_dump(write_trace(trace, "unclosed-mixed"))["validity"]

    assert validity["ok"] is False
    checks = [p["check"] for p in validity["problems"]]
    assert checks.count("slice_pairing") == 2, validity["problems"]
    # And the event naming no track is still reported by the check that owns that question.
    assert "track_uuid" in checks


def test_bytes_that_are_not_a_trace_are_reported_rather_than_raised(tmp_path):
    path = tmp_path / "garbage.perfetto-trace"
    path.write_bytes(b"\xff\xff\xff\xff not a trace at all")

    document = wire_dump(path)

    assert document["validity"]["ok"] is False
    assert document["validity"]["problems"][0]["check"] == "parses"
    assert document["wire"] is None


def test_a_problem_names_which_packet_it_is_about(write_trace):
    """A caller fixing a trace needs to know where to look, not only that something is wrong."""
    trace = reference_trace()
    trace.packet[4].track_event.track_uuid = 404  # the inner begin, now orphaned
    document = wire_dump(write_trace(trace, "orphaned-inner"))

    problems = document["validity"]["problems"]
    assert {p["check"] for p in problems} == {"track_uuid", "slice_pairing"}
    assert any(p["packet"] == 4 for p in problems)


# ---------------------------------------------------------------------------
# Determinism and the process boundary
# ---------------------------------------------------------------------------


def test_one_trace_renders_the_same_bytes_every_time(reference):
    first = perfetto_dump.render(wire_dump(reference))
    second = perfetto_dump.render(wire_dump(reference))

    assert first == second
    json.loads(first)


def test_the_command_line_prints_the_document_and_says_whether_it_held_together(
    reference, write_trace
):
    """What a caller in another language sees: JSON on stdout, verdict in the exit code."""
    good = subprocess.run(
        [sys.executable, str(HARNESS), str(reference), "--skip-trace-processor"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert good.returncode == 0, good.stderr
    assert json.loads(good.stdout)["validity"]["ok"] is True

    broken = write_trace(reference_traces.unmatched_begin(), "broken")
    bad = subprocess.run(
        [sys.executable, str(HARNESS), str(broken), "--skip-trace-processor"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert bad.returncode == 1, bad.stderr
    document = json.loads(bad.stdout)
    assert document["validity"]["ok"] is False
    assert document["validity"]["problems"][0]["check"] == "slice_pairing"


def test_a_missing_trace_is_a_harness_failure_not_a_verdict(tmp_path):
    result = subprocess.run(
        [sys.executable, str(HARNESS), str(tmp_path / "nope.perfetto-trace")],
        capture_output=True,
        text=True,
        check=False,
    )

    assert result.returncode == 2
    assert result.stdout == ""


def test_the_query_engine_failing_is_a_harness_failure_not_a_verdict(reference, monkeypatch):
    """Exit 1 means *the trace* is malformed. Nothing else may claim it.

    The engine fails in ways that have nothing to do with the file it was handed: no network
    for its first download, a corrupt cached binary, a port it cannot bind, a load that times
    out. Reporting any of those as 1 tells a caller its trace is bad, which is a lie a CI job
    would act on.
    """

    def explode(_path):
        raise OSError("the query engine could not start")

    monkeypatch.setattr(perfetto_dump, "trace_processor_view", explode)

    assert perfetto_dump.main([str(reference)]) == 2


# ---------------------------------------------------------------------------
# The trace-processor view
# ---------------------------------------------------------------------------


@pytest.mark.trace_processor
def test_the_query_engine_sees_the_same_trace_the_wire_view_did(reference):
    """The semantic half: wire-valid is not the same as meaning what it should.

    Slower than the rest — it starts the same query engine the Perfetto UI runs, downloading it
    once — so it carries its own marker and can be deselected.
    """
    view = perfetto_dump.dump(reference)["trace_processor"]

    assert 4242 in [row["pid"] for row in view["process"]]
    assert 7 in [row["tid"] for row in view["thread"]]

    slices = {row["name"]: row for row in view["slice"]}
    assert set(slices) == {"Work", "Detail"}
    assert slices["Detail"]["depth"] == 1
    assert slices["Detail"]["parent_id"] == slices["Work"]["id"]
    assert slices["Work"]["dur"] == 863  # 900 ns after the begin at ...037

    annotations = {row["key"]: row["int_value"] for row in view["slice_args"]}
    assert annotations["debug.attempt"] == 3

    counters = [row["value"] for row in view["counter"]]
    assert counters == [5.0, 2.0]


@pytest.mark.trace_processor
def test_a_query_against_a_column_this_release_lacks_is_reported_as_data(reference, monkeypatch):
    """Trace-processor's schema moves between releases.

    A caller that asked about slices gets them on the day the `track` schema changes, and reads
    why the other view is missing instead of finding it empty.
    """
    monkeypatch.setitem(
        perfetto_dump.QUERIES, "track", "select no_such_column from track"
    )
    view = perfetto_dump.dump(reference)["trace_processor"]

    assert "error" in view["track"]
    assert view["slice"], "one failing query does not take the others down"
