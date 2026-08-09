//! Argument handling that has to be true before anything is attached to or spawned.
//!
//! Unlike the rest of `tests/`, nothing here needs a Python interpreter or ptrace: it runs the
//! gcscope binary with arguments it must refuse, and asserts it refused. So these are not
//! `#[ignore]`d — they run in the default `cargo test`.

use std::path::PathBuf;
use std::process::Command;

/// Run the gcscope binary, returning `(exit code, stdout, stderr)`. The two streams stay
/// apart: clap prints a refusal on stderr and `--help` on stdout, and each test asserts on
/// the one that matters.
fn gcscope(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gcscope"))
        .args(args)
        .output()
        .expect("spawn gcscope");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("gcscope_cli_{}_{}.json", std::process::id(), tag))
}

/// A format gcscope does not have is refused with the list of the ones it does, and refused
/// early enough that no target was touched. `monitor` names a PID that may well exist; the
/// point is that the run never gets far enough to care.
#[test]
fn an_unrecognised_format_is_refused_with_the_accepted_list() {
    for args in [
        vec!["monitor", "1234", "--format", "perfetto-ish"],
        vec![
            "run",
            "-p",
            "python",
            "-s",
            "spin.py",
            "--format",
            "perfetto-ish",
        ],
    ] {
        let (code, _, stderr) = gcscope(&args);
        assert_ne!(code, 0, "{args:?} was accepted\n{stderr}");
        assert!(
            stderr.contains("perfetto-ish"),
            "the refusal does not say what was rejected: {stderr}"
        );
        assert!(
            stderr.contains("chrome"),
            "the refusal does not name what is accepted: {stderr}"
        );
    }
}

/// The refusal happens before the output path is opened, so a bad `--format` cannot cost an
/// operator the trace already sitting there — nor leave an empty file where there was none.
#[test]
fn an_unrecognised_format_neither_creates_nor_truncates_the_output_file() {
    let existing = temp_path("existing");
    let absent = temp_path("absent");
    std::fs::write(&existing, "an earlier run's trace").unwrap();
    std::fs::remove_file(&absent).ok();

    for path in [&existing, &absent] {
        let (code, _, stderr) = gcscope(&[
            "monitor",
            "1234",
            "-o",
            &path.to_string_lossy(),
            "--format",
            "perfetto-ish",
        ]);
        assert_ne!(code, 0, "{stderr}");
    }

    assert_eq!(
        std::fs::read_to_string(&existing).unwrap(),
        "an earlier run's trace"
    );
    assert!(!absent.exists(), "an empty trace was left behind");
    std::fs::remove_file(&existing).ok();
}

/// The flag is optional and the default is the format that shipped before it existed, so an
/// operator's existing command line means what it meant. `--help` is the cheapest place to
/// read that back, and it is also where the accepted values are documented.
#[test]
fn help_documents_the_flag_its_default_and_its_values() {
    for subcommand in ["monitor", "run"] {
        let (code, help, stderr) = gcscope(&[subcommand, "--help"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(help.contains("--format"), "{subcommand}: {help}");
        assert!(help.contains("[default: chrome]"), "{subcommand}: {help}");
    }
}
