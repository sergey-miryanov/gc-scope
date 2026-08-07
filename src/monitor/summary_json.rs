//! The end-of-run summary as JSON, for a CI check or another tool to read.
//!
//! One rule shapes the schema: a figure the build cannot supply has no key, so a check
//! thresholding on pause time fails to find the field against a 3.12 target rather than
//! passing against a zero (spec 0011 §2).
//!
//! The figures arrive folded from [`super::statistics`], the same values the table prints,
//! and nothing here computes one. Two renderings each doing their own arithmetic is how gcmon
//! ended up with two disagreeing accounts of one run (its ADR-0007).
//!
//! The JSON is hand-written like the Chrome encoder's: a fixed shape of numbers with one
//! constant string in it, nothing to escape, and the test at the bottom pinning the bytes.
//! `docs/summary-json.md` carries the schema and its consumer.

use std::io::Write;

use anyhow::{Context, Result};

use crate::monitor::statistics::{GenerationSummary, InterpreterSummary};

/// The document's name and version.
///
/// Coverage and the exact counts are in this first version rather than a second: a consumer
/// pinning `1` gets the reconstruction, not the observed counts alone.
pub const SCHEMA: &str = "gcscope.gc-summary/1";

/// The destination meaning stdout, spelled the way every other CLI spells it.
pub const STDOUT: &str = "-";

