//! The end-of-run summary: what each ring did over the span the Observer watched it.
//!
//! Read from the poll-time accumulator in [`super::cursor`], never from the written trace, so
//! a replayed stream of Records gives the same figures.
//!
//! Every figure is observed. Where Entries are overwritten between polls, the Records read are
//! a sample of what ran and the `records` column is what shows it.

use crate::monitor::cursor::Cursor;

/// What one generation of one interpreter did over the span the Observer watched it.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationSummary {
    pub generation: u32,
    /// The Lifetime counter's rise, plus the opening Record where that is a Collection in its
    /// own right. See [`summarize`].
    pub collections: i64,
    /// Objects collected across the span. Never counts the opening Record: CPython publishes a
    /// running total, and the total before it was never read.
    pub collected: i64,
    /// Uncollectable objects across the span, on the same basis as `collected`.
    pub uncollectable: i64,
    /// Records read. Below `collections` where Entries were overwritten between polls.
    pub records: u64,
    /// Pause summed over the Records read. `None` where the Entry layout carries no
    /// timestamps: absent, never zero (ADR 0017).
    pub pause_total_ns: Option<i64>,
}

impl GenerationSummary {
    /// Mean pause over the Records read, in nanoseconds.
    pub fn pause_mean_ns(&self) -> Option<f64> {
        match (self.pause_total_ns, self.records) {
            (Some(total), records) if records > 0 => Some(total as f64 / records as f64),
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
    /// Whether this build publishes the timestamps a pause figure needs. One answer per
    /// interpreter: its generations share a build.
    pub fn has_timing(&self) -> bool {
        self.generations.iter().any(|g| g.pause_total_ns.is_some())
    }
}

/// Fold the run's accumulators into one summary per interpreter.
///
/// They arrive ordered, so this walks them once and starts a block where the process or
/// interpreter changes.
///
/// Whether the opening Record counts as a Collection follows the tier: a ring Entry describes
/// one, an inline Entry is a snapshot of running totals and describes none. A lone snapshot
/// therefore reports nothing.
pub fn summarize(cursor: &Cursor) -> Vec<InterpreterSummary> {
    let mut blocks: Vec<InterpreterSummary> = Vec::new();

    for (key, observation) in cursor.observations() {
        if observation.is_empty() {
            continue;
        }
        let (first, last) = (observation.first(), observation.last());
        // On the ring tier this makes the count equal the Records read when nothing was lost,
        // the identity ticket 06 reads Coverage 1.0 off.
        let opening_record = i64::from(observation.has_timing());
        let generation = GenerationSummary {
            generation: key.generation,
            collections: last.collections - first.collections + opening_record,
            collected: last.collected - first.collected,
            uncollectable: last.uncollectable - first.uncollectable,
            records: observation.sampled(),
            pause_total_ns: observation
                .has_timing()
                .then(|| observation.measured_pause_ns()),
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

/// The column header, matched to the set the tier selects.
fn format_header(timed: bool) -> String {
    let counts = format!(
        "{:>3} {:>12} {:>12} {:>14}",
        "gen", "collections", "collected", "uncollectable"
    );
    if timed {
        format!(
            "{counts} {:>8} {:>14} {:>14}",
            "records", "pause total", "pause mean"
        )
    } else {
        counts
    }
}

fn format_row(g: &GenerationSummary, timed: bool) -> String {
    let counts = format!(
        "{:>3} {:>12} {:>12} {:>14}",
        g.generation, g.collections, g.collected, g.uncollectable
    );
    if timed {
        format!(
            "{counts} {:>8} {:>14} {:>14}",
            g.records,
            g.pause_total_ns
                .map_or_else(String::new, |ns| duration(ns as f64)),
            g.pause_mean_ns().map_or_else(String::new, duration),
        )
    } else {
        counts
    }
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

    /// A build that bounds each Collection with timestamps, so its Records carry a pause.
    static TIMED: LazyLock<&'static GcItemLayout> = LazyLock::new(|| {
        seq_layout(&[
            "ts_start",
            "ts_stop",
            "collections",
            "collected",
            "uncollectable",
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

    /// A layout with no timestamps cannot say how long it spent collecting, so the figure is
    /// absent. A `0` there reads as "this process spends no time in GC" (spec 0011 §2).
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

    /// The figures reach the table intact and under the right headings. Splitting on
    /// whitespace pins the column contents, as the `gc-stats` table's own test does.
    #[test]
    fn a_row_carries_its_generations_figures_in_column_order() {
        let summary = run(&[
            (7, vec![timed(1, 0, 10, 100, 1, 1_000, 1_400)]),
            (7, vec![timed(1, 0, 11, 150, 4, 2_000, 2_600)]),
        ]);

        let lines = render(&summary);
        let row = lines.last().unwrap();
        let cols: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            cols,
            ["1", "2", "50", "3", "2", "1.000", "us", "500.000", "ns"]
        );
    }

    /// A build with no timing renders the counts and stops there.
    #[test]
    fn an_untimed_row_carries_only_the_counts() {
        let summary = run(&[
            (7, vec![counted(2, 0, 10, 100, 0)]),
            (7, vec![counted(2, 0, 30, 900, 6)]),
        ]);

        let row = render(&summary).last().unwrap().clone();
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            ["2", "20", "800", "6"]
        );
    }
}
