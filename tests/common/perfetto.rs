//! Run `tests/harness/perfetto_dump.py` and hand back the document it printed, parsed.
//!
//! The process boundary and the parsing are solved once, here, so a test asserting about a
//! trace writes about the trace. The document's shape is the harness's to define; this side
//! only runs it and hands back `serde_json::Value`.
//!
//! Why a real parser rather than the shell-out-and-print-lines pattern the live tests use: the
//! question these tests ask is often "does the trace carry *exactly* these keys and no others",
//! and a flat text protocol answers it by substring match, which passes whether or not the
//! surprise was there. A key set is the assertion; a parsed document is what makes it one.

// Each test binary compiles its own copy of `common`, so a helper only some of them use looks
// unused in the others.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::test_python;

/// The document version this helper reads. `perfetto_dump.py` publishes it under `schema`, and
/// bumping it there without bumping it here is meant to fail loudly.
const SCHEMA: &str = "gcscope.perfetto-dump/1";

/// The same, for the field-number checker, which speaks a document of its own.
const CONSTANTS_SCHEMA: &str = "gcscope.proto-constants/1";

/// What the harness printed and what it concluded.
pub struct Dump {
    /// The parsed document. See `tests/harness/README.md` for its keys.
    pub document: Value,
    /// 0 the trace is structurally valid, 1 it is not. A harness failure never gets this far —
    /// [`dump`] panics on one, since it means the test could not be run rather than that the
    /// trace was bad.
    pub exit_code: i32,
}

impl Dump {
    /// The packets of the wire view. Panics if the bytes did not parse as a trace, which is a
    /// separate failure from an invalid one and worth a different message.
    pub fn packets(&self) -> &Vec<Value> {
        self.document["wire"]["packet"]
            .as_array()
            .unwrap_or_else(|| panic!("the trace did not parse:\n{}", self.problems()))
    }

    /// The rows of one trace-processor query, by the name the harness gave it.
    pub fn rows(&self, view: &str) -> &Vec<Value> {
        let value = &self.document["trace_processor"][view];
        value.as_array().unwrap_or_else(|| {
            panic!("no `{view}` rows; the harness said: {value}. Did you skip the query engine?")
        })
    }

    pub fn is_valid(&self) -> bool {
        self.document["validity"]["ok"] == Value::Bool(true)
    }

    /// Every structural problem, one per line, for an assertion message.
    pub fn problems(&self) -> String {
        match self.document["validity"]["problems"].as_array() {
            Some(problems) if !problems.is_empty() => problems
                .iter()
                .map(|p| format!("  {} — {}", p["check"], p["detail"]))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "  (none)".to_string(),
        }
    }
}

/// The keys of one JSON object, sorted. What an "and nothing else" assertion compares — the
/// question a text dump cannot answer, since an unexpected field is a token nobody read.
pub fn keys(value: &Value) -> Vec<&str> {
    let mut names: Vec<&str> = value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    names
}

fn harness_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("harness")
}

fn harness_script() -> PathBuf {
    harness_dir().join("perfetto_dump.py")
}

/// Write one of `reference_traces.py`'s traces to `out` and return the path.
///
/// These are authored with the protobuf bindings, so a decoder checked against them has been
/// shown a trace this repo did not encode. `kind` is a name from that file's `TRACES`.
pub fn reference_trace(python: &Path, kind: &str, out: &Path) -> PathBuf {
    let status = Command::new(python)
        .arg(harness_dir().join("reference_traces.py"))
        .arg(kind)
        .arg(out)
        .output()
        .expect("build a reference trace");
    assert!(
        status.status.success(),
        "could not build the `{kind}` trace: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    out.to_path_buf()
}

/// An interpreter that can run the harness, or `None`.
///
/// It calls into the package rather than importing the name: `import perfetto` succeeds on the
/// protobuf half alone, and a test that needs the query engine would then fail deep inside the
/// harness instead of skipping here.
pub fn harness_python() -> Option<PathBuf> {
    let python = test_python()?;
    let ok = Command::new(&python)
        .args([
            "-c",
            "from perfetto.protos.perfetto.trace.perfetto_trace_pb2 import Trace; Trace()",
        ])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    ok.then_some(python)
}

/// Whether a missing `perfetto` package must **fail** rather than skip.
///
/// Skipping is right on a laptop and wrong on the leg that just installed the package, where it
/// would turn a failed install into a green run. That leg sets `GCSCOPE_REQUIRE_PERFETTO=1`.
pub fn perfetto_required() -> bool {
    std::env::var("GCSCOPE_REQUIRE_PERFETTO").ok().as_deref() == Some("1")
}

/// An interpreter that can run the harness, or `None` after logging a skip.
pub fn harness_python_or_skip(test: &str) -> Option<PathBuf> {
    if let Some(python) = harness_python() {
        return Some(python);
    }
    let msg = "no interpreter with the `perfetto` package \
               (install it: pip install -r tests/harness/requirements.txt)";
    assert!(
        !perfetto_required(),
        "GCSCOPE_REQUIRE_PERFETTO=1 but {msg}; a leg that installs it must not pass by skipping"
    );
    eprintln!("SKIP {test}: {msg}");
    None
}

/// Hand a claims document to `proto_constants.py` and give back the verdict it printed.
///
/// The claims go over stdin rather than through a file: the table being checked is compiled
/// into this test binary, so writing it out somewhere would only add a path to clean up.
pub fn check_constants(python: &Path, claims: &str) -> Value {
    let mut child = Command::new(python)
        .arg(harness_dir().join("proto_constants.py"))
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the constants checker");
    child
        .stdin
        .take()
        .expect("the checker reads stdin")
        .write_all(claims.as_bytes())
        .expect("hand over the claims");

    let out = child.wait_with_output().expect("the checker finishes");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(
        out.status.code().unwrap_or(-1),
        2,
        "the checker could not run: {stderr}"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let verdict: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("the checker printed something that is not JSON: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });
    assert_eq!(
        verdict["schema"], CONSTANTS_SCHEMA,
        "the checker speaks a document version this helper does not: {}",
        verdict["schema"]
    );
    verdict
}

/// Both views of `trace`. Starts the query engine, which downloads its binary on first use.
pub fn dump(python: &Path, trace: &Path) -> Dump {
    run(python, trace, false)
}

/// The wire view alone — no query engine, so no download and no second process.
pub fn dump_wire_only(python: &Path, trace: &Path) -> Dump {
    run(python, trace, true)
}

fn run(python: &Path, trace: &Path, skip_trace_processor: bool) -> Dump {
    let mut command = Command::new(python);
    command.arg(harness_script()).arg(trace);
    if skip_trace_processor {
        command.arg("--skip-trace-processor");
    }

    let out = command.output().expect("run the dump harness");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().unwrap_or(-1);

    // 2 is the harness saying it could not run at all. That is this test's problem, not the
    // trace's, and swallowing it would look like a trace with no packets.
    assert_ne!(
        code,
        2,
        "the harness could not read {}: {stderr}",
        trace.display()
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let document: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("the harness printed something that is not JSON: {e}\nstdout: {stdout}\nstderr: {stderr}")
    });

    // The harness versions its document for a reason, so read it. Without this, a harness that
    // moved a key answers `Value::Null` to every lookup and the failure surfaces as
    // "the trace did not parse" with an empty problem list — a test blaming the trace for a
    // mismatch between these two files.
    assert_eq!(
        document["schema"], SCHEMA,
        "the harness speaks a document version this helper does not: {}",
        document["schema"]
    );

    Dump {
        document,
        exit_code: code,
    }
}
