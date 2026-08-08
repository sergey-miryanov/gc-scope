# 0018 — Export Perfetto traces

- **Status:** Not started
- **Kind:** feature — enhancement
- **Effort:** L
- **Origin:** Named as the next increment by spec 0011 (now deleted) §6, which deferred it so
  that the Loss arithmetic landed against an exporter that already worked. Background:
  [`docs/research/gcmon-inventory.md`](../docs/research/gcmon-inventory.md) §2.7, the largest
  single thing gcscope lacks and gcmon has.
- **Respects:** [ADR 0008](../docs/adr/0008-reader-consumer-package-layering.md) (layering),
  [ADR 0018](../docs/adr/0018-presentation-belongs-to-the-consumer.md) (presentation belongs to
  the consumer), and the rule the `TraceEvent` model exists to enforce: **an exporter encodes
  what it is handed and never converts from `GcStat` itself**

## 1. Problem statement

An operator gets one output format, Chrome JSON, and it is the weaker of the two the ecosystem
offers. Chrome traces are plain text that grows linearly with the run, load slowly past a few
hundred thousand events, and have no way to express a track that belongs to something other
than a process or a thread. Perfetto's UI is where trace analysis has moved, its binary
protobuf is a fraction of the size, and it can express track hierarchy, counter grouping and
shared Y axes that Chrome cannot.

The gap is not that Perfetto is nicer to look at. Two features gcscope wants next need track
concepts Chrome does not have: Loss drawn as intervals on a track of its own (spec 0019), and
process liveness on a shared `Processes` track. Both are blocked on this.

## 2. Solution

`gcscope monitor` and `gcscope run` gain `--format`, accepting `chrome`, `perfetto` or both.
Perfetto output is a binary `.perfetto-trace` file that opens in ui.perfetto.dev, carrying the
same Collections, sub-phases and counters as the Chrome trace, laid out so that the generations
group together, counters share axes where they are comparable, and a process with no GC
activity still appears rather than vanishing.

Existing Chrome captures are unaffected. `--format chrome` is the default and its bytes do not
move.

## 3. User stories

1. As an operator with a long capture, I want a Perfetto trace, so that the viewer opens it
   without the wait a large Chrome JSON costs.
2. As an operator, I want both formats from one run, so that I do not choose between the
   viewer I prefer and the one my colleague uses.
3. As an operator, I want each generation's counters grouped and axis-shared where they are
   comparable, so that G0/G1/G2 read against each other rather than at three unrelated scales.
4. As an operator monitoring a tree, I want a process that produced no GC activity to still
   appear, so that "this worker did nothing" is distinguishable from "this worker is missing".
5. As an operator already using `--output`, I want the flag to keep meaning what it means, so
   that adding a format does not rewrite my command line.
6. As a gcscope maintainer, I want the Perfetto exporter to consume `TraceEvent` and nothing
   else, so that the two formats cannot describe one run differently.
7. As a gcscope maintainer, I want the wire encoding verified against a real decoder, so that a
   wrong field number produces a test failure rather than a file that parses to the wrong
   thing.
8. As a gcscope maintainer, I want the layout policy's reasons carried across with it, so that
   the next person does not rediscover them by getting it wrong.

## 4. Implementation decisions

### The exporter is a second `EventsExporter`, and converts nothing

`monitor/convert.rs` stays the only place a `GcStat` becomes a `TraceEvent`. The Perfetto
exporter receives `TraceEvent`s and emits bytes, exactly as `ChromeTraceExporter` does. gcmon
reached this the expensive way and recorded it in its ADR-0007: two paths converting
independently drifted into two disagreeing traces of one run.

The `--format` fan-out is a combining exporter holding two others, which the trait already
supports.

### The layout policy is the valuable part, and it ports verbatim

The encoding is mechanical. What is hard-won sits in gcmon's `perfetto_format.py` and
`perfetto_proto.py`, and the inventory is explicit that the comments come across unchanged:

- the uuid-0 root descriptor that makes explicit process and thread ordering possible;
- the non-OS-scoped `GC Metrics` group track, which exists *only* because trace-processor
  ignores `sibling_order_rank` on process and thread tracks;
- `DebugAnnotation.name` being field **10**, not 1, because field 1 became an interned IID;
- the `Start Process` instant, without which a silent process's track is hidden by the UI;
- `y_axis_share_key`, so `G0/G1/G2 collected` share an axis;
- `heap_size` promoted to a top-level counter.

Each of these is a record of something that was wrong once. Porting the code without the
comment discards the finding and keeps the workaround.

### Hand-rolled protobuf or `prost` — decide first

gcmon hand-rolls it (its ADR-0001): 62 lines of varint, zigzag, fixed64, string and bytes
writers, plus hand-maintained field numbers guarded by a dedicated test. That suits gcscope's
lean dependency list, and it is the same call gcscope made about JSON.

The counter-argument is stronger here than it was for JSON, and the inventory states it: gcmon
validates its hand-rolled output against the real trace processor in CI, and gcscope has no
such leg. Hand-rolled protobuf without a differential test against a real decoder fails
**open** — it produces a file that parses to something *different* rather than a file that does
not parse. That is the failure mode ADR 0004 exists to warn about.

So the decision is not "hand-rolled or `prost`" alone. It is that pair: hand-rolled requires a
verification leg that decodes the output with something gcscope did not write.

### Timestamp domains must be stated, not mixed silently

GC events carry the target's clock; anything the Observer generates carries the Observer's.
gcmon mixes both in one trace without saying so (inventory §7 of the open questions), and
`mark_process_lifecycle` in gcscope already takes a `ts_ns` that all three call sites pass `0`
for. Whatever this exporter does about that, the trace has to say which clock a track is on.

## 5. Seams and testing decisions

- **Seam:** the encoder seam — `TraceEvent`s in, bytes out. Created by the `TraceEvent`
  extraction and already used by `ChromeTraceExporter`'s tests.
- **New seam needed:** none.
- **What makes a good test here:** wire-level. Bytes decoded back into fields, compared against
  what the events said, by a decoder gcscope did not write. A golden-file pin catches drift but
  cannot catch a field number that was wrong from the first commit.
- **Prior art:** the byte-for-byte gates at the bottom of `exporters/chrome.rs`; gcmon's
  `perfetto_proto.py` field-number test.
- **Cases:**
  1. One run through both exporters describes the same Collections, sub-phases and counters.
  2. A real decoder reads the output and recovers every track, slice and counter the events
     carried.
  3. A process with no GC activity appears in the trace.
  4. The regression guard: `--format chrome` output is byte-identical to what shipped before
     this spec.

## 6. Out of scope

- **Loss drawn as intervals.** Spec 0019, blocked on this one.
- **The `Processes` liveness track and RSS sampling.** Both want this exporter first; both are
  separately useful and separately specifiable.
- **The JSONL and stdout exporters**, and `combine`/`convert`.
- **The control plane and the pyperf hook.**

## 7. Further notes

**Open question inherited from spec 0011 §7:** whether Chrome output must stay compatible with
existing gcmon captures and the analysis notebooks that read them. It was not answerable while
one format existed; the moment there are two, it becomes "which format is canonical for those
notebooks", which is this spec's to settle. The evidence today says compatibility is intact:
the inventory records gcscope's Chrome events as byte-compatible with gcmon's
`trace_converter`, same names, categories, args and counter tracks, and the only Chrome bytes
that have moved since are additional `thread_name` lines where two tracks previously collided.
