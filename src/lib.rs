//! gcscope's implementation, exposed as a library.
//!
//! The split exists so integration tests in `tests/*.rs` can reach `PySession`
//! and the offset registry directly. A binary-only crate can only be driven
//! through `CARGO_BIN_EXE_gcscope`, which is enough for the CLI-level smoke
//! assertions but cannot observe the layout-cache hit or the soft-reattach
//! revalidation path — both are in-process state with no CLI surface. See
//! `docs/tests-harness-plan.md` §3.4.
//!
//! `src/main.rs` is a thin CLI dispatcher over this crate and holds no logic
//! beyond argument parsing.

pub mod cli;
pub mod cli_monitor;
pub mod cli_monitor_options;
pub mod diagram;
pub mod exporters;
pub mod list_pids;
pub mod memory;
pub mod monitor;
pub mod monitor_loop;
pub mod remote_debugging;
