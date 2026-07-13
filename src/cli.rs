//! Command-line entry point used by `src/main.rs`.
//!
//! The exact public surface (subcommands, flags, and no-argument rewriting)
//! is documented in `CONTRACTS.md` under "CLI contract". This file holds the
//! CLI parser and the entry-point function. Implementation of the
//! individual subcommand bodies lives in the relevant module; `caduceus run`
//! ultimately delegates to the orchestration tick defined in phase 7.

use std::ffi::OsString;

use clap::{Parser, Subcommand};

use caduceus::error::CaduceusResult;

/// Caduceus v0.1: poll GitHub, queue one unit of work per tick, finalise
/// code or investigation results.
#[derive(Debug, Parser)]
#[command(
    name = "caduceus",
    bin_name = "caduceus",
    version,
    about = "GitHub issue orchestrator with worker supervision",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Canonical subcommands.
///
/// A `None` value means the user invoked `caduceus` with no arguments and
/// the entry-point rewrites that to `caduceus run` before Clap dispatches.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a single tick (default subcommand).
    Run,
    /// Report daemon state.
    Status {
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Garbage-collect stale worktrees.
    WorktreeGc {
        /// Older-than threshold in days.
        #[arg(long, default_value_t = 30)]
        older_than_days: u64,
        /// Report eligible worktrees without removing them.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Reset queue state for a specific issue.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Migrate legacy queue state into the current schema.
    MigrateState {
        /// Path to the legacy state directory.
        #[arg(long)]
        from: std::path::PathBuf,
        /// Report what would change without modifying anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

/// Nested subcommand for `caduceus queue`.
#[derive(Debug, Subcommand)]
pub enum QueueAction {
    /// Move a terminal entry back to `Queued`.
    Reset {
        /// `owner/repo#number` identifier (validated by `issue::IssueKey`).
        issue: String,
        /// Print the planned change without applying it.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

/// Drive the CLI from `main`.
///
/// Subcommand bodies are filled in by their respective phase gates; this
/// stub returns `Ok(())` so the compile/fmt/clippy gates for Task 0.1 hold
/// without prematurely committing the orchestration loop body.
///
/// A bare `caduceus` invocation is rewritten to `caduceus run` before
/// Clap parsing. The rewrite uses `args_os()` per `CONTRACTS.md`,
/// "Implement no-argument behavior by inspecting `args_os` and inserting
/// `run` before Clap parsing"; a `--version` / `--help` flag is *not*
/// considered a bare invocation and is dispatched normally.
pub fn run() -> CaduceusResult<()> {
    let mut args: Vec<OsString> = std::env::args_os().collect();

    // `args_os()` returns at least the program name. If the only argument
    // is the program name, insert `run` so the user sees identical
    // behaviour to `caduceus run`.
    if args.len() == 1 {
        args.push(OsString::from("run"));
    }

    let _ = Cli::parse_from(args);
    Ok(())
}
