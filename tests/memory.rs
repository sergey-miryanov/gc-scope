//! Live-process coverage for `src/memory/` — the functions that inspect a real
//! process (its maps, modules, child tree, and memory) and so are unreachable by
//! the in-crate unit tests. `SpawnedPython` gives us a real interpreter to point
//! them at; the private finders (`find_section_in_*`, `validate_cookie`,
//! `try_find_runtime`) are covered transitively through `find_runtime`.
//!
//! Anything that reads the target's memory or its `proc_maps` entries needs
//! ptrace/taskport — not granted in the plain `build` CI job — so those are
//! `#[ignore]`d like tests/lifecycle.rs; run them with `cargo test -- --ignored`
//! where attach is permitted. The `sysinfo`-based `read_cmdline` needs no such
//! permission and runs under plain `cargo test`. With no Python present every
//! test skips with a log rather than failing.

mod common;

use common::{pid_alive, python_version, test_python, SpawnedPython};

use std::thread;
use std::time::{Duration, Instant};

use gcscope::memory::{binary, process, reader, regions};

/// `read_cmdline` is the reused-PID change-detector behind `PySession::revalidate`.
/// It must both name the running program and track liveness: `Some(text naming the
/// program)` while the PID runs, `None` once it's gone. The naming half is the
/// regression guard for the Windows fix — the plain `refresh_processes` left `cmd`
/// empty there, so the text must now come back populated on every platform, not
/// just the Unixes.
///
/// Uses `sysinfo`, so — like tests/spawn.rs — it needs no attach permission and
/// runs in the plain build job.
#[test]
fn read_cmdline_tracks_liveness_and_names_the_fixture() {
    let Some(python) = test_python() else {
        eprintln!("SKIP read_cmdline_tracks_liveness_and_names_the_fixture: no Python found (set GCSCOPE_TEST_PYTHON)");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let cmd = process::read_cmdline(pid).expect("a live PID must resolve to a command line");
    assert!(
        cmd.contains("spin.py"),
        "read_cmdline should name the running fixture, got: {cmd:?}"
    );

    drop(proc); // kill + reap the interpreter
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "spin.py should be gone after drop");

    assert!(
        process::read_cmdline(pid).is_none(),
        "a reaped PID must have no command line (the signal revalidate relies on)"
    );
}

