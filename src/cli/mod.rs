//! Command-line entry point used by `src/main.rs`.
//!
//! The exact public surface (subcommands, flags, and no-argument rewriting)
//! is documented in the CLI contract. This file holds the CLI parser and the
//! entry-point function. Implementation of the
//! individual subcommand bodies lives in the relevant module; `caduceus run`
//! ultimately delegates to `caduceus::tick::run_blocking`.

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use clap::{Parser, Subcommand};

use caduceus::config::{Config, SetupAction};
use caduceus::error::{CaduceusError, CaduceusResult};
use caduceus::executor::oci::OciExecutor;
use caduceus::executor::{Executor, ExecutorSpec, IssueWorkTarget, WorkTarget};
use caduceus::issue::IssueKey;
use caduceus::queue::{
    display_digest, Phase, QueueEntry, QueueState, RemoveOutcome, StateStore, TicketType,
};
use caduceus::readiness::{self, DiagnosticCanary, DiagnosticStatus, ReadinessVerdict};
use caduceus::DaemonLock;

static GIT_AUTHOR_WARNED: AtomicBool = AtomicBool::new(false);

/// Schema version of the `queue` JSON envelope emitted by
/// `queue show`, `queue reset --json`, and `queue remove --json`.
/// Bumped when the envelope shape changes so consumers can detect
/// the version. Distinct from `STATUS_SCHEMA_VERSION` — the queue
/// and status schemas version independently.
const QUEUE_SCHEMA_VERSION: &str = "queue/1.0";

/// Caduceus v1.0.0: poll GitHub, queue one unit of work per tick, finalise
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
    /// Check live OCI production readiness.
    Doctor {
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Do not run the optional diagnostic canary.
        #[arg(long)]
        skip_canary: bool,
        /// Digest-pinned image to use for the optional diagnostic canary.
        #[arg(long)]
        canary_image: Option<String>,
        /// Command argv to use for the optional diagnostic canary.
        #[arg(long, num_args = 1..)]
        canary_command: Vec<String>,
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
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
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
    /// List queue entries, or print full detail for one entry.
    Show {
        /// Optional `owner/repo#number` identifier; when omitted
        /// every entry is listed as a table.
        issue: Option<String>,
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Remove a queue entry entirely.
    ///
    /// Drops only the queue entry: the worktree, claim file, remote
    /// branch, and PR are left for the reaper / `worktree-gc` and
    /// are never touched. Refuses `InProgress`, `AwaitingReview`,
    /// and `Done` by default; `--force` relaxes the phase guard only
    /// (an active claim file is always refused).
    Remove {
        /// `owner/repo#number` identifier (validated by `issue::IssueKey`).
        issue: String,
        /// Print the planned change without applying it.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Print machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Relax the phase guard (allow `InProgress`, `AwaitingReview`,
        /// `Done`). The active-claim-file guard is never relaxable.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

/// Drive the CLI from `main`.
///
/// A bare `caduceus` invocation is rewritten to `caduceus run` before
/// Clap parsing. The rewrite uses `args_os()` per the CLI contract,
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
                    json,
                    force_finalization_reset,
                },
        }) => run_queue_reset(&issue, dry_run, json, force_finalization_reset),
        Some(Command::Queue {
            action: QueueAction::Reprocess { issue, dry_run },
        }) => run_queue_reprocess(&issue, dry_run),
        Some(Command::Queue {
            action: QueueAction::Show { issue, json },
        }) => run_queue_show(issue.as_deref(), json),
        Some(Command::Queue {
            action:
                QueueAction::Remove {
                    issue,
                    dry_run,
                    json,
                    force,
                },
        }) => run_queue_remove(&issue, dry_run, force, json),
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
            let (host_name, host_email) = caduceus::finalize::commit::host_git_identity();
            let name_from_tier3 = cfg.git_author_name.is_none() && host_name.is_none();
            let email_from_tier3 = cfg.git_author_email.is_none() && host_email.is_none();
            if (name_from_tier3 || email_from_tier3)
                && !GIT_AUTHOR_WARNED.swap(true, Ordering::SeqCst)
            {
                tracing::warn!("git_author: no config or host identity resolved — falling back to \"Caduceus Daemon <caduceus@daemon.local>\". Configure git_author_name + git_author_email in the caduceus: config block to silence this warning.");
            }
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
        Some(Command::Doctor {
            json,
            skip_canary,
            canary_image,
            canary_command,
        }) => run_doctor(json, skip_canary, canary_image, canary_command),
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

