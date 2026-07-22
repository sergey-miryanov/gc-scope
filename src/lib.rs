//! gcscope's implementation, exposed as a library.
//!
//! The split exists so integration tests in `tests/*.rs` can reach `PySession`
//! and the offset registry directly. A binary-only crate can only be driven
//! through `CARGO_BIN_EXE_gcscope`, which is enough for the CLI-level smoke
//! assertions but cannot observe the layout-cache hit or the soft-reattach
//! revalidation path — both are in-process state with no CLI surface. See
//! `docs/adr/0005-testing-strategy.md`.
//!
//! `src/main.rs` is a thin CLI dispatcher over this crate and holds no logic
//! beyond argument parsing.
//!
//! Module layers, foundation upward:
//! - [`memory`] — read the target's process memory and parse its binary images.
//! - [`remote_debugging`] — the CPython runtime model: version detection, the
//!   offset system, [`PySession`](remote_debugging::session::PySession), GC-stat
//!   decoding. The single source of truth for *reading* the runtime.
//! - [`snapshot`] / [`monitor`] — two consumers of that model: a one-shot snapshot
//!   collector (rendered by [`tui`]) and a streaming event monitor.
//! - [`cli`] — command definitions and handlers; the top layer `main.rs` dispatches to.

pub mod cli;
pub mod list_pids;
pub mod memory;
pub mod monitor;
pub mod remote_debugging;
pub mod snapshot;
pub mod tui;
