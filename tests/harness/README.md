# The Perfetto verification harness

`perfetto_dump.py` reads a Perfetto trace and prints what is in it, as JSON, for another test
to assert on. It exists because a hand-rolled protobuf encoder fails **open**: without a
decoder gcscope did not write, a wrong field number produces a file that parses *to something
else* rather than a file that does not parse — the failure mode
[ADR 0004](../../docs/adr/0004-per-platform-image-layout.md) warns about.

The harness is a translator, not a judge of content. It knows the Perfetto format; it knows
nothing about GC. What tracks should exist, what a slice should be called, which counters
should share an axis — those are expectations, and they live in Rust beside the encoder that
produced them. **Reading this directory should not reveal what gcscope emits.**

## Running it

```powershell
pip install -r tests/harness/requirements.txt

# the harness itself
python -m pytest tests/harness
python -m pytest tests/harness -m "not trace_processor"   # skip the query engine

# on a trace
python tests/harness/perfetto_dump.py trace.perfetto-trace
python tests/harness/perfetto_dump.py trace.perfetto-trace --skip-trace-processor

# on a table of field numbers
python tests/harness/proto_constants.py claims.json

# from Rust
cargo test --test perfetto_harness -- --ignored     # the seam
cargo test --test perfetto_encoder -- --ignored     # gcscope's encoder through it
```

Rust reaches it through `tests/common/perfetto.rs`, which runs the script and hands back the
parsed document. That module is the seam later Perfetto tests assert through, so nobody
re-solves the process boundary or the parsing.

## The document

```json
{
  "schema": "gcscope.perfetto-dump/1",
  "bytes": 412,
  "validity": {"ok": false, "problems": [{"check": "…", "detail": "…", "packet": 4}]},
  "wire": {"packet": [ … ]},
  "trace_processor": {"slice": [ … ], "process": [ … ], "track": {"error": "…"}}
}
```

`wire` is `null` when the bytes did not parse. `trace_processor` is `null` when the query
engine was skipped, and any single query that fails becomes `{"error": …}` under its own name
rather than taking the dump down — trace-processor's columns move between releases, and a
caller that asked about slices should still get them on the day the `track` schema changes.
The failure is in the document, so nobody mistakes a missing view for an empty one.

Exit codes: **0** the trace is structurally valid, **1** it is not, **2** the harness could not
run. A rejection is data either way, printed with its problems listed.

## Three readers

| reader | from | answers |
|---|---|---|
| `wire` | `perfetto_trace_pb2` | is this the field I meant to write |
| `trace_processor` | the binary the Perfetto UI runs | does a wire-valid trace *mean* what it should |
| `proto_constants.py` | the same generated descriptors | is this the number that field actually has |

Fields the trace processor does not expose as columns — `sibling_order_rank` among them — come
from the wire view. Do not "fix" a missing column by asserting on something else that happens
to exist.

`proto_constants.py` is the narrowest of the three, and the only one that reads no trace. It
takes claims — `(message, field, number)` and `(scope, enum, value, number)` — from a file or
stdin, and answers whether the schema agrees, so a table of field numbers transcribed by hand
in another language is re-derived from the descriptors on every run. It knows nothing about who
is asking or what they intend to write into those fields. Same exit codes: **0** every claim
holds, **1** at least one does not, **2** it could not run.

A message is named by its path from the file, so a nested one is `ClockSnapshot.Clock` and the
bare `Clock` stays unknown — resolving it by hunting through every parent would answer a question
nobody asked. An enum the file declares outside any message, `BuiltinClock` among them, takes the
empty scope `""`.

Its answer is only as current as the pinned `perfetto`. A field added or moved upstream is
invisible here until someone bumps `requirements.txt` — the guarantee is against the schema
someone checked in, not against the world.

## Three rendering decisions

`MessageToDict` supplies the rendering, so the grammar comes from the `.proto` rather than from
us. Three of its behaviours are decisions, each silent when wrong, each with a test in
`test_perfetto_dump.py`:

1. **`preserving_proto_field_name=True`.** Names arrive as the schema spells them. Without it
   every name is camelCased and stops matching the field table an encoder was written against.
2. **`always_print_fields_with_no_presence=False`.** A field nobody wrote stays absent. The
   alternative prints every unwritten field at its default, drowning the one signal this view
   is for — a producer wrote something nobody expected — in zeros it did not write. Safe here
   in a way it would not be under proto3 defaults: Perfetto's fields carry explicit presence,
   so a `0` that was written is rendered and a field never touched is omitted.
3. **64-bit fields render as JSON strings.** Protobuf's rule rather than a choice, and the
   right one: a nanosecond timestamp is around 1.7×10¹⁸ and an IEEE double holds integers
   exactly only to 9×10¹⁵. Callers parse them from strings.

## Structural checks

Each check names itself in the problem it reports, so a caller asserts on the name rather than
on the wording of a message. Every one has a test with a hand-built trace it must reject.

| check | rule |
|---|---|
| `parses` | the bytes decode as a `Trace` |
| `sequence_id` | every packet names the sequence it belongs to |
| `track_uuid` | every track a `TrackEvent` names was declared by some `TrackDescriptor` |
| `slice_pairing` | on each track, every begin closes and no end arrives unopened |

`sequence_id` is stricter than the format demands — Perfetto tolerates packets outside any
sequence. Interning and incremental state are sequence-scoped, so a packet that escapes its
sequence is far more likely a producer bug than a design, and a producer that genuinely needs
one should be made to say so here. `track_uuid` resolves a declaration made *anywhere* in the
trace, since descriptors and events may be interleaved.

Both track checks resolve **per-sequence defaults**: a producer may state
`trace_packet_defaults.track_event_defaults.track_uuid` once and leave the field off every
event after it, which is Perfetto's standard way of not repeating a uuid on thousands of
packets. Reading only the explicit field calls that pattern malformed, and files every such
event under track `0`, where a begin on one track pairs with an end on another — so the pairing
check would go quiet exactly where it matters. Defaults hold until the sequence states new
ones; `previous_packet_dropped` resetting them is not modelled.

## Reference traces

`reference_traces.py` builds them with the protobuf bindings, at call time rather than as
committed bytes, so the reference and the schema cannot drift apart on disk. A decoder whose
only input has ever been the encoder it checks is a weak oracle: the two agree by construction
and neither has been shown a third opinion.

None of them is a GC trace. The well-formed one is a process running a slice called `Work` with
a `Detail` inside it and a counter that samples twice. The rest are the malformed cases the
checks above must reject — a validity check nobody has watched reject something is a check that
might only ever say yes.

## Why the dependencies live here

`requirements.txt` pins them exactly. The trace bytes do not depend on which interpreter a
target runs, so this gains nothing from a range and loses reproducibility.

They are deliberately **not** in `gcscope_probe/pyproject.toml`. That package is a distributable
C extension whose `requires-python = ">=3.13,<3.15"` is an ABI gate on internal struct layout;
a harness that reads a file format has no business inheriting an interpreter bound from it.