fn run_doctor(
    json: bool,
    skip_canary: bool,
    canary_image: Option<String>,
    canary_command: Vec<String>,
) -> CaduceusResult<()> {
    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| CaduceusError::Other(format!("build doctor runtime: {err}")))?;
    let mut report = runtime.block_on(readiness::run_live(&config));
    if !skip_canary {
        let image = canary_image.or_else(|| std::env::var("CADUCEUS_DOCTOR_CANARY_IMAGE").ok());
        let command = if canary_command.is_empty() {
            std::env::var("CADUCEUS_DOCTOR_CANARY_COMMAND")
                .ok()
                .map(|value| value.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            canary_command
        };
        let canary = runtime.block_on(run_canary(&config, &report, image, command));
        report.diagnostic_canary = Some(canary);
    } else {
        report.diagnostic_canary = Some(DiagnosticCanary {
            status: DiagnosticStatus::Skip,
            detail: "disabled by --skip-canary".to_string(),
        });
    }
    if let Err(err) = readiness::write_informational_report(&config.state_dir, &report) {
        eprintln!("caduceus doctor: could not write informational report: {err}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", readiness::render_human(&report));
    }
    if report.verdict == ReadinessVerdict::Unavailable {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_canary(
    config: &Config,
    report: &caduceus::readiness::ReadinessReport,
    image: Option<String>,
    command: Vec<String>,
) -> DiagnosticCanary {
    let Some(image) = image else {
        return DiagnosticCanary {
            status: DiagnosticStatus::Skip,
            detail:
                "no canary image configured (use --canary-image or CADUCEUS_DOCTOR_CANARY_IMAGE)"
                    .to_string(),
        };
    };
    if command.is_empty() {
        return DiagnosticCanary {
            status: DiagnosticStatus::Skip,
            detail: "no canary command configured (use --canary-command or CADUCEUS_DOCTOR_CANARY_COMMAND)".to_string(),
        };
    }
    if report.verdict != ReadinessVerdict::Ready {
        return DiagnosticCanary {
            status: DiagnosticStatus::Skip,
            detail: "mandatory readiness is unavailable; canary was not attempted".to_string(),
        };
    }
    let Some(sandbox) = config.sandbox.as_ref() else {
        return DiagnosticCanary {
            status: DiagnosticStatus::Skip,
            detail: "OCI sandbox is not configured".to_string(),
        };
    };
    let mut canary_config = config.clone();
    let mut canary_sandbox = sandbox.clone();
    canary_sandbox.image = image;
    canary_sandbox.pull_policy = caduceus::config::OciPullPolicy::IfMissing;
    canary_config.sandbox = Some(canary_sandbox);

    let run_id = format!("doctor-canary-{}", uuid::Uuid::new_v4().simple());
    let canary_root = std::env::temp_dir().join(&run_id);
    canary_config.state_dir = canary_root.join("state");
    canary_config.repo_storage_root = canary_root.join("repos");
    canary_config.workdir_base = canary_root.join("workdirs");
    canary_config.log_path = canary_root.join("doctor-canary.log");
    let canary_paths = [
        canary_config.state_dir.clone(),
        canary_config.repo_storage_root.clone(),
        canary_config.workdir_base.clone(),
    ];
    if let Err(err) = canary_paths.iter().try_for_each(std::fs::create_dir_all) {
        return DiagnosticCanary {
            status: DiagnosticStatus::Failure,
            detail: format!("cannot create canary storage: {err}"),
        };
    }
    #[cfg(unix)]
    for path in &canary_paths {
        if let Err(err) =
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        {
            let _ = std::fs::remove_dir_all(&canary_root);
            return DiagnosticCanary {
                status: DiagnosticStatus::Failure,
                detail: format!("cannot secure canary storage: {err}"),
            };
        }
    }
    let worktree = canary_config.workdir_base.join(&run_id);
    if let Err(err) = std::fs::create_dir_all(&worktree) {
        let _ = std::fs::remove_dir_all(&canary_root);
        return DiagnosticCanary {
            status: DiagnosticStatus::Failure,
            detail: format!("cannot create canary worktree: {err}"),
        };
    }
    let issue = match IssueKey::parse("caduceus/doctor#1") {
        Ok(issue) => issue,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&canary_root);
            return DiagnosticCanary {
                status: DiagnosticStatus::Failure,
                detail: format!("cannot create canary issue key: {err}"),
            };
        }
    };
    let spec = ExecutorSpec {
        self_exe: std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("caduceus")),
        target: WorkTarget::Issue(IssueWorkTarget {
            key: issue,
            title: "OCI readiness canary".to_string(),
            body: "Diagnostic only".to_string(),
            labels: Vec::new(),
            branch_name: "doctor-canary".to_string(),
        }),
        worktree: worktree.clone(),
        run_id,
        context_json: "{}".to_string(),
        worker_command: command,
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let executor = OciExecutor::new(
        canary_config.clone(),
        std::sync::Arc::new(caduceus::infra::disk::DiskPressureGuard::from_config(
            &canary_config,
        )),
    );
    let outcome = executor.run(&spec).await;
    let issue_key = match &spec.target {
        WorkTarget::Issue(issue) => &issue.key,
        WorkTarget::PullRequest(_) => {
            return DiagnosticCanary {
                status: DiagnosticStatus::Failure,
                detail: "doctor canary only supports issue targets".to_string(),
            };
        }
    };
    let result = match outcome {
        Ok(outcome) => {
            match caduceus::worker::parse_result_file(&outcome.result_path, issue_key) {
                Ok(result) => DiagnosticCanary {
                    status: DiagnosticStatus::Pass,
                    detail: format!("production path completed with {:?} result", result.status),
                },
                Err(err) => DiagnosticCanary {
                    status: DiagnosticStatus::Failure,
                    detail: format!("result artifact validation failed: {err}"),
                },
            }
        }
        Err(err) => DiagnosticCanary {
            status: DiagnosticStatus::Failure,
            detail: format!("production path failed: {err}"),
        },
    };
    let _ = std::fs::remove_dir_all(canary_root);
    result
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
    let review_removed = {
        let runner = caduceus::worktree::GitRunner::new(&config);
        rt.block_on(caduceus::repo::review_worktree::gc_review_worktrees(
            &config,
            &runner,
            older_than_days,
            dry_run,
        ))?
    };
    if dry_run {
        println!("caduceus worktree-gc: dry-run complete; 0 worktrees removed (use without --dry-run to apply)");
    } else {
        println!(
            "caduceus worktree-gc: removed {removed} worktree(s) and {review_removed} review worktree(s)"
        );
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
    json: bool,
    force_finalization_reset: bool,
) -> CaduceusResult<()> {
    let key = IssueKey::parse(issue)?;
    // Honour $CADUCEUS_CONFIG for explicit operator scripts; fall
    // back to the canonical resolution chain otherwise. The cron
    // tick uses `Config::load`, but a CLI reset typically runs
    // from a script that already has the config path in env.
    let config = resolve_queue_config()?;
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
        if json {
            print_queue_json(
                &state_dir,
                serde_json::json!({
                    "action": "reset",
                    "dry_run": true,
                    "key": key.display_key(),
                    "cleared_finalization": force_finalization_reset,
                    "dropped_checkpoint": checkpoint,
                }),
            )?;
            return Ok(());
        }
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
    if json {
        print_queue_json(&state_dir, serde_json::to_value(&outcome)?)?;
        return Ok(());
    }
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

/// `caduceus queue show [<issue>] [--json]` — read-only inspection
/// of the queue. The list form renders every entry as a table in
/// `BTreeMap` (lexical) order; the detail form renders one entry
/// including its finalization checkpoint. Both forms support
/// `--json` with the versioned queue envelope. `show` never takes
/// the daemon lock and never writes state.
fn run_queue_show(issue: Option<&str>, json: bool) -> CaduceusResult<()> {
    let config = resolve_queue_config()?;
    let state_dir = config.state_dir.clone();
    let store = StateStore::open(&state_dir)?;
    let snap = store.snapshot()?;
    match issue {
        None => {
            if json {
                let entries: Vec<&QueueEntry> = snap.entries.values().collect();
                print_queue_json(&state_dir, serde_json::json!({ "entries": entries }))?;
            } else {
                println!("{}", render_queue_table(&snap));
            }
        }
        Some(issue_text) => {
            let key = IssueKey::parse(issue_text)?;
            match snap.entry(&key) {
                Some(entry) => {
                    if json {
                        print_queue_json(&state_dir, serde_json::to_value(entry)?)?;
                    } else {
                        println!("{}", render_entry_detail(entry));
                    }
                }
                None => {
                    let err = CaduceusError::Queue {
                        context: "show",
                        stderr: format!("no entry for {}", key.display_key()),
                    };
                    if json {
                        print_queue_json_with_diagnostic(
                            &state_dir,
                            serde_json::Value::Null,
                            Some("no_entry"),
                        )?;
                    }
                    return Err(err);
                }
            }
        }
    }
    Ok(())
}

/// `caduceus queue remove <issue> [--dry-run] [--force] [--json]` —
/// drop a queue entry entirely. Only the queue entry is removed:
/// the worktree, claim file, remote branch, and PR are left for the
/// reaper / `worktree-gc` and are never touched. `--force` relaxes
/// the phase guard only; an active claim file is always refused.
/// The live path takes the daemon lock so a concurrent tick cannot
/// run while state is mutated; the dry-run path is read-only and
/// never takes the lock.
fn run_queue_remove(issue: &str, dry_run: bool, force: bool, json: bool) -> CaduceusResult<()> {
    let key = IssueKey::parse(issue)
        .map_err(|e| CaduceusError::Config(format!("invalid issue key: {e}")))?;
    let config = resolve_queue_config()?;
    let state_dir = config.state_dir.clone();
    if dry_run {
        // Dry-run: read-only. Don't take the daemon lock — we
        // still need to load the state to report what would
        // change, but we never write.
        let store = StateStore::open(&state_dir)?;
        let snap = store.snapshot()?;
        let entry = snap.entry(&key).ok_or_else(|| CaduceusError::Queue {
            context: "remove",
            stderr: format!("no entry for {}", key.display_key()),
        })?;
        // Mirror the live guards so a dry-run surfaces the same
        // refusal the operator would hit on the real remove.
        if !force {
            match entry.phase {
                Phase::InProgress | Phase::AwaitingReview | Phase::Done => {
                    return Err(CaduceusError::Queue {
                        context: "remove",
                        stderr: format!(
                            "refusing to remove entry {}: phase is {:?}; use --force to override",
                            key.display_key(),
                            entry.phase
                        ),
                    });
                }
                _ => {}
            }
        }
        let digest = display_digest(&key.display_key());
        let claim_path = store.claims_dir().join(format!("{digest}.claim"));
        if claim_path.is_file() {
            return Err(CaduceusError::Queue {
                context: "remove",
                stderr: format!(
                    "refusing to remove entry {}: active claim file exists",
                    key.display_key()
                ),
            });
        }
        let outcome = RemoveOutcome {
            key: key.display_key(),
            phase: entry.phase.as_str().to_string(),
            dropped_checkpoint: entry.finalization.clone(),
        };
        if json {
            print_queue_json(&state_dir, serde_json::to_value(&outcome)?)?;
        } else {
            println!("would remove {} (phase={})", outcome.key, outcome.phase);
            if outcome.dropped_checkpoint.is_some() {
                println!("  finalization checkpoint would be dropped");
            }
        }
        return Ok(());
    }
    // Live path: take the daemon lock first so a concurrent tick
    // can't run while we're mutating state. Then take the
    // state.lock (acquired inside `StateStore::remove_entry`).
    let _daemon = match DaemonLock::try_acquire(&state_dir)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "caduceus: another tick holds {}/daemon.lock; refusing to remove",
                state_dir.display()
            );
            return Err(CaduceusError::Queue {
                context: "remove",
                stderr: "another tick is in progress; refusing to remove".to_string(),
            });
        }
    };
    let store = StateStore::open(&state_dir)?;
    let outcome = store.remove_entry(&key, force)?;
    if json {
        print_queue_json(&state_dir, serde_json::to_value(&outcome)?)?;
        return Ok(());
    }
    println!("removed {}", outcome.key);
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

