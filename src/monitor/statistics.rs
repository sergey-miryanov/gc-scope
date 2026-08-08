//! The end-of-run summary: what each ring did over the span the Observer watched it.
//!
//! Read from the poll-time accumulator in [`super::cursor`], never from the written trace, so
//! a replayed stream of Records gives the same figures.
//!
//! Counts and pause totals are what ran: [`super::loss`] reconstructs them from CPython's own
//! cumulative counters, so they stay exact however many Records a poll missed. Coverage is the
//! share gcscope holds a Record for, and says whether a figure derived from those Records
//! describes the run or a biased sample of it.

use crate::monitor::cursor::Cursor;
use crate::monitor::loss::account;

/// What one generation of one interpreter did over the span the Observer watched it.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationSummary {
    pub generation: u32,
    /// Collections that ran over the span, from the target's counter rather than the Records
    /// read: exact under any amount of Loss.
    pub collections: i64,
    /// Objects collected across the span. Never counts the opening Record: CPython publishes a
    /// running total, and the total before it was never read.
    pub collected: i64,
    /// Uncollectable objects across the span, on the same basis as `collected`.
    pub uncollectable: i64,
    /// Records read. Below `collections` where Entries were overwritten between polls, and a
    /// different quantity from [`observed`](Self::observed) on the counter-only tier, where two
    /// snapshots witness no Collection between them.
    pub records: u64,
    /// Collections a Record was read for. `collections` is this plus [`lost`](Self::lost), the
    /// identity an auditor checks the reconstruction against.
    pub observed: i64,
    /// Collections no Record was read for. Overwritten unread on the ring tier, and every
    /// Collection in the span on the counter-only tier, where nothing describes one.
    pub lost: i64,
    /// The share of `collections` a Record was read for, in `[0, 1]`. Zero on a build whose
    /// Entries describe no Collection.
    pub coverage: f64,
    /// Pause over the span, from the target's cumulative accumulator. `None` where the layout
    /// carries no `duration`: absent, never zero (ADR 0017).
    pub pause_total_ns: Option<i64>,
    /// Pause summed over the Records read: as much of the total as gcscope watched.
    pub pause_measured_ns: Option<i64>,
    /// The multiplier taking a measured figure to its exact counterpart. For figures that
    /// partition the pause, never for a percentile. See [`super::loss::LossAccount`].
    pub scale_factor: Option<f64>,
}

impl GenerationSummary {
    /// Mean pause per Collection, in nanoseconds. Exact over exact, so Loss moves neither
    /// side of the division.
    pub fn pause_mean_ns(&self) -> Option<f64> {
        match (self.pause_total_ns, self.collections) {
            (Some(total), collections) if collections > 0 => {
                Some(total as f64 / collections as f64)
            }
            _ => None,
        }
    }
}

/// One interpreter of one process. A busy sub-interpreter and a quiet one keep separate
/// blocks; nothing here is mixed across the two.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpreterSummary {
    pub pid: u32,
    pub interpreter: i64,
    pub generations: Vec<GenerationSummary>,
}

impl InterpreterSummary {
    /// Whether this build bounds its Collections with timestamps. One answer per interpreter:
    /// its generations share a build.
    ///
    /// Read off the measured figure, not the exact one, which a timed build can lack. Keying on
    /// the exact figure would drop `records` and `coverage` from such a build's table over a
    /// pause it cannot reconstruct.
    pub fn has_timing(&self) -> bool {
        self.generations
            .iter()
            .any(|g| g.pause_measured_ns.is_some())
    }
}