/// `find_python_modules` is the first step of every runtime lookup: it must return
/// at least the interpreter/libpython, every entry python-named and mapped at a
/// non-zero base. Version-independent — it inspects the process image, not CPython
/// internals — so it runs on the whole 3.8–3.15 matrix.
#[test]
#[ignore = "reads proc_maps; needs ptrace/taskport — run with --ignored"]
fn find_python_modules_lists_the_interpreter() {
    let Some(python) = test_python() else {
        eprintln!("SKIP find_python_modules_lists_the_interpreter: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let modules = process_modules(proc.pid());
    assert!(!modules.is_empty(), "a live interpreter must expose python modules");
    for (path, base) in &modules {
        assert!(
            path.to_lowercase().contains("python"),
            "find_python_modules must only return python-named modules, got {path:?}"
        );
        assert_ne!(*base, 0, "module {path:?} was mapped at a zero base");
    }
}

fn process_modules(pid: u32) -> Vec<(String, usize)> {
    binary::find_python_modules(pid).expect("find_python_modules on a live child")
}

/// `list_regions` must return the process's memory map, including at least one
/// mapping backed by a python image. This is the raw material the whole finder
/// walks. Version-independent.
#[test]
#[ignore = "reads proc_maps; needs ptrace/taskport — run with --ignored"]
fn list_regions_includes_a_python_mapping() {
    let Some(python) = test_python() else {
        eprintln!("SKIP list_regions_includes_a_python_mapping: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let regions = regions::list_regions(proc.pid()).expect("list_regions on a live child");
    assert!(!regions.is_empty(), "a live process must have mapped regions");
    assert!(
        regions.iter().any(|m| {
            m.filename()
                .and_then(|p| p.to_str())
                .is_some_and(|s| s.to_lowercase().contains("python"))
        }),
        "at least one region should be backed by a python image"
    );
}

/// `get_child_pids` of the interpreter must be empty: `spin.py` spawns nothing, so
/// it is a leaf. This pins the "no children" answer the recursive runtime search
/// bottoms out on — a spurious child would make the search recurse into a wrong or
/// non-existent PID. (`get_child_pids` swallows its own errors into an empty vec,
/// so this also confirms it doesn't misreport a valid leaf.)
#[test]
#[ignore = "enumerates the child process tree; needs taskport on macOS — run with --ignored"]
fn get_child_pids_of_a_leaf_is_empty() {
    let Some(python) = test_python() else {
        eprintln!("SKIP get_child_pids_of_a_leaf_is_empty: no Python found");
        return;
    };
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let children = process::get_child_pids(proc.pid());
    assert!(
        children.is_empty(),
        "spin.py is a leaf; get_child_pids should be empty, got {children:?}"
    );
}

/// The keystone: `find_runtime` locates `_PyRuntime`, and the reader must agree.
///
/// This drives the entire host-platform finder chain — `try_find_runtime` →
/// `find_section_in_<format>` → `validate_cookie` — then independently confirms the
/// address by reading it back through every reader entry point. The `_PyRuntime`
/// section and its `"xdebugpy"` cookie only exist from 3.13 on, so this is gated;
/// pre-3.13 uses the separate `find_runtime_pre_3_13` path (covered by live_smoke).
#[test]
#[ignore = "attaches and reads process memory; needs ptrace/taskport — run with --ignored"]
fn find_runtime_locates_the_cookie() {
    let Some(python) = test_python() else {
        eprintln!("SKIP find_runtime_locates_the_cookie: no Python found");
        return;
    };
    if let Some(ver) = python_version(&python)
        && ver < (3, 13)
    {
        eprintln!(
            "SKIP find_runtime_locates_the_cookie: needs 3.13+ for the _PyRuntime section, got {}.{}",
            ver.0, ver.1
        );
        return;
    }
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");
    let pid = proc.pid();

    let addr = process::find_runtime(pid).expect("find_runtime should locate _PyRuntime on 3.13+");
    assert_ne!(addr, 0, "_PyRuntime address must be non-zero");

    // find_runtime already validated the cookie internally; the reader path must
    // independently land on the same bytes, or the two disagree about the address.
    let bytes = reader::read_memory(pid, addr, 8).expect("read the cookie via read_memory");
    assert_eq!(
        &bytes, b"xdebugpy",
        "the runtime address must start with the debug cookie"
    );

    // read_u64 must decode exactly those 8 bytes, little-endian.
    let word = reader::read_u64(pid, addr).expect("read the cookie word via read_u64");
    assert_eq!(
        word,
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        "read_u64 must be the little-endian decode of the same 8 bytes"
    );

    // The handle-reuse (`*_h`) readers — the hot-path variants — must agree too.
    let handle = reader::open_handle(pid).expect("open_handle on a live child");
    let via_handle = reader::read_memory_h(&handle, addr, 8).expect("read via a reused handle");
    assert_eq!(
        &via_handle, b"xdebugpy",
        "read_memory_h must read the same cookie as the one-shot read_memory"
    );
}

/// `find_runtime_module` returns both the address and the on-disk path of the
/// module whose `PyRuntime` validated — the identity a layout cache keys on. The
/// path must name a python image. 3.13+, for the same reason as above.
#[test]
#[ignore = "attaches and reads process memory; needs ptrace/taskport — run with --ignored"]
fn find_runtime_module_returns_a_python_path() {
    let Some(python) = test_python() else {
        eprintln!("SKIP find_runtime_module_returns_a_python_path: no Python found");
        return;
    };
    if let Some(ver) = python_version(&python)
        && ver < (3, 13)
    {
        eprintln!(
            "SKIP find_runtime_module_returns_a_python_path: needs 3.13+, got {}.{}",
            ver.0, ver.1
        );
        return;
    }
    let proc = SpawnedPython::spawn(&python).expect("spin.py should reach READY");

    let (addr, path) =
        process::find_runtime_module(proc.pid()).expect("find_runtime_module on 3.13+");
    assert_ne!(addr, 0, "_PyRuntime address must be non-zero");
    assert!(
        path.to_lowercase().contains("python"),
        "the validating module should be a python image, got {path:?}"
    );
}