/// Resolve the queue CLI config through `$CADUCEUS_CONFIG` first,
/// then the canonical chain — the same resolution every queue
/// subcommand uses.
fn resolve_queue_config() -> CaduceusResult<Config> {
    match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(Path::new(&path)),
        None => Config::load(),
    }
}

/// Print a versioned `queue/1.0` JSON envelope to stdout.
fn print_queue_json(state_dir: &Path, payload: serde_json::Value) -> CaduceusResult<()> {
    print_queue_json_with_diagnostic(state_dir, payload, None)
}

/// Print a versioned `queue/1.0` JSON envelope with an optional
/// top-level `diagnostic` string, mirroring the `status --json`
/// envelope convention.
fn print_queue_json_with_diagnostic(
    state_dir: &Path,
    payload: serde_json::Value,
    diagnostic: Option<&str>,
) -> CaduceusResult<()> {
    let envelope = serde_json::json!({
        "app_version": env!("CARGO_PKG_VERSION"),
        "schema": QUEUE_SCHEMA_VERSION,
        "state_dir": state_dir.display().to_string(),
        "diagnostic": diagnostic,
        "payload": payload,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).map_err(|err| {
            CaduceusError::Other(format!("serialise queue JSON envelope: {err}"))
        })?
    );
    Ok(())
}

