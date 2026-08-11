# 0021 — The Perfetto encoder is hand-rolled, and a decoder gcscope did not write is what makes that safe

**Status:** Accepted — implemented 2026-08-09. First step of the Perfetto exporter, alongside
[ADR 0008](0008-reader-consumer-package-layering.md)'s layering and the fan-out
`monitor/exporters/combining.rs` already carries.

## Context

gcscope writes Chrome Trace JSON by hand: the shapes are small and fixed, the crate has no
serde dependency, and a byte-for-byte gate pins the output. Perfetto's binary format is the
second output an operator asked for, and it raises the same question with a different answer
available. `prost` (or `protobuf-codegen`) would generate message types from Perfetto's
`.proto` files and serialize them, and the numbers would be right by construction.

The slice of the format gcscope needs is small — varints, length-delimited submessages, and
about forty field numbers. gcmon writes the same traces from a 62-line encoder plus a
hand-maintained table, and that table is directly transcribable.

What makes the choice a real trade-off is how a hand-rolled encoder fails. gcmon shipped four
bugs in this exact code: `TrackEvent.type` and `track_uuid` renumbered upstream, timestamps
written inside `TrackEvent` where field 1 is a `timestamp_delta_us` oneof member,
`DebugAnnotation.name` moving from field 1 to field 10, and a varint masked to 32 bits that
wrote `sibling_order_rank = -1` as `0`. Every one of them produced a trace that **parsed**. It
rendered wrong, and a human opening the UI is what caught it. That is the failure mode
[ADR 0004](0004-per-platform-image-layout.md) names for image layout — it fails *open*, the
read lands somewhere plausible and returns garbage — arriving in a second subsystem.

Generating from the `.proto` would remove the field-number risk and add: a build dependency
that compiles protobuf schemas, a vendored copy of Perfetto's `.proto` tree to keep current,
and a generated-code layer between `TraceEvent` and the bytes. gcscope already owns one
hand-maintained table of numbers-that-must-be-right — the CPython struct offsets — and the
whole of `remote_debugging/offsets/` is the machinery for keeping it honest.

## Decision

**The encoder is hand-rolled, and every constant it writes with is re-derived from the official
descriptors on a CI leg. Neither half ships without the other.**

1. **Three layers, importing one way only.** `protobuf.rs` holds the wire primitives and knows
   no field numbers; `perfetto_proto.rs` holds the field numbers and knows no wire format;
   `perfetto.rs` writes messages out of both. The direction is what keeps the wire layer small
   enough to audit against upstream, which is the reason for hand-rolling at all.
2. **The field-number table and its proof are one declaration.** A macro in `perfetto_proto.rs`
   emits each constant *and* its entry in the list handed to the checker, so a constant that
   nothing asserts cannot be written. gcmon's equivalent is two files that a careless commit
   can separate.
3. **The checker is the harness's, not gcscope's.** `tests/harness/proto_constants.py` takes
   `(message, field, number)` claims and answers from `perfetto_trace_pb2`. It knows nothing
   about who is asking or what they intend to write, which is the same rule the dump harness
   follows: reading `tests/harness/` should not reveal what gcscope emits.
4. **Every primitive is decoded back by the `perfetto` package**, at the values where an
   encoder goes wrong — the varint boundaries, a negative that a 32-bit mask changes, a double
   whose bits are not its decimal, a string whose byte length is not its character count, an
   empty submessage.
5. **`perfetto` is a test dependency and never a runtime one.** Nothing under `src/` may import
   a generated message class. A line such as `use perfetto_trace_pb2::CounterDescriptor` would
   work, would be tempting, and would undo the decision.
6. **Timestamps live on `TracePacket.timestamp`, and every packet names its sequence.** Two of
   gcmon's four bugs are these two rules; they are in `perfetto_proto.rs`'s comments as well as
   here.
7. **`--output` names the Chrome path, and a format whose bytes are not JSON resolves its own.**
   `--format perfetto -o run.json` writes `run.perfetto-trace`. `.json` is the one extension
   replaced, because it is the one that would lie about the contents; any other suffix is
   appended to, since `-o capture.2026-08-09` is a name an operator chose rather than a claim
   about the format, and replacing it would fold two days' captures into one file. The
   resolution happens in the fan-out rather than in each encoder, so two formats given one
   `--output` write two files instead of truncating each other into one.

## Consequences

- gcscope gains no build-time codegen, no vendored `.proto` tree, and no serialization crate.
  The `perfetto` package stays a `requirements.txt` line for one CI leg.
- Upstream renumbering is caught by a test rather than by an operator, and it is one edit in one
  file when it happens. It is caught only on the leg that has the package: a laptop running
  `cargo test` sees the encoder's own unit tests and not the schema.
- **The checker is only as current as the pinned `perfetto`.** `requirements.txt` pins it
  exactly, so a field added or moved upstream is invisible until someone bumps the pin. That is
  the same shape as the layout sweep in [ADR 0011](0011-layout-equivalence-sweep.md): the
  guarantee is against the schema someone checked in, not against the world.
- The pin is `0.57.2` rather than `0.56.0` because `TrackDescriptor.process_ordering` and
  `thread_ordering` do not exist in the older schema, and a constant with nothing to check
  against is a constant nobody is guarding.
- A negative `int32` decodes the same whether it was sign-extended to ten bytes or truncated to
  five, because the reader takes the low 32 bits either way. So the ten-byte form the spec asks
  for is pinned by a unit test on the bytes, and the *value* assertions through the decoder
  catch it only on an `int64` field. Both are kept; neither alone covers it.
- A trace is the concatenation of its packets, with no header, length or index, so the encoder
  appends as it goes and a capture killed mid-run leaves a readable prefix. Chrome's array
  terminator has no counterpart here, which is why `close` only flushes.
- The uuid-0 root descriptor is written on `open` rather than before the first event, so a run
  that observed nothing still produces a file a reader opens.
- Two formats now answer differently to the same `--output`, so the line a run prints names the
  files it opened rather than the path that was typed.
- The Perfetto output is pinned byte-for-byte the way the Chrome output is, at the bottom of
  `exporters/perfetto.rs`. Ascending field order and a fixed sequence id are decisions, and
  packet counts and length prefixes both survive breaking them.

## Alternatives considered

- **`prost` with Perfetto's `.proto` files.** Rejected for the dependency and the vendored
  schema tree, given that the checker recovers most of the safety a generated encoder would
  have. Worth revisiting if gcscope starts writing a substantially larger slice of the format,
  or if interning arrives — that adds state the hand-rolled path would have to grow.
- **Assert the field numbers in Rust against a copy of the schema.** A copy is the thing being
  checked, so the test would agree with the table by construction. The numbers have to come
  from outside the repo.
- **Round-trip the encoder's output through a reader gcscope owns.** The failure this whole ADR
  is about survives it: a wrong constant is read back through the same wrong constant. gcmon's
  four bugs all passed their round-trip tests.
