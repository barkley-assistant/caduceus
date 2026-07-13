//! `caduceus` binary entry point.
//!
//! The CLI parses the canonical subcommands listed in `CONTRACTS.md` under
//! "CLI contract": `run`, `status`, `worktree-gc`, `queue reset`, and
//! `migrate-state`. A no-argument invocation is equivalent to `caduceus run`
//! — that rewriting happens inside the CLI parser, before Clap dispatches,
//! so a bare cron tick never prints help or version output.
//!
//! `run` is silent on success (per the Cron model in `CONTRACTS.md`); all
//! diagnostics go to stderr.

use std::process::ExitCode;

use caduceus::config::Config;
use caduceus::error::CaduceusResult;

mod cli;

fn main() -> ExitCode {
    // The CLI router inspects `args_os` and inserts `run` when the user
    // invoked `caduceus` with no arguments, before Clap parsing. This is
    // the contractually documented behaviour (CONTRACTS.md, "Implement
    // no-argument behavior by inspecting `args_os`...").
    match cli::run() {
        Ok(()) => ExitCode::from(0),
        Err(err) => {
            // Diagnostics to stderr; cron captures nothing on success.
            eprintln!("caduceus: {err}");
            err.exit_code()
        }
    }
}

/// Parse configuration through the canonical resolver chain. Used by both
/// `run` and `status`; wrapper around the typed loader so it remains easy
/// to grow during later phases without touching `main`.
#[allow(dead_code)]
pub(crate) fn load_config() -> CaduceusResult<Config> {
    Config::load()
}