/// Stable snake_case label for a ticket type (independent of the
/// serde rename attribute on [`TicketType`]).
fn ticket_type_label(ticket_type: TicketType) -> &'static str {
    match ticket_type {
        TicketType::Code => "code",
        TicketType::Investigation => "investigation",
    }
}

/// Render the human list table for `queue show`. Columns: key,
/// phase, ticket type, attempts, generation, and age (seconds since
/// `updated_at`). Entries iterate in `BTreeMap` lexical order.
fn render_queue_table(state: &QueueState) -> String {
    if state.entries.is_empty() {
        return "queue: no entries".to_string();
    }
    let now = Utc::now();
    let mut out = String::from("key\tphase\tticket\tattempts\tgeneration\tage\n");
    for entry in state.entries.values() {
        let age = (now - entry.updated_at).num_seconds().max(0);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}s\n",
            entry.key.display_key(),
            entry.phase.as_str(),
            ticket_type_label(entry.ticket_type),
            entry.attempts,
            entry.generation,
            age,
        ));
    }
    out
}

/// Render the human detail view for `queue show <key>`, including
/// the finalization checkpoint (branch, run id, stage, PR).
fn render_entry_detail(entry: &QueueEntry) -> String {
    let mut out = String::new();
    out.push_str(&format!("entry {}\n", entry.key.display_key()));
    out.push_str(&format!("  phase: {}\n", entry.phase.as_str()));
    out.push_str(&format!(
        "  ticket_type: {}\n",
        ticket_type_label(entry.ticket_type)
    ));
    out.push_str(&format!("  attempts: {}\n", entry.attempts));
    out.push_str(&format!("  last_error: {:?}\n", entry.last_error));
    out.push_str(&format!("  last_run_id: {:?}\n", entry.last_run_id));
    out.push_str(&format!("  next_attempt_at: {:?}\n", entry.next_attempt_at));
    out.push_str(&format!("  queued_at: {}\n", entry.queued_at.to_rfc3339()));
    out.push_str(&format!(
        "  updated_at: {}\n",
        entry.updated_at.to_rfc3339()
    ));
    out.push_str(&format!("  generation: {}\n", entry.generation));
    out.push_str(&format!("  blocked_source: {:?}\n", entry.blocked_source));
    out.push_str(&format!(
        "  blocked_recovery_hint: {:?}\n",
        entry.blocked_recovery_hint
    ));
    match entry.finalization.as_ref() {
        Some(check) => {
            out.push_str("  finalization:\n");
            out.push_str(&format!("    run_id: {}\n", check.run_id));
            out.push_str(&format!("    branch_name: {}\n", check.branch_name));
            out.push_str(&format!(
                "    result_path: {}\n",
                check.result_path.display()
            ));
            out.push_str(&format!("    stage: {}\n", check.stage.as_str()));
            out.push_str(&format!("    commit_oid: {:?}\n", check.commit_oid));
            out.push_str(&format!("    pr_number: {:?}\n", check.pr_number));
            out.push_str(&format!("    pr_url: {:?}\n", check.pr_url));
        }
        None => {
            out.push_str("  finalization: none\n");
        }
    }
    out
}

