# The JSON summary

`--summary-json` writes what `--summary` prints, as one JSON document, so a CI job can
threshold on GC behaviour and another tool can read the run without parsing a table.

```powershell
gcscope run -s bench.py --summary-json gc-summary.json
gcscope monitor 12345 --summary-json -     # `-` is stdout
```

`monitor` keeps stdout to itself, so `-` there gives a consumer the document and nothing
else. `run` forwards the target's own stdout to gcscope's, so it refuses `-` and asks for a
path instead.

The two flags are independent: pass both to watch the table and keep the document.

## The document

```json
{
  "schema": "gcscope.gc-summary/1",
  "interpreters": [
    {
      "pid": 32508,
      "interpreter": 0,
      "generations": [
        {"generation": 0, "collections": 205, "collected": 405600, "uncollectable": 0, "records": 88, "observed": 88, "lost": 117, "coverage": 0.4292682926829268, "pause_total_ns": 30931300, "pause_measured_ns": 13298300, "pause_mean_ns": 150884.39024390245, "scale_factor": 2.325958957159938},
        {"generation": 1, "collections": 29, "collected": 30240, "uncollectable": 0, "records": 23, "observed": 23, "lost": 6, "coverage": 0.7931034482758621, "pause_total_ns": 2206100, "pause_measured_ns": 1263400, "pause_mean_ns": 76072.41379310345, "scale_factor": 1.7461611524457812}
      ]
    }
  ]
}
```

Each entry of `interpreters` covers one interpreter of one process over the span gcscope
watched it. A process running sub-interpreters gets one entry each, and a monitored process
tree one per interpreter per process, so no figure is a tree-wide total. Two entries can
share a `pid`.

Every figure covers the **accounted span**: from the first Record gcscope read of that
generation to the last. Collections that ran before the attach are outside it.
[ADR 0019](adr/0019-loss-is-accounted-over-the-observed-span.md) has the reasoning.

## Fields

| Key | Meaning |
|---|---|
| `generation` | The GC generation, `0`–`2` |
| `collections` | Collections that ran, from CPython's own counter. Exact under any amount of Loss |
| `collected` | Objects collected. Excludes the opening Record, whose predecessor was never read |
| `uncollectable` | Uncollectable objects, on the same basis |
| `records` | Records gcscope read |
| `observed` | Collections a Record was read for. `0` on a build whose Entries describe no single Collection |
| `lost` | Collections no Record was read for. `collections` is always `observed + lost` |
| `coverage` | `observed / collections`, in `[0, 1]`. Says whether a figure derived from the Records describes the run or a biased sample of it |
| `pause_total_ns` | Pause over the span, from the target's cumulative accumulator: what ran, not what was read |
| `pause_measured_ns` | Pause summed over the Records read: as much of the total as gcscope watched |
| `pause_mean_ns` | `pause_total_ns / collections` |
| `scale_factor` | `pause_total_ns / pause_measured_ns`. Multiply a measured figure that partitions the pause to estimate its exact counterpart. Never a percentile — the sample behind one is biased, and scaling it makes the bias look like a measurement |

Timestamps are nanoseconds, as CPython publishes them.

## Absence is the schema's load-bearing rule

**A figure the build cannot supply has no key.** Below 3.15 CPython publishes no GC
timestamps, so those targets carry no `pause_*` key and no `scale_factor`. A check
thresholding on pause time fails to find the field rather than passing against a zero it
would read as "this process spends no time in GC".

Test the key's presence, never its value:

```python
pause = generation.get("pause_total_ns")
if pause is None:
    raise SystemExit("this build publishes no pause timing; the threshold cannot be checked")
```

`coverage` is present on both tiers. Its `0` on a pre-3.15 target is an answer, not a
placeholder: the counts are exact and nothing behind them describes a single Collection.

Only the `pause_*` keys and `scale_factor` vary with the build. The rest are in every
generation object, behind one backstop: the encoder drops any figure that is not a finite
number rather than writing something no JSON parser accepts. Nothing gcscope reconstructs can
produce one today, so reading `coverage` directly is safe; `.get` is what makes a consumer
certain.

## What reads it

The pyperf hook of [spec 0011](../specs/0011-loss-reconstruction-and-gc-statistics.md) reads
this document to put GC metrics into a benchmark's metadata. It runs the benchmark under
`gcscope run`, then folds the summary:

```python
import json


def gc_metadata(path):
    """GC metrics for one benchmarked process, from a gcscope JSON summary."""
    document = json.load(open(path))
    if document["schema"].rsplit("/", 1)[1] != "1":
        raise SystemExit(f"unsupported summary schema {document['schema']}")

    # The benchmark runs in the interpreter gcscope spawned, which is the first block.
    generations = document["interpreters"][0]["generations"]

    metadata = {
        "gc_collections": sum(g["collections"] for g in generations),
        "gc_collected": sum(g["collected"] for g in generations),
        # Weighted by what each generation ran, so a busy gen 0 is not averaged away.
        "gc_coverage": sum(g["coverage"] * g["collections"] for g in generations)
        / max(sum(g["collections"] for g in generations), 1),
    }

    # Absent on a build with no timing, and a metric this run cannot supply is one the
    # benchmark record should not carry.
    if all("pause_total_ns" in g for g in generations):
        metadata["gc_pause_total_ns"] = sum(g["pause_total_ns"] for g in generations)

    return metadata
```

Summing across generations is this consumer's choice, not the schema's: the document keeps
them apart because they collect at wildly different rates.

## Versioning

`schema` names the document and its version. Coverage and the exact counts are in version
`1` rather than arriving in a later one, so a consumer that pins `1` gets the reconstruction
rather than the observed counts alone.

Adding a key is not a new version; a consumer must ignore keys it does not know, and must
not treat a missing key as an error unless it needs that figure. Renaming or removing one
is, and the byte-for-byte test at the bottom of `src/monitor/summary_json.rs` is what makes
either deliberate.
