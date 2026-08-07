//! The end-of-run summary: what the Observer read each ring do, per generation.
//!
//! Built from the poll-time accumulator in [`super::cursor`] rather than by re-reading
//! anything at the end, so the same figures are available from a replayed stream of Records
//! when the pyperf hook needs them.
//!
//! Every figure here is *observed*. On a build that overwrites Entries faster than gcscope
//! polls, the Records read are a sample of what ran; the `records` column is what shows it.

use crate::monitor::cursor::Cursor;

/// What one generation of one interpreter did over the span the Observer watched it.
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationSummary {
    pub generation: u32,
    /// Collections in the accounted span: the Lifetime counter's own rise, plus the Record
    /// that opened the span.
    pub collections: i64,
    /// Objects collected across the span. Unlike `collections` this cannot include the
    /// opening Record's own contribution: CPython publishes a running total, and the total
    /// before that Record was never read.
    pub collected: i64,
    /// Uncollectable objects across the span, on the same basis as `collected`.
    pub uncollectable: i64,
    /// Records read. Below `collections` where Entries were overwritten between polls.
    pub records: u64,
    /// Pause summed over the Records read. `None` where the build's Entry layout carries no
    /// timestamps — the figure is absent, never zero (ADR 0017).
    pub pause_total_ns: Option<i64>,
}

impl GenerationSummary {
    /// Mean pause over the Records read, in nanoseconds. `None` on a build with no timing, and
    /// on one whose ring published nothing to average.
    pub fn pause_mean_ns(&self) -> Option<f64> {
        match (self.pause_total_ns, self.records) {
            (Some(total), records) if records > 0 => Some(total as f64 / records as f64),
            _ => None,
        }
    }
}

/// One interpreter of one process. Figures are never mixed across interpreters: a busy
/// sub-interpreter and a quiet one keep separate blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpreterSummary {
    pub pid: u32,
    pub interpreter: i64,
    pub generations: Vec<GenerationSummary>,
}

impl InterpreterSummary {
    /// Whether this build publishes the timestamps a pause figure needs. Uniform across an
    /// interpreter's generations, since they share one build.
    pub fn has_timing(&self) -> bool {
        self.generations.iter().any(|g| g.pause_total_ns.is_some())
    }
}

/// Fold the run's accumulators into one summary per interpreter, ordered by process then
/// interpreter then generation.
///
/// The accumulators already arrive in that order, so this walks them once and starts a block
/// wherever the process or interpreter changes.
pub fn summarize(cursor: &Cursor) -> Vec<InterpreterSummary> {
    let mut blocks: Vec<InterpreterSummary> = Vec::new();

    for (key, observation) in cursor.observations() {
        if observation.is_empty() {
            continue;
        }
        let (first, last) = (observation.first(), observation.last());
        let generation = GenerationSummary {
            generation: key.generation,
            // The counter's rise plus the Record that opened the span: with no Loss this
            // equals the Records read, which is what makes Coverage 1.0 in ticket 06.
            collections: last.collections - first.collections + 1,
            collected: last.collected - first.collected,
            uncollectable: last.uncollectable - first.uncollectable,
            records: observation.sampled(),
            pause_total_ns: observation
                .has_timing()
                .then(|| observation.measured_pause_ns()),
        };

        match blocks.last_mut() {
            Some(block) if block.pid == key.pid && block.interpreter == key.interpreter => {
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
/// One block per interpreter, each naming the process and interpreter it covers, so no figure
/// is left to look like a tree-wide total. A block's column set follows the tier its build
/// sits in: a build with no timing has no pause to show, and no `records` column either,
/// since a counter snapshot is not a Collection to compare a count against.
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

/// The column header, matched to the column set the tier selects.
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

/// A nanosecond figure at a readable scale. Three decimals throughout, so a column of them
/// lines up and a pause an order of magnitude apart from its neighbour still reads as one.
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

    /// A build publishing the Lifetime totals and nothing else. The two layouts are how the
    /// tiers are expressed here: no test names a Python version.
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

    /// The counts are what CPython's Lifetime totals did over the span, not what they read at
    /// the end. An operator asking "how much GC did this run do" gets the run's figure rather
    /// than one that has been accumulating since the interpreter started.
    #[test]
    fn counts_are_what_moved_across_the_observed_span() {
        let summary = run(&[
            (7, vec![counted(0, 0, 100, 4_000, 2)]),
            (7, vec![counted(0, 0, 140, 9_500, 7)]),
        ]);

        let gen0 = &summary[0].generations[0];
        // 40 the counter rose, plus the Record that opened the span.
        assert_eq!(gen0.collections, 41);
        assert_eq!(gen0.collected, 5_500);
        assert_eq!(gen0.uncollectable, 5);
        assert_eq!(gen0.records, 2);
    }

    /// A single Record is a span of one Collection with nothing to difference against, so the
    /// object counts stay at zero rather than reporting the interpreter's whole history.
    #[test]
    fn a_single_record_reports_one_collection_and_no_object_counts() {
        let summary = run(&[(7, vec![counted(0, 0, 900, 40_000, 3)])]);

        let gen0 = &summary[0].generations[0];
        assert_eq!(gen0.collections, 1);
        assert_eq!(gen0.collected, 0);
        assert_eq!(gen0.uncollectable, 0);
        assert_eq!(gen0.records, 1);
    }

    /// A build whose Entry layout has no timestamps cannot say how long it spent collecting,
    /// so the figure is absent. Reporting `0` there reads as "this process spends no time in
    /// GC", which is the wrong conclusion (spec 0011 §2).
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

    /// A build with timing reports the pause it measured across the Records it read, and the
    /// mean over those same Records.
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

    /// Each generation is its own row: they collect at wildly different rates, and one
    /// generation's figures must not stand in for another's.
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

    /// Interpreters get their own block rather than being summed into one. A busy
    /// sub-interpreter's activity is otherwise indistinguishable from the main one's.
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
        assert_eq!(summary[0].generations[0].collections, 3);
        assert_eq!(summary[1].generations[0].collections, 401);
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

    /// A run that read nothing has no ring to report on, which is different from a run whose
    /// rings all read zero.
    #[test]
    fn a_run_that_read_nothing_summarizes_to_nothing() {
        assert_eq!(summarize(&Cursor::new()), []);
        assert_eq!(render(&[]), ["No GC collections were observed."]);
    }

    // ── the table ────────────────────────────────────────────────────────────────────

    /// Every block names the process and interpreter it covers, so the reader never has to
    /// guess whether a figure is one interpreter's or a mix of several.
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

    /// A block is a title, its header between two rules, then one row per generation, and two
    /// blocks are separated by a blank line. The rules are cut to the header's width, so a
    /// column added without widening them would show up here.
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
    /// dashes would still invite the reader to compare it with a real one.
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

    /// The figures reach the table intact and under the right headings. Splitting the row on
    /// whitespace pins the column contents, the way the `gc-stats` table's own test does.
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
            ["2", "21", "800", "6"]
        );
    }
}
