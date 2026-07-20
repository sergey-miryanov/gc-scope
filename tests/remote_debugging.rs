//! Live coverage for the remote_debugging "live bucket" — the functions that read a
//! real interpreter's memory and so can't be unit-tested: `version::detect`,
//! `offsets::read_offsets`, `PySession`'s readers + `gc_stats`, and the pre-3.13
//! `find_runtime_pre_3_13` path. `SpawnedPython` gives us the live target.
//!
//! Like tests/lifecycle.rs these attach to a live process (ptrace/taskport) and are
//! `#[ignore]`d; run with `cargo test -- --ignored` where attach is permitted.

mod common;

use common::{python_version, test_python, SpawnedPython};

use gcscope::memory::process;
use gcscope::remote_debugging::offsets::{self, pre_3_13};
use gcscope::remote_debugging::session::PySession;
use gcscope::remote_debugging::version;

/// `version::detect` reads the running interpreter's version from memory (the
/// `Py_Version` symbol on 3.13+, a rodata string scan otherwise). It must agree with
/// what the interpreter reports for itself. Version-independent — runs on the whole
/// supported range.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn detect_matches_the_interpreter_version() {
    let Some(python) = test_python() else {
        eprintln!("SKIP detect_matches_the_interpreter_version: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let detected = version::detect(proc.pid()).expect("detect a live interpreter's version");
    if let Some((major, minor)) = python_version(&python) {
        assert_eq!(
            (detected.major, detected.minor),
            (major, minor),
            "version::detect disagrees with `python --version`"
        );
    }
}

/// `read_offsets` finds `_PyRuntime`, reads the live `_Py_DebugOffsets` version word,
/// and resolves the matching (or same-minor fallback) compiled layout. 3.13+ only:
/// `_Py_DebugOffsets` doesn't exist before then.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn read_offsets_resolves_runtime_and_a_same_minor_layout() {
    let Some(python) = test_python() else {
        eprintln!("SKIP read_offsets_resolves_runtime_and_a_same_minor_layout: no Python found");
        return;
    };
    if let Some(ver) = python_version(&python)
        && ver < (3, 13)
    {
        eprintln!(
            "SKIP read_offsets_resolves_runtime_and_a_same_minor_layout: needs 3.13+, got {}.{}",
            ver.0, ver.1
        );
        return;
    }
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let detected = version::detect(pid).expect("detect version");
    let (runtime_addr, live_word, resolved) =
        offsets::read_offsets(pid, &detected).expect("read_offsets on 3.13+");

    assert_ne!(runtime_addr, 0, "_PyRuntime address must be non-zero");
    // The live version word decodes to the same minor line as detect saw.
    let live = version::PythonVersion::from_hex(live_word).expect("live version word decodes");
    assert_eq!(
        (live.major, live.minor),
        (detected.major, detected.minor),
        "the live _Py_DebugOffsets word disagrees with detect"
    );
    // The resolved layout is same-minor (an exact hit, or the ABI-frozen fallback);
    // its hex need not equal the live word, but the major.minor must match.
    assert_eq!(
        resolved.expected_version() & 0xffff_0000,
        live_word & 0xffff_0000,
        "resolved layout is from a different minor than the live interpreter"
    );
}

/// A `PySession` reads memory through `read`/`read_u64`/`read_i64`; pointed at
/// `_PyRuntime` they must all land on the `"xdebugpy"` cookie and agree on its bytes.
/// Also pins `supports_gc_stats` for a modern build. 3.13+ (the cookie is 3.13+).
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn session_readers_agree_on_the_runtime_cookie() {
    let Some(python) = test_python() else {
        eprintln!("SKIP session_readers_agree_on_the_runtime_cookie: no Python found");
        return;
    };
    if let Some(ver) = python_version(&python)
        && ver < (3, 13)
    {
        eprintln!(
            "SKIP session_readers_agree_on_the_runtime_cookie: needs 3.13+, got {}.{}",
            ver.0, ver.1
        );
        return;
    }
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let session = PySession::attach(proc.pid()).expect("attach to the live interpreter");
    let addr = session.runtime_addr();
    assert_ne!(addr, 0);

    let bytes = session.read(addr, 8).expect("read the cookie via PySession::read");
    assert_eq!(&bytes, b"xdebugpy", "the runtime address must start with the debug cookie");

    let word = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    assert_eq!(session.read_u64(addr).expect("read_u64"), word);
    assert_eq!(session.read_i64(addr).expect("read_i64"), word as i64);

    assert!(session.supports_gc_stats(), "a 3.13+ build must support GC stats");
}

/// `gc_stats` walks the interpreter's per-generation stats from live memory. It must
/// read without error on any build that reports GC-stats support (an idle interpreter
/// may legitimately have collected nothing, so the vec can be empty — the contract
/// under test is a clean read, not a specific count).
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn gc_stats_reads_cleanly_on_a_supported_build() {
    let Some(python) = test_python() else {
        eprintln!("SKIP gc_stats_reads_cleanly_on_a_supported_build: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let session = PySession::attach(proc.pid()).expect("attach to the live interpreter");
    if !session.supports_gc_stats() {
        eprintln!("SKIP gc_stats_reads_cleanly_on_a_supported_build: build reports no GC-stats support");
        return;
    }
    session
        .gc_stats(false)
        .expect("gc_stats must read cleanly on a supported live interpreter");
}

/// The pre-3.13 runtime finder resolves `_PyRuntime` via the `_PyRuntime` symbol and
/// a structural cross-reference (no cookie exists yet). Needs a genuinely pre-3.13
/// interpreter — set `GCSCOPE_TEST_PYTHON` to one — and skips on 3.13+, where the
/// cookie-based `find_runtime` is the path instead.
#[test]
#[ignore = "attaches to a live process; needs ptrace/taskport — run with --ignored"]
fn find_runtime_pre_3_13_locates_the_runtime() {
    let Some(python) = test_python() else {
        eprintln!("SKIP find_runtime_pre_3_13_locates_the_runtime: no Python found");
        return;
    };
    let Some((major, minor)) = python_version(&python) else {
        eprintln!("SKIP find_runtime_pre_3_13_locates_the_runtime: could not determine version");
        return;
    };
    if (major, minor) >= (3, 13) {
        eprintln!(
            "SKIP find_runtime_pre_3_13_locates_the_runtime: needs pre-3.13 (set GCSCOPE_TEST_PYTHON), got {major}.{minor}"
        );
        return;
    }
    let table = pre_3_13::table_for_version(major, minor)
        .unwrap_or_else(|| panic!("no legacy layout for {major}.{minor}"));

    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let (addr, path) = process::find_runtime_pre_3_13(proc.pid(), &table)
        .expect("find _PyRuntime on a pre-3.13 interpreter");

    assert_ne!(addr, 0, "_PyRuntime address must be non-zero");
    assert!(
        path.to_lowercase().contains("python"),
        "the resolving module should be a python image, got {path:?}"
    );
}
