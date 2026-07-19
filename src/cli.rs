//! Command-line entry point used by `src/main.rs`.
//!
//! The exact public surface (subcommands, flags, and no-argument rewriting)
//! is documented in `CONTRACTS.md` under "CLI contract". This file holds the
//! CLI parser and the entry-point function. Implementation of the
//! individual subcommand bodies lives in the relevant module; `caduceus run`
//! ultimately delegates to the orchestration tick defined in phase 7.

use std::ffi::OsString;

use clap::{Parser, Subcommand};

use caduceus::config::{Config, SetupAction};
use caduceus::error::{CaduceusError, CaduceusResult};
use caduceus::issue::IssueKey;
use caduceus::queue::StateStore;
use caduceus::DaemonLock;

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
        from: Option<std::path::PathBuf>,
        /// Report what would change without modifying anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Migrate to the SQLite state store.
        #[arg(long, default_value_t = false, conflicts_with = "from")]
        to_sqlite: bool,
    },
    /// Generate minimal non-secret configuration.
    #[command(name = "setup", about = "Generate minimal non-secret configuration")]
    Setup {
        /// Print the planned action without writing.
        #[arg(long)]
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
        /// Drop the persisted `FinalizationCheckpoint` along with
        /// the run-tracking fields. By default the checkpoint is
        /// preserved so a follow-up tick resumes from the saved
        /// branch / PR. When this flag is set, the CLI surfaces
        /// the branch / PR in a warning so the operator can
        /// reconcile manually; the daemon never deletes the
        /// remote branch or PR itself.
        #[arg(long, default_value_t = false)]
        force_finalization_reset: bool,
    },
    /// Create a new generation for an issue (reopen or reprocess).
    /// Increments the generation counter and moves the entry to
    /// `Queued` if it was in a terminal phase.
    Reprocess {
        /// `owner/repo#number` identifier.
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

    let cli = Cli::parse_from(args);
    match cli.command {
        Some(Command::Queue {
            action:
                QueueAction::Reset {
                    issue,
                    dry_run,
                    force_finalization_reset,
                },
        }) => run_queue_reset(&issue, dry_run, force_finalization_reset),
        Some(Command::Queue {
            action: QueueAction::Reprocess { issue, dry_run },
        }) => run_queue_reprocess(&issue, dry_run),
        Some(Command::WorktreeGc {
            older_than_days,
            dry_run,
        }) => run_worktree_gc(older_than_days, dry_run),
        Some(Command::Run) => {
            // Resolve the config through the same env-aware
            // chain the other subcommands use. Cron never sets
            // `CADUCEUS_CONFIG`, so a missing env var still
            // falls through to `Config::load()` and surfaces
            // a configuration error.
            let cfg = match std::env::var_os("CADUCEUS_CONFIG") {
                Some(path) => Config::load_from(std::path::Path::new(&path))?,
                None => Config::load()?,
            };
            let outcome = caduceus::tick::run_blocking(cfg)?;
            // Map the outcome to the documented exit code so
            // the cron model (Processed / Idle / Cancelled →
            // 0; Failed → 1) holds without changing the CLI.
            let exit_code = caduceus::tick::exit_code_for_tests(&outcome);
            std::process::exit(exit_code as i32);
        }
        Some(Command::Status { json }) => {
            // Load the same config the canonical tick
            // would use, then render the report.
            let config = match std::env::var_os("CADUCEUS_CONFIG") {
                Some(path) => caduceus::config::Config::load_from(std::path::Path::new(&path))?,
                None => caduceus::config::Config::load()?,
            };
            let (output, diagnostic) = caduceus::status::report(&config.state_dir, json)?;
            if json {
                println!("{output}");
            } else {
                print!("{output}");
            }
            // Map the diagnostic to the documented exit code
            // per RUN-005:
            //   - No diagnostic → exit 0 (valid rendered state)
            //   - NoState → exit 2 (missing state)
            //   - CorruptState or CorruptQueue → exit 1
            match diagnostic {
                Some(caduceus::status::StatusDiagnostic::NoState) => {
                    std::process::exit(2);
                }
                Some(
                    caduceus::status::StatusDiagnostic::CorruptState { .. }
                    | caduceus::status::StatusDiagnostic::CorruptQueue { .. },
                ) => {
                    std::process::exit(1);
                }
                None => {}
            }
            Ok(())
        }
        Some(Command::MigrateState {
            from,
            dry_run,
            to_sqlite,
        }) => {
            if to_sqlite {
                run_migrate_state_to_sqlite(dry_run)
            } else if let Some(from_path) = from {
                run_migrate_state(&from_path, dry_run)
            } else {
                Err(CaduceusError::Config(
                    "either --from <path> or --to-sqlite is required".to_string(),
                ))
            }
        }
        Some(Command::Setup { dry_run }) => {
            let hermes_home = match std::env::var_os("HERMES_HOME") {
                Some(h) => std::path::PathBuf::from(&h),
                None => {
                    eprintln!("caduceus: $HERMES_HOME is required for setup");
                    return Err(CaduceusError::Config(
                        "HERMES_HOME must be set for setup".to_string(),
                    ));
                }
            };
            let report = caduceus::config::setup_config(&hermes_home, dry_run)?;
            match report.action {
                SetupAction::Created => {
                    println!("caduceus setup: created {}", report.path.display());
                }
                SetupAction::Updated => {
                    println!("caduceus setup: updated {}", report.path.display());
                }
                SetupAction::Skipped => {} // dry-run already printed
            }
            Ok(())
        }
        // Every other subcommand is a stub for now; `run` is the
        // canonical "no-op success" so the cron tick contract
        // (silent on success) holds while the rest of the daemon
        // is being built.
        _ => Ok(()),
    }
}