/// Fold the run's accumulators into one summary per interpreter.
///
/// They arrive ordered, so this walks them once and starts a block where the process or
/// interpreter changes.
///
/// The counts and the pause come from [`super::loss::account`]. What is left here is the two
/// Lifetime totals it does not carry, and the grouping.
pub fn summarize(cursor: &Cursor) -> Vec<InterpreterSummary> {
    let mut blocks: Vec<InterpreterSummary> = Vec::new();

    for (key, observation) in cursor.observations() {
        // The cursor folds a Record into every accumulator it creates, so a block never covers
        // a ring nothing is known about. Asserted rather than skipped: silently dropping the
        // row would hide the day that stops being true.
        debug_assert!(!observation.is_empty(), "a ring with no Record behind it");
        let (first, last) = (observation.first(), observation.last());
        let reconstructed = account(observation);
        let generation = GenerationSummary {
            generation: key.generation,
            collections: reconstructed.exact_collections,
            // Neither total takes the opening Record: CPython publishes each as a running
            // total, and the total before that Record was never read.
            collected: last.collected - first.collected,
            uncollectable: last.uncollectable - first.uncollectable,
            records: observation.sampled(),
            observed: reconstructed.observed_collections,
            lost: reconstructed.lost_collections,
            coverage: reconstructed.coverage,
            pause_total_ns: reconstructed.exact_pause_ns,
            pause_measured_ns: reconstructed.measured_pause_ns,
            scale_factor: reconstructed.scale_factor,
        };

        // A repeated generation closes the block. A PID forgotten and then observed again
        // holds two spans under one key, and one table with two `gen 0` rows says nothing
        // about which observation each belongs to.
        match blocks.last_mut() {
            Some(block)
                if block.pid == key.pid
                    && block.interpreter == key.interpreter
                    && !block
                        .generations
                        .iter()
                        .any(|g| g.generation == key.generation) =>
            {
                block.generations.push(generation);
            }
            _ => blocks.push(InterpreterSummary {
                pid: key.pid,
                interpreter: key.interpreter,
                generations: vec![generation],
            }),
        }
    }

    blocks
}

/// The summary as lines, ready to print.
///
/// Each block names the process and interpreter it covers, so no figure reads as a tree-wide
/// total. Its column set follows the tier: a build with no timing shows no pause, and no
/// `records` either, since a counter snapshot is not a Collection to compare a count against.
/// Coverage appears on both, being what says which of the two a reader is looking at.
pub fn render(summary: &[InterpreterSummary]) -> Vec<String> {
    if summary.is_empty() {
        return vec!["No GC collections were observed.".to_string()];
    }

    let mut lines = Vec::new();
    for block in summary {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "python {}, interpreter {}",
            block.pid, block.interpreter
        ));

        let timed = block.has_timing();
        let header = format_header(timed);
        let rule = "-".repeat(header.len());
        lines.push(rule.clone());
        lines.push(header);
        lines.push(rule);
        lines.extend(block.generations.iter().map(|g| format_row(g, timed)));
    }
    lines
}

/// The columns a tier shows, each with the width its heading and its figures share.
fn columns(timed: bool) -> Vec<(&'static str, usize)> {
    let mut columns = vec![
        ("gen", 3),
        ("collections", 12),
        ("collected", 12),
        ("uncollectable", 14),
    ];
    if timed {
        columns.push(("records", 8));
    }
    columns.push(("coverage", 9));
    if timed {
        columns.extend([("pause total", 14), ("pause mean", 14)]);
    }
    columns
}

/// The column header, matched to the set the tier selects.
fn format_header(timed: bool) -> String {
    let headings: Vec<String> = columns(timed)
        .iter()
        .map(|&(name, _)| name.to_string())
        .collect();
    lay_out(&headings, timed)
}

fn format_row(g: &GenerationSummary, timed: bool) -> String {
    let mut cells = vec![
        g.generation.to_string(),
        g.collections.to_string(),
        g.collected.to_string(),
        g.uncollectable.to_string(),
    ];
    if timed {
        cells.push(g.records.to_string());
    }
    cells.push(format!("{:.3}", g.coverage));
    if timed {
        cells.push(
            g.pause_total_ns
                .map_or_else(String::new, |ns| duration(ns as f64)),
        );
        cells.push(g.pause_mean_ns().map_or_else(String::new, duration));
    }
    lay_out(&cells, timed)
}

/// Right-align each cell under its column.
fn lay_out(cells: &[String], timed: bool) -> String {
    let columns = columns(timed);
    // `zip` yields the shorter side, so a column added to one and not the other slides every
    // figure past it one heading left, under a rule still cut to the header's width. Nothing
    // about the output would look wrong.
    debug_assert_eq!(cells.len(), columns.len(), "a column lost its cell");
    columns
        .iter()
        .zip(cells)
        .map(|(&(_, width), cell)| format!("{cell:>width$}"))
        .collect::<Vec<String>>()
        .join(" ")
}

