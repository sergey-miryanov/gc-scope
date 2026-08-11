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

A message is named by its path from the file: `ClockSnapshot.Clock` for a nested one. An enum
declared at file scope rather than inside a message is claimed against the empty scope `""`.

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
    """Every top-level message in the Perfetto trace schema, by name.

    Nested messages are not in here and are not meant to be: they are reached through their
    parent by [`_resolve_message`], so `Clock` on its own stays an unknown message rather than
    resolving to whichever enclosing message happens to declare one.
    """
    from perfetto.protos.perfetto.trace import perfetto_trace_pb2

    return dict(perfetto_trace_pb2.DESCRIPTOR.message_types_by_name)


def _file_enums() -> dict:
    """Every enum the file declares outside any message, by name — `BuiltinClock` among them.

    Perfetto puts the clock domain ids at file scope, so a table naming one has no message to
    hang it off. Claims spell that scope as the empty string.
    """
    from perfetto.protos.perfetto.trace import perfetto_trace_pb2

    return dict(perfetto_trace_pb2.DESCRIPTOR.enum_types_by_name)


def _resolve_message(name: str, messages: dict):
    """A message by its path from the file: `TracePacket`, or `ClockSnapshot.Clock`.

    `None` for anything the schema does not declare, including a path whose first step exists
    and whose second does not — the caller reports that as an unknown message, which is what it
    is. A dotted name is the only way to reach a nested message here, deliberately: resolving a
    bare `Clock` by searching every parent would answer a question nobody asked.
    """
    head, _, rest = name.partition(".")
    descriptor = messages.get(head)
    while descriptor is not None and rest:
        head, _, rest = rest.partition(".")
        descriptor = descriptor.nested_types_by_name.get(head)
    return descriptor


def check_fields(claims: list, messages: dict) -> list[dict]:
    """Each `(message, field, number)` against the schema's own number for that field."""
    problems = []
    for message, field, number in claims:
        where = f"{message}.{field}"
        descriptor = _resolve_message(message, messages)
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


def check_enums(claims: list, messages: dict, file_enums: dict) -> list[dict]:
    """Each `(scope, enum, value, number)` against the schema's own value for that name.

    `scope` is the message declaring the enum, or the empty string for one the file declares
    outside any message. Both spellings report the same way, so a table does not have to know
    which kind it is looking at until the schema disagrees with it.
    """
    problems = []
    for message, enum, value, number in claims:
        where = f"{message}.{enum}.{value}" if message else f"{enum}.{value}"
        if message:
            descriptor = _resolve_message(message, messages)
            if descriptor is None:
                problems.append(_problem(where, f"no message named {message} in the schema"))
                continue
            enum_descriptor = descriptor.enum_types_by_name.get(enum)
        else:
            enum_descriptor = file_enums.get(enum)
        if enum_descriptor is None:
            where_from = message or "the trace schema"
            problems.append(_problem(where, f"{where_from} declares no enum named {enum}"))
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
    problems = check_fields(fields, messages) + check_enums(enums, messages, _file_enums())
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