/// `caduceus worktree-gc [--older-than-days N] [--dry-run]` —
/// the v0.1 maintenance entry point that sweeps stale
/// worktrees across every repository in
/// `config.watched_repos`.
///
/// The action is a thin wrapper around
/// [`caduceus::worktree::gc`]; it owns config loading, the
/// `DaemonLock` (so a tick is not concurrent with the sweep),
/// and the report rendering.
fn run_worktree_gc(older_than_days: u64, dry_run: bool) -> CaduceusResult<()> {
    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let state_dir = config.state_dir.clone();
    // The GC may legitimately take seconds when many
    // worktrees are present; the daemon lock is non-blocking
    // so a concurrent tick wins the race. We log a clear
    // error so the operator can re-run later.
    let _daemon = match DaemonLock::try_acquire(&state_dir)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "caduceus: another tick holds {}/daemon.lock; refusing to GC",
                state_dir.display()
            );
            return Err(CaduceusError::Worktree {
                context: "gc",
                stderr: "another tick is in progress; refusing to GC".to_string(),
            });
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("build tokio runtime: {err}"),
        })?;
    let removed = rt.block_on(caduceus::worktree::gc(&config, older_than_days, dry_run))?;
    if dry_run {
        println!("caduceus worktree-gc: dry-run complete; 0 worktrees removed (use without --dry-run to apply)");
    } else {
        println!("caduceus worktree-gc: removed {removed} worktree(s)");
    }
    Ok(())
}

/// `caduceus queue reset <owner/repo#number>` — the only v0.1
/// recovery operation for a `Failed` or `Skipped` entry.
///
/// The CLI acquires `DaemonLock` and the `state.lock` (so it
/// cannot run concurrently with a tick) and refuses entries
/// with an active claim file. The persisted
/// `FinalizationCheckpoint` is preserved by default; the
/// `--force-finalization-reset` flag drops it and surfaces the
/// branch / PR in a warning so the operator can reconcile
/// manually. The daemon never deletes the remote branch or PR
/// itself.
fn run_queue_reset(
    issue: &str,
    dry_run: bool,
    force_finalization_reset: bool,
) -> CaduceusResult<()> {
    let key = IssueKey::parse(issue)?;
    // Honour $CADUCEUS_CONFIG for explicit operator scripts; fall
    // back to the canonical resolution chain otherwise. The cron
    // tick uses `Config::load`, but a CLI reset typically runs
    // from a script that already has the config path in env.
    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let state_dir = config.state_dir.clone();
    if dry_run {
        // Dry-run: read-only. Don't take the daemon lock — we
        // still need to load the state to report what would
        // change, but we never write.
        let store = StateStore::open(&state_dir)?;
        let snap = store.snapshot()?;
        let entry = snap.entry(&key).ok_or_else(|| CaduceusError::Queue {
            context: "reset",
            stderr: format!("no entry for {}", key.display_key()),
        })?;
        let checkpoint = store.finalization_for(&key)?;
        println!(
            "would reset {} (phase={:?}, attempts={}, last_error={:?}, last_run_id={:?})",
            key.display_key(),
            entry.phase,
            entry.attempts,
            entry.last_error,
            entry.last_run_id
        );
        if let Some(check) = checkpoint.as_ref() {
            println!(
                "  finalization checkpoint (would {}): branch={:?}, run_id={:?}, stage={:?}, pr_url={:?}",
                if force_finalization_reset { "drop" } else { "preserve" },
                check.branch_name,
                check.run_id,
                check.stage,
                check.pr_url
            );
        }
        return Ok(());
    }
    // Live path: take the daemon lock first so a concurrent tick
    // can't run while we're mutating state. Then take the
    // state.lock (acquired inside `StateStore::reset_entry`).
    let _daemon = match DaemonLock::try_acquire(&state_dir)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "caduceus: another tick holds {}/daemon.lock; refusing to reset",
                state_dir.display()
            );
            return Err(CaduceusError::Queue {
                context: "reset",
                stderr: "another tick is in progress; refusing to reset".to_string(),
            });
        }
    };
    let store = StateStore::open(&state_dir)?;
    let outcome = store.reset_entry(&key, force_finalization_reset)?;
    println!("reset {} to Queued", key.display_key());
    if let Some(check) = outcome.dropped_checkpoint.as_ref() {
        eprintln!(
            "warning: dropped finalization checkpoint branch={:?} run_id={:?} stage={:?} pr_url={:?} pr_number={:?} commit_oid={:?}",
            check.branch_name,
            check.run_id,
            check.stage,
            check.pr_url,
            check.pr_number,
            check.commit_oid
        );
        eprintln!(
            "warning: the remote branch and PR were NOT deleted; reconcile manually if appropriate"
        );
    }
    Ok(())
}

