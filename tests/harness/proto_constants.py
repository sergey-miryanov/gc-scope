"""Check a hand-maintained table of Perfetto field numbers against the official descriptors.

An encoder that writes protobuf by hand owns a table of field numbers, and a wrong one fails
**open**: the trace still parses, it just says something else. Nothing in the producer's own
output can catch that, because its reader and its writer agree by construction. This re-derives
each number from the schema that generated `perfetto_trace_pb2`.

It knows nothing about who is asking. A claim is `(message, field, number)` or
`(message, enum, value, number)`, and the answer is whether the schema agrees. What the caller
intends to *write* into those fields is not visible here, and should not be.

Usage:
    python proto_constants.py CLAIMS      # a JSON file, or `-` for stdin

The claims document:
    {"fields": [["TracePacket", "timestamp", 8], ...],
     "enums":  [["TrackEvent", "Type", "TYPE_SLICE_BEGIN", 1], ...]}

Exit codes: 0 every claim holds, 1 at least one does not, 2 the checker could not run (bad
arguments, unreadable or malformed claims, missing dependency). The verdict is printed on
stdout as JSON either way, so a caller reads *which* claim was wrong rather than inferring it.
"""

from __future__ import annotations

import argparse
import json
import sys
import traceback
from pathlib import Path

SCHEMA = "gcscope.proto-constants/1"


def _problem(claim: str, detail: str) -> dict:
    return {"claim": claim, "detail": detail}


def _messages() -> dict:
    """Every message in the Perfetto trace schema, by name.

    Top-level only, which is every message a field-number table names. A nested one would have
    to be reached through its parent, and asking for it here reports an unknown message rather
    than resolving something that happens to share a name.
    """
    from perfetto.protos.perfetto.trace import perfetto_trace_pb2

    return dict(perfetto_trace_pb2.DESCRIPTOR.message_types_by_name)


def check_fields(claims: list, messages: dict) -> list[dict]:
    """Each `(message, field, number)` against the schema's own number for that field."""
    problems = []
    for message, field, number in claims:
        where = f"{message}.{field}"
        descriptor = messages.get(message)
        if descriptor is None:
            problems.append(_problem(where, f"no message named {message} in the schema"))
            continue
        actual = descriptor.fields_by_name.get(field)
        if actual is None:
            problems.append(_problem(where, f"{message} has no field named {field}"))
        elif actual.number != number:
            problems.append(
                _problem(where, f"claimed field {number}, the schema says {actual.number}")
            )
    return problems


def check_enums(claims: list, messages: dict) -> list[dict]:
    """Each `(message, enum, value, number)` against the schema's own value for that name."""
    problems = []
    for message, enum, value, number in claims:
        where = f"{message}.{enum}.{value}"
        descriptor = messages.get(message)
        if descriptor is None:
            problems.append(_problem(where, f"no message named {message} in the schema"))
            continue
        enum_descriptor = descriptor.enum_types_by_name.get(enum)
        if enum_descriptor is None:
            problems.append(_problem(where, f"{message} declares no enum named {enum}"))
            continue
        actual = enum_descriptor.values_by_name.get(value)
        if actual is None:
            problems.append(_problem(where, f"{enum} has no value named {value}"))
        elif actual.number != number:
            problems.append(
                _problem(where, f"claimed value {number}, the schema says {actual.number}")
            )
    return problems


def check(claims: dict) -> dict:
    """The whole verdict for one claims document.

    `checked` is reported rather than left implicit: a caller that handed over 55 claims and is
    told 0 were checked has learnt something a bare "no problems" would have hidden.
    """
    messages = _messages()
    fields = claims.get("fields", [])
    enums = claims.get("enums", [])
    problems = check_fields(fields, messages) + check_enums(enums, messages)
    return {
        "schema": SCHEMA,
        "checked": {"fields": len(fields), "enums": len(enums)},
        "ok": not problems,
        "problems": problems,
    }


def render(verdict: dict) -> str:
    return json.dumps(verdict, sort_keys=True, indent=2)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("claims", help="a JSON claims document, or `-` for stdin")
    args = parser.parse_args(argv)

    try:
        text = sys.stdin.read() if args.claims == "-" else Path(args.claims).read_text()
        claims = json.loads(text)
    except (OSError, ValueError) as failure:
        print(f"could not read the claims: {failure}", file=sys.stderr)
        return 2

    # A checker that cannot run must not answer "no problems". Anything that fails here is this
    # file or its dependency, never the table it was given, so it takes an exit code of its own.
    try:
        verdict = check(claims)
    except ImportError as missing:
        print(f"the checker needs the `perfetto` package: {missing}", file=sys.stderr)
        return 2
    except Exception:  # noqa: BLE001 - the checker failing is its own exit code
        traceback.print_exc()
        print("the checker could not read the schema; this is not a verdict", file=sys.stderr)
        return 2

    print(render(verdict))
    return 0 if verdict["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