/// A nanosecond figure at a readable scale, three decimals throughout so a column lines up.
fn duration(ns: f64) -> String {
    let scaled = |unit: &str, divisor: f64| format!("{:.3} {}", ns / divisor, unit);
    match ns.abs() {
        n if n >= 1e9 => scaled("s", 1e9),
        n if n >= 1e6 => scaled("ms", 1e6),
        n if n >= 1e3 => scaled("us", 1e3),
        _ => scaled("ns", 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_debugging::gc_stats::GcStat;
    use crate::remote_debugging::offsets::offset_table::{GcItemLayout, seq_layout};
    use std::sync::LazyLock;

    /// A build that bounds each Collection with timestamps and keeps the generation's
    /// cumulative pause total beside them.
    static TIMED: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
            "duration",
        ])
    });

    /// A build publishing the Lifetime totals and nothing else. The two layouts express the
    /// tiers, so no test below names a Python version.
    static COUNTS_ONLY: LazyLock<&'static GcItemLayout> =
        LazyLock::new(|| seq_layout(&["collections", "collected", "uncollectable"]));

    #[allow(clippy::too_many_arguments)]
    fn timed(
        generation: u32,
        interpreter: i64,
        collections: i64,
        collected: i64,
        uncollectable: i64,
        ts_start: i64,
        ts_stop: i64,
    ) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            interpreter,
            *TIMED,
            &[
                ("collections", collections),
                ("collected", collected),
                ("uncollectable", uncollectable),
                ("ts_start", ts_start),
                ("ts_stop", ts_stop),
            ],
        )
    }

    /// A timed Record from generation 0 of interpreter 0, carrying the running pause total the
    /// exact figures are reconstructed from.
    fn with_duration(
        collections: i64,
        ts_start: i64,
        ts_stop: i64,
        cumulative_secs: f64,
    ) -> GcStat {
        GcStat::from_fields(
            0,
            0,
            0,
            *TIMED,
            &[
                ("collections", collections),
                ("ts_start", ts_start),
                ("ts_stop", ts_stop),
                ("duration", f64::to_bits(cumulative_secs) as i64),
            ],
        )
    }

    fn counted(
        generation: u32,
        interpreter: i64,
        collections: i64,
        collected: i64,
        uncollectable: i64,
    ) -> GcStat {
        GcStat::from_fields(
            generation,
            0,
            interpreter,
            *COUNTS_ONLY,
            &[
                ("collections", collections),
                ("collected", collected),
                ("uncollectable", uncollectable),
            ],
        )
    }

    /// Drive the poll-time accumulator with scripted polls, the way a run would.
    fn run(polls: &[(u32, Vec<GcStat>)]) -> Vec<InterpreterSummary> {
        let mut cursor = Cursor::new();
        for (pid, records) in polls {
            cursor.admit(*pid, records);
        }
        summarize(&cursor)
    }

    /// The counts are what the Lifetime totals did over the span, not what they read at the
    /// end. "How much GC did this run do" gets the run's figure, not one that has been
    /// accumulating since the interpreter started.
    #[test]
    fn counts_are_what_moved_across_the_observed_span() {
        let summary = run(&[
            (7, vec![counted(0, 0, 100, 4_000, 2)]),
            (7, vec![counted(0, 0, 140, 9_500, 7)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.collections, 40);
        assert_eq!(gen0.collected, 5_500);
        assert_eq!(gen0.uncollectable, 5);
        assert_eq!(gen0.records, 2);
    }

    /// One snapshot leaves nothing to difference against, so an inline build reports nothing
    /// rather than the interpreter's history or a Collection it merely witnessed. This is what
    /// a script calling `gc.disable()` reads back; crediting the opening Record put one
    /// collection on every generation of every such run.
    #[test]
    fn a_lone_snapshot_from_an_inline_build_reports_nothing() {
        let summary = run(&[(7, vec![counted(0, 0, 900, 40_000, 3)])]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.collections, 0);
        assert_eq!(gen0.collected, 0);
        assert_eq!(gen0.uncollectable, 0);
        assert_eq!(gen0.records, 1);
    }

    /// A ring Entry *is* a Collection, so the opening Record counts as one. That makes the
    /// count equal the Records read when nothing was overwritten, which is where ticket 06
    /// reads Coverage from.
    #[test]
    fn a_ring_build_counts_the_record_that_opened_the_span() {
        let one = run(&[(7, vec![timed(0, 0, 900, 40_000, 0, 1_000, 1_400)])]);
        assert_eq!(one[0].generations[0].collections, 1);
        assert_eq!(one[0].generations[0].records, 1);

        let many = run(&[
            (7, vec![timed(0, 0, 10, 100, 0, 1_000, 1_400)]),
            (7, vec![timed(0, 0, 11, 150, 0, 2_000, 2_600)]),
            (7, vec![timed(0, 0, 12, 190, 0, 3_000, 3_100)]),
        ]);
        let gen0 = &many[0].generations[0];
        assert_eq!(gen0.collections, 3);
        assert_eq!(gen0.records, 3, "nothing was lost, so the two agree");
    }

    /// The headline of the surface. A ring that ran 100 Collections while two polls caught two
    /// reports 100, with Coverage saying how much of that was watched rather than counted.
    #[test]
    fn the_counts_are_what_ran_not_what_was_read() {
        let summary = run(&[
            (7, vec![timed(0, 0, 1, 10, 0, 1_000, 1_400)]),
            (7, vec![timed(0, 0, 100, 5_000, 0, 900_000, 900_600)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.collections, 100);
        assert_eq!(gen0.records, 2);
        assert_eq!(gen0.lost, 98);
        assert!((gen0.coverage - 0.02).abs() < 1e-12, "{}", gen0.coverage);
    }

    /// Nothing an inline build publishes describes a Collection, so its counts stand alone
    /// with no distribution behind them and Coverage says so (ADR 0017).
    #[test]
    fn the_counter_only_tier_reports_zero_coverage() {
        let summary = run(&[
            (7, vec![counted(0, 0, 10, 100, 0)]),
            (7, vec![counted(0, 0, 30, 900, 0)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.collections, 20);
        assert_eq!(gen0.coverage, 0.0);
        assert_eq!(gen0.lost, 20);
        // Two Records were read and they witness no Collection between them, which is why
        // `records` is not the term that reconciles against the count.
        assert_eq!(gen0.records, 2);
        assert_eq!(gen0.observed, 0);
    }

    /// What ran is what was read plus what was lost, on both tiers. Spec 0011's story 19, and
    /// the three figures the JSON form publishes to make it checkable.
    #[test]
    fn the_counts_reconcile_on_both_tiers() {
        let summary = run(&[
            (
                7,
                vec![
                    counted(0, 0, 10, 100, 0),
                    timed(1, 0, 1, 10, 0, 1_000, 1_400),
                ],
            ),
            (
                7,
                vec![
                    counted(0, 0, 30, 900, 0),
                    timed(1, 0, 100, 5_000, 0, 900_000, 900_600),
                ],
            ),
        ]);

        for g in &summary[0].generations {
            assert_eq!(g.collections, g.observed + g.lost, "gen {}", g.generation);
        }
    }

    /// The pause is the target's own cumulative figure over the span, not the share the polls
    /// caught. Under Loss the two differ by the pause of everything overwritten.
    #[test]
    fn the_pause_total_is_the_targets_figure_not_the_sum_of_what_was_read() {
        let summary = run(&[
            (7, vec![with_duration(1, 1_000, 1_400, 0.000_000_400)]),
            (7, vec![with_duration(100, 900_000, 900_600, 0.000_500_000)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.pause_total_ns, Some(500_000));
        assert_eq!(gen0.pause_measured_ns, Some(1_000));
        assert_eq!(gen0.scale_factor, Some(500.0));
        // The mean is exact over exact: 500 us across the 100 Collections that ran, not the
        // 500 ns average of the two that happened to be caught.
        assert_eq!(gen0.pause_mean_ns(), Some(5_000.0));
    }

    /// A build can bound its Collections without publishing the total the exact pause is
    /// differenced from. Its rows keep the columns describing what was read and leave the pause
    /// blank, rather than losing `records` and `coverage` over a figure it cannot reconstruct.
    #[test]
    fn a_build_that_prices_no_pause_total_keeps_the_columns_it_can_fill() {
        static UNPRICED: LazyLock<&'static GcItemLayout> =
            LazyLock::new(|| seq_layout(&["ts_start", "ts_stop", "collections"]));
        let unpriced = |counter: i64, ts_start: i64, ts_stop: i64| {
            GcStat::from_fields(
                0,
                0,
                0,
                *UNPRICED,
                &[
                    ("collections", counter),
                    ("ts_start", ts_start),
                    ("ts_stop", ts_stop),
                ],
            )
        };

        let summary = run(&[
            (7, vec![unpriced(1, 1_000, 1_400)]),
            (7, vec![unpriced(100, 900_000, 900_600)]),
        ]);
        assert!(summary[0].has_timing(), "its Collections are still spans");

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.pause_total_ns, None);
        assert_eq!(gen0.pause_mean_ns(), None);
        assert_eq!(gen0.pause_measured_ns, Some(1_000));

        let row = render(&summary).last().unwrap().clone();
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            ["0", "100", "0", "0", "2", "0.020"],
            "the pause cells are empty, not zero"
        );
    }

    /// A layout with no timestamps cannot say how long it spent collecting, so the figure is
    /// absent. A `0` there reads as "this process spends no time in GC" (ADR 0017).
    #[test]
    fn a_build_without_timing_reports_no_pause_figures() {
        let summary = run(&[
            (7, vec![counted(0, 0, 10, 100, 0)]),
            (7, vec![counted(0, 0, 12, 150, 0)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.pause_total_ns, None);
        assert_eq!(gen0.pause_mean_ns(), None);
        assert!(!summary[0].has_timing());
    }

    /// A build with timing reports the pause measured across the Records read, and the mean
    /// over those same Records.
    #[test]
    fn a_build_with_timing_reports_pause_total_and_mean() {
        let summary = run(&[
            (7, vec![timed(0, 0, 10, 100, 0, 1_000, 1_400)]),
            (7, vec![timed(0, 0, 11, 150, 0, 2_000, 2_600)]),
        ]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.pause_total_ns, Some(1_000));
        assert_eq!(gen0.pause_mean_ns(), Some(500.0));
        assert!(summary[0].has_timing());
    }

    /// Generations collect at wildly different rates, so each takes its own row and none
    /// stands in for another.
    #[test]
    fn generations_are_reported_separately_and_in_order() {
        let summary = run(&[(
            7,
            vec![
                counted(2, 0, 1, 5, 0),
                counted(0, 0, 90, 800, 0),
                counted(1, 0, 8, 60, 0),
            ],
        )]);

        assert_eq!(summary.len(), 1);
        let generations: Vec<u32> = summary[0]
            .generations
            .iter()
            .map(|g| g.generation)
            .collect();
        assert_eq!(generations, [0, 1, 2]);
    }

    /// Summed into one block, a busy sub-interpreter's activity is indistinguishable from the
    /// main interpreter's. Each gets its own.
    #[test]
    fn interpreters_are_reported_separately() {
        let summary = run(&[
            (
                7,
                vec![counted(0, 0, 10, 100, 0), counted(0, 3, 500, 9_000, 0)],
            ),
            (
                7,
                vec![counted(0, 0, 12, 150, 0), counted(0, 3, 900, 20_000, 0)],
            ),
        ]);

        assert_eq!(summary.len(), 2);
        assert_eq!((summary[0].pid, summary[0].interpreter), (7, 0));
        assert_eq!((summary[1].pid, summary[1].interpreter), (7, 3));
        assert_eq!(summary[0].generations[0].collections, 2);
        assert_eq!(summary[1].generations[0].collections, 400);
    }

    /// A worker whose read failed for a tick and was re-discovered on the next, or a PID the
    /// OS recycled, holds two spans under one ring key. Each gets its own block: interleaved,
    /// they leave two `gen 0` rows in one table with nothing telling them apart.
    #[test]
    fn a_pid_observed_twice_keeps_its_two_spans_in_separate_blocks() {
        let mut cursor = Cursor::new();
        cursor.admit(7, &[counted(0, 0, 10, 100, 0), counted(1, 0, 4, 20, 0)]);
        cursor.admit(7, &[counted(0, 0, 30, 500, 0), counted(1, 0, 9, 80, 0)]);
        cursor.forget(7);
        cursor.admit(7, &[counted(0, 0, 1, 5, 0)]);
        cursor.admit(7, &[counted(0, 0, 6, 55, 0)]);

        let summary = summarize(&cursor);
        assert_eq!(summary.len(), 2, "{summary:#?}");
        assert_eq!(
            summary[0]
                .generations
                .iter()
                .map(|g| (g.generation, g.collections))
                .collect::<Vec<(u32, i64)>>(),
            [(0, 20), (1, 5)]
        );
        // The second span starts from the re-attach, not from the first span's counters.
        assert_eq!(
            summary[1]
                .generations
                .iter()
                .map(|g| (g.generation, g.collections))
                .collect::<Vec<(u32, i64)>>(),
            [(0, 5)]
        );
    }

    /// One monitoring run covers a process tree, so each process gets its own block too.
    #[test]
    fn processes_are_reported_separately_and_in_pid_order() {
        let summary = run(&[
            (900, vec![counted(0, 0, 1, 1, 0)]),
            (100, vec![counted(0, 0, 1, 1, 0)]),
        ]);

        let pids: Vec<u32> = summary.iter().map(|s| s.pid).collect();
        assert_eq!(pids, [100, 900]);
    }

    /// A run that read nothing has no ring to report on, which is not the same as a run whose
    /// rings all read zero.
    #[test]
    fn a_run_that_read_nothing_summarizes_to_nothing() {
        assert_eq!(summarize(&Cursor::new()), []);
        assert_eq!(render(&[]), ["No GC collections were observed."]);
    }

    // ── the table ────────────────────────────────────────────────────────────────────

    /// Every block names what it covers, so a reader never has to guess whether a figure is
    /// one interpreter's or a mix of several.
    #[test]
    fn each_block_names_the_process_and_interpreter_it_covers() {
        let summary = run(&[
            (7, vec![counted(0, 0, 10, 100, 0), counted(0, 3, 5, 50, 0)]),
            (7, vec![counted(0, 0, 12, 150, 0), counted(0, 3, 9, 90, 0)]),
        ]);

        let lines = render(&summary);
        assert!(
            lines.iter().any(|l| l == "python 7, interpreter 0"),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "python 7, interpreter 3"),
            "{lines:#?}"
        );
    }

    /// A block is a title, a header between two rules, then a row per generation, with a blank
    /// line between blocks. The rules are cut to the header's width, so a column added without
    /// widening them shows up here.
    #[test]
    fn a_block_is_a_titled_table_and_blocks_are_separated() {
        let summary = run(&[(7, vec![counted(0, 0, 10, 100, 0), counted(0, 3, 5, 50, 0)])]);

        let lines = render(&summary);
        let header = format_header(false);
        assert_eq!(
            lines,
            [
                "python 7, interpreter 0",
                &"-".repeat(header.len()),
                &header,
                &"-".repeat(header.len()),
                &format_row(&summary[0].generations[0], false),
                "",
                "python 7, interpreter 3",
                &"-".repeat(header.len()),
                &header,
                &"-".repeat(header.len()),
                &format_row(&summary[1].generations[0], false),
            ]
        );
    }

    /// The pause columns exist only where the build supplies them. A header over a column of
    /// dashes still invites comparison with a real one.
    #[test]
    fn the_pause_columns_appear_only_where_the_build_publishes_timing() {
        let untimed = render(&run(&[
            (7, vec![counted(0, 0, 10, 100, 0)]),
            (7, vec![counted(0, 0, 12, 150, 0)]),
        ]));
        let header = untimed.iter().find(|l| l.contains("collections")).unwrap();
        assert!(!header.contains("pause"), "{header}");
        assert!(!header.contains("records"), "{header}");

        let timed_lines = render(&run(&[
            (7, vec![timed(0, 0, 10, 100, 0, 1_000, 1_400)]),
            (7, vec![timed(0, 0, 11, 150, 0, 2_000, 2_600)]),
        ]));
        let header = timed_lines
            .iter()
            .find(|l| l.contains("collections"))
            .unwrap();
        assert!(header.contains("pause total"), "{header}");
        assert!(header.contains("records"), "{header}");
    }

    /// The figures reach the table intact and under the right headings, pinned by splitting on
    /// whitespace as the `gc-stats` table's own test does. Four Collections ran against two
    /// Records read, so the row carries Loss too.
    #[test]
    fn a_row_carries_its_generations_figures_in_column_order() {
        let summary = run(&[
            (7, vec![timed(1, 0, 10, 100, 1, 1_000, 1_400)]),
            (7, vec![timed(1, 0, 13, 150, 4, 2_000, 2_600)]),
        ]);

        let lines = render(&summary);
        let row = lines.last().unwrap();
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            cols,
            [
                "1", "4", "50", "3", "2", "0.500", "1.000", "us", "250.000", "ns"
            ]
        );
    }

    /// A build with no timing renders the counts and the Coverage saying nothing sits behind
    /// them.
    #[test]
    fn an_untimed_row_carries_the_counts_and_no_distribution() {
        let summary = run(&[
            (7, vec![counted(2, 0, 10, 100, 0)]),
            (7, vec![counted(2, 0, 30, 900, 6)]),
        ]);

        let row = render(&summary).last().unwrap().clone();
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            ["2", "20", "800", "6", "0.000"]
        );
    }
}