/// `caduceus queue reprocess <issue>` — create a new generation
/// for the issue, incrementing its generation counter and moving
/// it back to `Queued` if it was in a terminal phase.
fn run_queue_reprocess(issue: &str, dry_run: bool) -> CaduceusResult<()> {
    use caduceus::issue::IssueKey;
    use caduceus::queue::StateStore;

    let config = match std::env::var_os("CADUCEUS_CONFIG") {
        Some(path) => Config::load_from(std::path::Path::new(&path))?,
        None => Config::load()?,
    };
    let key = IssueKey::parse(issue)
        .map_err(|e| CaduceusError::Config(format!("invalid issue key: {e}")))?;
    let state_dir = &config.state_dir;
    let store = StateStore::open(state_dir)?;
    let snap = store.snapshot()?;
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
    let cfg_path = resolve_config_path_for_write();
    let config = match cfg_path.as_ref() {
        Some(path) => Config::load_from(path)?,
        None => Config::load()?,
    };
    let state_dir = config.state_dir.clone();
    let report = caduceus::migrate_to_sqlite::migrate_to_sqlite(
        &state_dir,
        dry_run,
        caduceus::migrate_to_sqlite::LockPolicy::Acquire,
        cfg_path.as_deref(),
    )?;
    match &report.outcome {
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::Migrated { entries } => {
            println!(
                "caduceus migrate-state: migrated {entries} entries to SQLite (backend: sqlite)"
            );
        }
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::DryRun { would_migrate } => {
            println!("caduceus migrate-state: dry-run; would migrate {would_migrate} entries (backend would be: sqlite)");
        }
        caduceus::migrate_to_sqlite::SqliteMigrationOutcome::AlreadyCurrent => {
            println!("caduceus migrate-state: already current; backend: json");
        }
    }
    Ok(())
}

/// Best-effort resolution of the config file path so the migration
/// command can write `state_backend` back to the same file the daemon
/// loads from. Returns `None` when no canonical path is known (the
/// migration still succeeds, but the config is left untouched).
fn resolve_config_path_for_write() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CADUCEUS_CONFIG") {
        return Some(std::path::PathBuf::from(path));
    }
    if let Some(hermes_home) = std::env::var_os("HERMES_HOME") {
        return Some(std::path::PathBuf::from(hermes_home).join("config.yaml"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(std::path::PathBuf::from(home).join(".config/caduceus/config.yaml"));
    }
    None
}

/// `caduceus migrate-state --from <path> [--dry-run]` —
/// import a legacy v0 state file into the current schema
/// under `<state_dir>/state.json`. The import path is
/// idempotent: a second invocation with the same input
/// against an unchanged live state is a no-op. See
/// "The migrate-state Subcommand" in
/// https://github.com/barkley-assistant/caduceus/wiki/State-Recovery for
/// the rollout, rollback, and recovery procedures.
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
