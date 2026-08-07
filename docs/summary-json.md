# The JSON summary

`--summary-json` writes what `--summary` prints, as one JSON document, so a CI job can
threshold on GC behaviour and another tool can read the run without parsing a table. Pass both
flags to get the table and the document.

```powershell
gcscope run -s bench.py --summary-json gc-summary.json
gcscope monitor 12345 --summary-json -     # `-` is stdout
```

`monitor` keeps stdout to itself, so `-` there gives a consumer the document and nothing else.
`run` forwards the target's own stdout to gcscope's, so it refuses `-` and asks for a path.

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

Each entry of `interpreters` covers one interpreter of one process. Sub-interpreters get one
each and a monitored tree one per interpreter per process, so no figure is a tree-wide total,
and two entries can share a `pid`. A process is covered for 1024 interpreters at a time,
dropping whichever it has seen least recently, so an interpreter that stopped collecting
early in a churning run can be missing while every one still running is kept. gcscope warns
on stderr when that happens, and never for a workload that is not creating interpreters by
the thousand.

Every figure covers the **accounted span**, from the first Record gcscope read of that
generation to the last. Collections that ran before the attach sit outside it, for the reasons
in [ADR 0019](adr/0019-loss-is-accounted-over-the-observed-span.md).

## Fields

| Key | Meaning |
|---|---|
| `generation` | The GC generation, `0`–`2` |
| `collections` | Collections that ran, from CPython's own counter. Exact under any amount of Loss |
| `collected` | Objects collected. Excludes the opening Record, whose predecessor was never read |
| `uncollectable` | Uncollectable objects, on the same basis |
| `records` | Records gcscope read |
| `observed` | Collections a Record was read for. `0` where an Entry describes no single Collection |
| `lost` | Collections no Record was read for. `collections` is always `observed + lost` |
| `coverage` | `observed / collections`, in `[0, 1]`. Says whether the figures beside it describe the run or a biased sample of it |
| `pause_total_ns` | Pause over the span, from the target's cumulative accumulator: what ran, not what was read |
| `pause_measured_ns` | Pause summed over the Records read |
| `pause_mean_ns` | `pause_total_ns / collections` |
| `scale_factor` | `pause_total_ns / pause_measured_ns`. Scales a measured figure that partitions the pause, never a percentile: the sample behind one is biased, and scaling it makes the bias look like a measurement |

Timestamps are nanoseconds, as CPython publishes them.

## Absent keys

**A figure the build cannot supply has no key.** Below 3.15 CPython publishes no GC
timestamps, so those targets carry no `pause_*` and no `scale_factor`. A check thresholding on
pause time fails to find the field rather than passing against a zero it reads as "this
process spends no time in GC".

```python
pause = generation.get("pause_total_ns")
if pause is None:
    raise SystemExit("this build publishes no pause timing; the threshold cannot be checked")
```

Only the `pause_*` keys and `scale_factor` vary with the build. `coverage` rides both tiers,
and its `0` on a pre-3.15 target is an answer: the counts are exact and nothing behind them
describes a single Collection. One backstop covers the rest, the encoder dropping any figure
that is not a finite number rather than writing something no parser accepts, so `.get` is what
makes a consumer certain.

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

    # A metric this run cannot supply is one the benchmark record should not carry.
    if all("pause_total_ns" in g for g in generations):
        metadata["gc_pause_total_ns"] = sum(g["pause_total_ns"] for g in generations)

    return metadata
```

Summing across generations is this consumer's choice. The document keeps them apart because
they collect at wildly different rates.

## Versioning

`schema` names the document and its version. Coverage and the exact counts are in version `1`
rather than a later one, so a consumer pinning `1` gets the reconstruction rather than the
observed counts alone.

Adding a key is not a new version: ignore keys you do not know, and treat a missing one as an
error only where you need that figure. Renaming or removing a key is, and the byte-for-byte
test in `src/monitor/summary_json.rs` is what makes either deliberate.