/// The whole summary as one JSON document, newline-terminated.
pub fn encode(summary: &[InterpreterSummary]) -> String {
    let blocks: Vec<Vec<String>> = summary.iter().map(interpreter).collect();

    let mut lines = vec!["{".to_string(), format!(r#"  "schema": "{SCHEMA}","#)];
    lines.extend(indent(list("interpreters", blocks)));
    lines.push("}".to_string());
    lines.join("\n") + "\n"
}

/// Write the document where the operator asked for it.
pub fn write(destination: &str, summary: &[InterpreterSummary]) -> Result<()> {
    let document = encode(summary);
    if destination == STDOUT {
        // The one thing gcscope puts on stdout, which is why the table goes to stderr.
        // Written rather than `print!`ed: a consumer piping into `head` closes the pipe, and
        // `print!` panics on that.
        let mut out = std::io::stdout().lock();
        return out
            .write_all(document.as_bytes())
            .and_then(|()| out.flush())
            .context("Failed to write the JSON summary to stdout");
    }
    std::fs::write(destination, document)
        .with_context(|| format!("Failed to write the JSON summary to {destination}"))
}

/// One interpreter of one process, as the lines of its object.
fn interpreter(block: &InterpreterSummary) -> Vec<String> {
    let generations = block
        .generations
        .iter()
        .map(|g| vec![generation(g)])
        .collect();

    let mut lines = vec![
        "{".to_string(),
        format!(r#"  "pid": {},"#, block.pid),
        format!(r#"  "interpreter": {},"#, block.interpreter),
    ];
    lines.extend(indent(list("generations", generations)));
    lines.push("}".to_string());
    lines
}

/// One generation's figures, on one line. Every value is a number, so it reads like a row of
/// the table.
fn generation(g: &GenerationSummary) -> String {
    let mut fields = vec![
        ("generation", g.generation.to_string()),
        ("collections", g.collections.to_string()),
        ("collected", g.collected.to_string()),
        ("uncollectable", g.uncollectable.to_string()),
        ("records", g.records.to_string()),
        ("observed", g.observed.to_string()),
        ("lost", g.lost.to_string()),
    ];

    let mut supplied = |key: &'static str, value: Option<String>| {
        if let Some(value) = value {
            fields.push((key, value));
        }
    };
    supplied("coverage", number(g.coverage));
    supplied("pause_total_ns", g.pause_total_ns.map(|ns| ns.to_string()));
    supplied(
        "pause_measured_ns",
        g.pause_measured_ns.map(|ns| ns.to_string()),
    );
    supplied("pause_mean_ns", g.pause_mean_ns().and_then(number));
    supplied("scale_factor", g.scale_factor.and_then(number));

    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!(r#""{key}": {value}"#))
        .collect();
    format!("{{{}}}", body.join(", "))
}

/// A float as JSON, or nothing where JSON has no spelling for it. A non-finite figure is not
/// a figure, and absence is what this schema says about one.
fn number(value: f64) -> Option<String> {
    value.is_finite().then(|| value.to_string())
}

/// `"name": [ … ]` with one item per line, as the lines of the enclosing object. An empty
/// list keeps its brackets together, so a run that read nothing still parses.
fn list(name: &str, items: Vec<Vec<String>>) -> Vec<String> {
    if items.is_empty() {
        return vec![format!(r#""{name}": []"#)];
    }

    let last = items.len() - 1;
    let mut lines = vec![format!(r#""{name}": ["#)];
    for (position, item) in items.into_iter().enumerate() {
        let mut item = indent(item);
        if position != last {
            item.last_mut().expect("an item with no lines").push(',');
        }
        lines.extend(item);
    }
    lines.push("]".to_string());
    lines
}

fn indent(lines: Vec<String>) -> Vec<String> {
    lines.into_iter().map(|line| format!("  {line}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::statistics::render;

    /// One generation of a ring build: counts reconstructed over the span, and the pause
    /// priced from the target's cumulative total.
    fn timed(generation: u32) -> GenerationSummary {
        GenerationSummary {
            generation,
            collections: 100,
            collected: 4_990,
            uncollectable: 2,
            records: 2,
            observed: 2,
            lost: 98,
            coverage: 0.02,
            pause_total_ns: Some(500_000),
            pause_measured_ns: Some(1_000),
            scale_factor: Some(500.0),
        }
    }

    /// One generation of an inline build: the counts stand alone, with nothing describing a
    /// single Collection behind them.
    fn counted(generation: u32) -> GenerationSummary {
        GenerationSummary {
            generation,
            collections: 20,
            collected: 800,
            uncollectable: 0,
            records: 2,
            observed: 0,
            lost: 20,
            coverage: 0.0,
            pause_total_ns: None,
            pause_measured_ns: None,
            scale_factor: None,
        }
    }

    fn block(
        pid: u32,
        interpreter: i64,
        generations: Vec<GenerationSummary>,
    ) -> InterpreterSummary {
        InterpreterSummary {
            pid,
            interpreter,
            generations,
        }
    }

    /// The key/value pairs of each generation object in the document, in order.
    ///
    /// Not a JSON parser: a generation object is one line of `"key": number` pairs with no
    /// nesting and no strings in it, which the byte-for-byte test below keeps true.
    fn generations(document: &str) -> Vec<Vec<(&str, &str)>> {
        document
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(r#"{"generation""#))
            .map(|line| {
                line.trim_end_matches(',')
                    .trim_matches(|c| c == '{' || c == '}')
                    .split(", ")
                    .map(|pair| {
                        let (key, value) = pair.split_once(": ").expect("a key and a value");
                        (key.trim_matches('"'), value)
                    })
                    .collect()
            })
            .collect()
    }

    /// The first generation of the first interpreter, which most cases below only need one of.
    fn only_generation(summary: &[InterpreterSummary]) -> Vec<(String, String)> {
        let document = encode(summary);
        generations(&document)
            .first()
            .unwrap_or_else(|| panic!("no generation in\n{document}"))
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn figure(fields: &[(String, String)], key: &str) -> Option<String> {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    /// The document, byte for byte, for a summary carrying both tiers.
    ///
    /// Pinned the way the Chrome trace is: its consumers are outside this repo, so a renamed
    /// key or a silently dropped figure has to fail here. Paste new bytes in only after
    /// deciding the schema changed.
    #[test]
    fn the_document_is_this_shape() {
        let document = encode(&[
            block(7, 0, vec![timed(0), counted(1)]),
            block(900, 3, vec![counted(0)]),
        ]);

        assert_eq!(
            document,
            r#"{
  "schema": "gcscope.gc-summary/1",
  "interpreters": [
    {
      "pid": 7,
      "interpreter": 0,
      "generations": [
        {"generation": 0, "collections": 100, "collected": 4990, "uncollectable": 2, "records": 2, "observed": 2, "lost": 98, "coverage": 0.02, "pause_total_ns": 500000, "pause_measured_ns": 1000, "pause_mean_ns": 5000, "scale_factor": 500},
        {"generation": 1, "collections": 20, "collected": 800, "uncollectable": 0, "records": 2, "observed": 0, "lost": 20, "coverage": 0}
      ]
    },
    {
      "pid": 900,
      "interpreter": 3,
      "generations": [
        {"generation": 0, "collections": 20, "collected": 800, "uncollectable": 0, "records": 2, "observed": 0, "lost": 20, "coverage": 0}
      ]
    }
  ]
}
"#
        );
    }

    /// Every figure the table prints reaches the object, at full precision rather than the
    /// table's three decimals.
    #[test]
    fn the_json_carries_what_the_table_prints() {
        let summary = [block(7, 0, vec![timed(1)])];
        let fields = only_generation(&summary);

        assert_eq!(figure(&fields, "generation").as_deref(), Some("1"));
        assert_eq!(figure(&fields, "collections").as_deref(), Some("100"));
        assert_eq!(figure(&fields, "collected").as_deref(), Some("4990"));
        assert_eq!(figure(&fields, "uncollectable").as_deref(), Some("2"));
        assert_eq!(figure(&fields, "records").as_deref(), Some("2"));
        assert_eq!(figure(&fields, "pause_total_ns").as_deref(), Some("500000"));
        assert_eq!(figure(&fields, "pause_mean_ns").as_deref(), Some("5000"));

        // Rounding the JSON figure the way the table does has to land on the table's own cell.
        let cells: Vec<String> = render(&summary)
            .last()
            .unwrap()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        assert_eq!(cells[1], figure(&fields, "collections").unwrap());
        let coverage: f64 = figure(&fields, "coverage").unwrap().parse().unwrap();
        assert_eq!(cells[5], format!("{coverage:.3}"));
    }

    /// The reconciling identity of ADR 0019, published so a consumer can check the
    /// reconstruction against CPython's own counters without re-deriving it.
    #[test]
    fn the_exact_counts_and_coverage_ride_both_tiers() {
        for generations in [vec![timed(0)], vec![counted(0)]] {
            let fields = only_generation(&[block(7, 0, generations)]);
            for key in ["collections", "observed", "lost", "coverage", "records"] {
                assert!(figure(&fields, key).is_some(), "{key} missing: {fields:?}");
            }

            let count = |key| -> i64 { figure(&fields, key).unwrap().parse().unwrap() };
            assert_eq!(count("collections"), count("observed") + count("lost"));
        }
    }

    /// The rule the schema exists for. A CI check thresholding on pause time must fail to
    /// find the key against a build that publishes none, rather than pass against a zero.
    #[test]
    fn a_figure_the_build_cannot_supply_has_no_key() {
        let fields = only_generation(&[block(7, 0, vec![counted(0)])]);

        for key in [
            "pause_total_ns",
            "pause_measured_ns",
            "pause_mean_ns",
            "scale_factor",
        ] {
            assert_eq!(figure(&fields, key), None, "{key}: {fields:?}");
        }
        // Coverage is a figure this tier does supply, and `0` is its answer, not a placeholder.
        assert_eq!(figure(&fields, "coverage").as_deref(), Some("0"));
    }

    /// A ring build can bound its Collections without publishing the total the exact pause is
    /// differenced from. What it measured is still a figure; what it cannot reconstruct is
    /// absent.
    #[test]
    fn a_build_that_prices_no_total_keeps_the_pause_it_measured() {
        let unpriced = GenerationSummary {
            pause_total_ns: None,
            scale_factor: None,
            ..timed(0)
        };
        let fields = only_generation(&[block(7, 0, vec![unpriced])]);

        assert_eq!(
            figure(&fields, "pause_measured_ns").as_deref(),
            Some("1000")
        );
        assert_eq!(figure(&fields, "pause_total_ns"), None);
        assert_eq!(figure(&fields, "pause_mean_ns"), None);
    }

    /// JSON cannot spell a non-finite figure. Dropping the key keeps the document parseable
    /// and says what absence says everywhere else here.
    #[test]
    fn a_figure_json_cannot_spell_is_absent_rather_than_invalid() {
        let broken = GenerationSummary {
            coverage: f64::NAN,
            scale_factor: Some(f64::INFINITY),
            ..timed(0)
        };
        let fields = only_generation(&[block(7, 0, vec![broken])]);

        assert_eq!(figure(&fields, "coverage"), None, "{fields:?}");
        assert_eq!(figure(&fields, "scale_factor"), None, "{fields:?}");
    }

    /// Each interpreter of each process keeps its own object, the breakdown the table prints.
    /// Two interpreters of one process are two entries, never one summed pair.
    #[test]
    fn every_interpreter_of_every_process_keeps_its_own_object() {
        let document = encode(&[
            block(7, 0, vec![timed(0), timed(1)]),
            block(7, 3, vec![timed(0)]),
            block(900, 0, vec![counted(0)]),
        ]);

        let named: Vec<&str> = document
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(r#""pid""#) || line.starts_with(r#""interpreter""#))
            .collect();
        assert_eq!(
            named,
            [
                r#""pid": 7,"#,
                r#""interpreter": 0,"#,
                r#""pid": 7,"#,
                r#""interpreter": 3,"#,
                r#""pid": 900,"#,
                r#""interpreter": 0,"#,
            ]
        );
        assert_eq!(generations(&document).len(), 4);
    }

    /// A run that read no ring still hands its consumer a document. Nothing to report is an
    /// empty list, not an empty file the CI job has to special-case.
    #[test]
    fn a_run_that_read_nothing_still_writes_a_document() {
        assert_eq!(
            encode(&[]),
            "{\n  \"schema\": \"gcscope.gc-summary/1\",\n  \"interpreters\": []\n}\n"
        );
    }

    /// The document reaches the path the operator named, whole.
    #[test]
    fn the_document_reaches_the_path_it_was_asked_for() {
        let path = std::env::temp_dir().join(format!(
            "gcscope_summary_json_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let summary = [block(7, 0, vec![timed(0)])];

        write(path.to_str().unwrap(), &summary).expect("the summary is written");
        let written = std::fs::read_to_string(&path).expect("the file is there");
        std::fs::remove_file(&path).ok();

        assert_eq!(written, encode(&summary));
    }

    /// A path gcscope cannot write says so, rather than losing the summary quietly.
    #[test]
    fn a_path_that_cannot_be_written_is_reported() {
        let path = std::env::temp_dir().join("gcscope_no_such_directory_here");
        let error = write(path.join("summary.json").to_str().unwrap(), &[])
            .expect_err("a missing directory is not writable");
        assert!(
            format!("{error}").contains("summary.json"),
            "the message names the path: {error}"
        );
    }
}
