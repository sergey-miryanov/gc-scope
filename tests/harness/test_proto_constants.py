"""Tests for the field-number checker itself.

The checker is the only thing standing between a hand-transcribed table and a trace that parses
to the wrong message, so it has to be shown a wrong number and seen to reject it. A checker
nobody has watched say no is one that might only ever say yes.

The correct claims here are read out of the descriptors at test time rather than written down,
so this file has no second copy of the table to keep in step — and no opinion about which
fields any producer cares about.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

import proto_constants  # noqa: E402

CHECKER = Path(__file__).parent / "proto_constants.py"


@pytest.fixture
def messages() -> dict:
    return proto_constants._messages()


def field_number(messages: dict, message: str, field: str) -> int:
    return messages[message].fields_by_name[field].number


# ---------------------------------------------------------------------------
# Claims that hold
# ---------------------------------------------------------------------------


def test_claims_taken_from_the_schema_all_hold(messages):
    claims = {
        "fields": [
            ["TracePacket", "timestamp", field_number(messages, "TracePacket", "timestamp")],
            ["TrackDescriptor", "uuid", field_number(messages, "TrackDescriptor", "uuid")],
        ],
        "enums": [["TrackEvent", "Type", "TYPE_SLICE_BEGIN", 1]],
    }
    verdict = proto_constants.check(claims)

    assert verdict["ok"], verdict["problems"]
    assert verdict["checked"] == {"fields": 2, "enums": 1}


def test_an_empty_claim_set_is_reported_as_nothing_checked():
    """A caller handed 55 claims and told `checked: 0` has learnt something a bare "no problems"
    would have hidden — an input that never arrived reads exactly like one that was fine."""
    verdict = proto_constants.check({})

    assert verdict["ok"]
    assert verdict["checked"] == {"fields": 0, "enums": 0}


# ---------------------------------------------------------------------------
# Claims that do not
# ---------------------------------------------------------------------------


def test_a_wrong_field_number_is_rejected_with_both_numbers(messages):
    actual = field_number(messages, "DebugAnnotation", "name")
    verdict = proto_constants.check({"fields": [["DebugAnnotation", "name", 1]]})

    assert not verdict["ok"]
    (problem,) = verdict["problems"]
    assert problem["claim"] == "DebugAnnotation.name"
    assert "1" in problem["detail"] and str(actual) in problem["detail"]


def test_a_field_the_message_does_not_have_is_rejected():
    verdict = proto_constants.check({"fields": [["TrackDescriptor", "colour", 3]]})

    assert not verdict["ok"]
    (problem,) = verdict["problems"]
    assert problem["claim"] == "TrackDescriptor.colour"
    assert "no field named colour" in problem["detail"]


def test_a_message_the_schema_does_not_have_is_rejected():
    verdict = proto_constants.check({"fields": [["TrackDescriptorr", "uuid", 1]]})

    assert not verdict["ok"]
    assert "no message named TrackDescriptorr" in verdict["problems"][0]["detail"]


def test_a_wrong_enum_value_is_rejected():
    verdict = proto_constants.check({"enums": [["TrackEvent", "Type", "TYPE_SLICE_END", 1]]})

    assert not verdict["ok"]
    (problem,) = verdict["problems"]
    assert problem["claim"] == "TrackEvent.Type.TYPE_SLICE_END"
    assert "the schema says 2" in problem["detail"]


def test_an_enum_the_message_does_not_declare_is_rejected():
    verdict = proto_constants.check({"enums": [["TrackEvent", "Ordering", "EXPLICIT", 3]]})

    assert not verdict["ok"]
    assert "declares no enum named Ordering" in verdict["problems"][0]["detail"]


def test_a_value_the_enum_does_not_have_is_rejected():
    verdict = proto_constants.check({"enums": [["TrackEvent", "Type", "TYPE_FLOW", 5]]})

    assert not verdict["ok"]
    assert "no value named TYPE_FLOW" in verdict["problems"][0]["detail"]


def test_every_bad_claim_is_reported_not_only_the_first():
    """A checker that stopped at the first problem would make fixing a table an N-run loop, and
    would hide how much of it is wrong."""
    verdict = proto_constants.check(
        {
            "fields": [["TracePacket", "timestamp", 7], ["TrackDescriptor", "uuid", 2]],
            "enums": [["TrackEvent", "Type", "TYPE_SLICE_BEGIN", 9]],
        }
    )

    assert [problem["claim"] for problem in verdict["problems"]] == [
        "TracePacket.timestamp",
        "TrackDescriptor.uuid",
        "TrackEvent.Type.TYPE_SLICE_BEGIN",
    ]


# ---------------------------------------------------------------------------
# The command line
# ---------------------------------------------------------------------------


def run(claims_path: Path) -> tuple[int, dict | None]:
    result = subprocess.run(
        [sys.executable, str(CHECKER), str(claims_path)],
        capture_output=True,
        text=True,
    )
    document = json.loads(result.stdout) if result.stdout.strip() else None
    return result.returncode, document


def test_a_table_that_holds_exits_zero(tmp_path, messages):
    claims = tmp_path / "claims.json"
    claims.write_text(
        json.dumps({"fields": [["Trace", "packet", field_number(messages, "Trace", "packet")]]})
    )

    code, document = run(claims)

    assert code == 0
    assert document["schema"] == proto_constants.SCHEMA
    assert document["ok"]


def test_a_table_that_does_not_exits_one_with_the_problem_printed(tmp_path):
    claims = tmp_path / "claims.json"
    claims.write_text(json.dumps({"fields": [["Trace", "packet", 2]]}))

    code, document = run(claims)

    assert code == 1
    assert document["problems"][0]["claim"] == "Trace.packet"


def test_claims_that_cannot_be_read_exit_two_not_one(tmp_path):
    """1 means "your table is wrong". A checker that could not read its input has said nothing
    about the table, and must not spend that code saying it."""
    missing = tmp_path / "nothing-here.json"
    assert run(missing)[0] == 2

    malformed = tmp_path / "malformed.json"
    malformed.write_text("{not json")
    assert run(malformed)[0] == 2


def test_a_claim_of_the_wrong_shape_exits_two(tmp_path):
    """A three-element field claim missing its number is a broken caller, not a wrong number."""
    claims = tmp_path / "claims.json"
    claims.write_text(json.dumps({"fields": [["Trace", "packet"]]}))

    assert run(claims)[0] == 2