/// `caduceus queue reprocess <issue>` — create a new generation
/// for the issue, incrementing its generation counter and moving
/// it back to `Queued` if it was in a terminal phase.
fn run_queue_reprocess(issue: &str, dry_run: bool) -> CaduceusResult<()> {
    use caduceus::issue::IssueKey;
    use caduceus::queue::{QueueEntry, StateStore};

    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let key = IssueKey::parse(issue)
        .map_err(|e| CaduceusError::Config(format!("invalid issue key: {e}")))?;
    let state_dir = &config.state_dir;
    let store = StateStore::open(state_dir)?;
    let mut snap = store.snapshot()?;
    let entry = snap.entry(&key).ok_or_else(|| CaduceusError::Queue {
        context: "reprocess",
        stderr: format!("entry {} not found in queue", key.display_key()),
    })?;

    // Increment the generation.
    let new_generation = entry.generation.saturating_add(1);

    if dry_run {
        println!(
            "reprocess {}: current generation={}, would set generation={}",
            key.display_key(),
            entry.generation,
            new_generation,
        );
        return Ok(());
    }

    store.reprocess_entry(&key)?;
    println!(
        "reprocessed {}: new generation={}",
        key.display_key(),
        new_generation,
    );
    Ok(())
}

/// `caduceus migrate-state --to-sqlite [--dry-run]` —
/// migrate the current JSON state to the SQLite store.
fn run_migrate_state_to_sqlite(dry_run: bool) -> CaduceusResult<()> {
    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let state_dir = config.state_dir.clone();
    let report = caduceus::migrate_to_sqlite::migrate_to_sqlite(
        &state_dir,
        dry_run,
        caduceus::migrate_to_sqlite::LockPolicy::Acquire,
    )?;
    match &report.outcome {
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::Migrated { entries } => {
            println!("caduceus migrate-state: migrated {entries} entries to SQLite");
        }
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::DryRun { would_migrate } => {
            println!("caduceus migrate-state: dry-run; would migrate {would_migrate} entries");
        }
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::AlreadyCurrent => {
            println!("caduceus migrate-state: already current; no changes");
        }
    }
    Ok(())
}

/// `caduceus migrate-state --from <path> [--dry-run]` —
/// import a legacy v0 state file into the current schema
/// under `<state_dir>/state.json`. The import path is
/// idempotent: a second invocation with the same input
/// against an unchanged live state is a no-op. See
/// `MIGRATION.md` for the rollout, rollback, and recovery
/// procedures.
fn run_migrate_state(from: &std::path::Path, dry_run: bool) -> CaduceusResult<()> {
    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let state_dir = config.state_dir.clone();
    let report = caduceus::migrate::run(from, &state_dir, dry_run)?;
    match &report.outcome {
        caduceus::migrate::MigrationOutcome::Imported { migrated, skipped } => {
            println!("caduceus migrate-state: imported {migrated} entries, skipped {skipped}");
        }
        caduceus::migrate::MigrationOutcome::DryRun {
            would_migrate,
            would_skip,
        } => {
            println!(
                "caduceus migrate-state: dry-run; would import {would_migrate}, would skip {would_skip}"
            );
        }
        caduceus::migrate::MigrationOutcome::AlreadyCurrent => {
            println!("caduceus migrate-state: already current; no changes");
        }
    }
    Ok(())
}
