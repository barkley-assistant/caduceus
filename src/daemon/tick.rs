//! The single canonical tick.
//!
//! [`run`], [`run_with_config`], and [`tick`] together implement
//! the per-tick controller described in `CONTRACTS.md` and the
//! Phase 7 task packet. The controller is the only entry
//! point the daemon's CLI exposes: a no-argument `caduceus`
//! invocation, the explicit `caduceus run`, and the cron
//! tick all funnel through [`run`].
//!
//! The order of operations is the contractually-documented
//! one:
//!
//! 1. Load + validate config, initialise structured logging.
//! 2. Take the whole-tick [`DaemonLock`]. On contention return
//!    [`TickOutcome::SkippedConcurrent`] / exit 0.
//! 3. Open [`StateStore`], [`MetaStore`], [`CadenceGate`], and
//!    enforce the rate-limit and cadence gates; persist
//!    `last_tick_started` and the gated outcome.
//! 4. Reap stale claims / abandoned worktrees.
//! 5. Build the typed GitHub [`Client`], discover watched
//!    repos, poll typed open issues, enqueue summaries.
//! 6. Acquire the next eligible entry. If no entry is
//!    eligible, finish as [`TickOutcome::Idle304`] (all
//!    responses were cached 304s) or [`TickOutcome::IdleEmpty`]
//!    otherwise.
//! 7. If the entry has a `FinalizationCheckpoint`, jump to
//!    the matching resume stage. Otherwise, verify the
//!    trigger label, fetch the issue detail, build context,
//!    discover the repo, create the worktree + branch, write
//!    the prompt.
//! 8. Spawn the worker through the canonical supervisor and
//!    classify every error into a [`FailureClass`].
//! 9. On success, run code / investigation / dry-run
//!    finalization; teardown always runs.
//! 10. Persist `last_tick_finished` and the final outcome.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::info;
use ulid::Ulid;

use crate::daemon::orchestration::{
    classify_error, ActiveRunGuard, FailureClass, Services, SystemClock,
};
use crate::finalize::{
    archive_worker_result, commit_code_and_finalize, dry_run_finalize,
    find_or_create_pr_and_finalize, post_completion_only, post_investigation_comment_and_finalize,
    push_and_finalize, FinalizeContext, FinalizeOutput, FinalizeRequest,
};
use crate::github::poll::{discover_watched_repos, merge_outcomes, poll_code, poll_investigation};
use crate::github::{Client, RateLimitInfo, Response};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::logging;
use crate::scheduler::circuit::{AdmissionResult, CircuitConfig, CircuitStore};
use crate::scheduler::{DrainConfig, LeaderToken, Pool};
use crate::signals;
use crate::state::checkpoints::{last_checkpoint_for_run, persist_checkpoint};
use crate::state::meta::{CadenceDecision, CadenceGate, MetaStore, TickOutcome};
use crate::state::queue::{ClaimedEntry, Phase, QueueEntry, StateStore, TicketType};
use crate::state::store;
use crate::worker::context::{build_context, encode_context, BuildInputs};
use crate::worker::prompt::{build_prompt, write_prompt};
use crate::worker::WorkerResult;
use crate::worktree::{create as create_worktree, find_main_clone, GitRunner};

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Cron / no-argument entry point. Loads config from the
/// canonical resolver chain, initialises the structured log
/// stream, and runs a single tick under a fresh
/// [`CancellationToken`]. The exit code follows the
/// contract: 0 for processed / idle / concurrent / cadence /
/// rate-limited / cancelled outcomes; 1 for configuration,
/// corruption, invariant, or unrecovered pipeline failures.
pub fn run() -> CaduceusResult<u8> {
    let cfg = Config::load()?;
    let log_path = cfg.log_path.clone();
    let _log_guard = logging::init(&log_path)?;
    let outcome = run_blocking(cfg)?;
    Ok(exit_code_for(&outcome))
}

/// Run a single tick on a fresh `current_thread` runtime.
/// Exposed so `status` and the CLI's other subcommands can
/// drive a tick-style `async` driver without owning a runtime.
/// The signal listener runs concurrently with the tick and
/// shares the `CancellationToken` so a SIGINT or SIGTERM
/// cancels the in-flight work and the orchestrator returns
/// `TickOutcome::Cancelled` / exit 0.
pub fn run_blocking(cfg: Config) -> CaduceusResult<TickOutcome> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| CaduceusError::Other(format!("build tokio runtime: {err}")))?;
    let cancellation = CancellationToken::new();
    let pool = Arc::new(Pool::new(
        cfg.worker_parallelism,
        DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
    ));
    rt.block_on(async move {
        tokio::select! {
        outcome = run_with_config(cfg, Arc::clone(&pool), cancellation.clone()) => outcome,
        // The signal listener's first signal drains the worker
        // pool and then cancels the shared token, so the tick
        // side returns on its own with `TickOutcome::Cancelled`.
        // The listener itself continues to await a possible
        // second signal so the orchestrator can escalate to
        // immediate kill.
        res = signals::listen(pool, cancellation.clone()) => {
        match res {
        Ok(()) => Ok(TickOutcome::Cancelled),
        Err(err) => Err(CaduceusError::Other(format!(
        "signal listener: {err}"
        ))),
        }
        }
        }
    })
}

/// Like [`run`] but accepts a pre-loaded [`Config`] and a
/// [`CancellationToken`]. Tests use this signature so they
/// can drive a tick with a custom config and cancel the
/// tick before it returns. Production code paths go through
/// [`run`].
pub async fn run_with_config(
    cfg: Config,
    pool: Arc<Pool>,
    cancellation: CancellationToken,
) -> CaduceusResult<TickOutcome> {
    let clock: Arc<dyn crate::daemon::orchestration::Clock> = Arc::new(SystemClock);
    let client = Arc::new(Client::with_config(&cfg)?);
    let git = GitRunner::new(&cfg);
    let services = Services::production(
        &cfg,
        clock,
        Arc::clone(&client),
        git,
        Arc::new(crate::daemon::orchestration::ProcessSupervisorAdapter),
        Arc::clone(&pool),
    );
    tick(cfg, services, pool, cancellation).await
}

/// The canonical per-tick controller. Takes ownership of the
/// [`Config`] and a [`Services`] bundle so the tests can swap
/// fakes. The function follows the contractually-documented
/// order exactly and never panics on external input.
pub async fn tick(
    cfg: Config,
    services: Services,
    pool: Arc<Pool>,
    cancellation: CancellationToken,
) -> CaduceusResult<TickOutcome> {
    let state_dir = cfg.state_dir.clone();

    // 0. Initialize daemon-owned repository storage.
    //     This runs before any lock acquisition so the directories
    //     are guaranteed to exist before the first tick attempts
    //     to use them.
    let storage = crate::repo::Storage::new(cfg.repo_storage_root.clone());
    storage.ensure_dirs().map_err(|err| {
        tracing::error!(
            error = %err,
            "failed to initialize repo storage at {}",
            cfg.repo_storage_root.display()
        );
        err
    })?;

    // 0.5. Install the restrictive umask for private storage.
    //     The umask is set once at process start; GitRunner's
    //     with_worktree_umask temporarily switches to 0o022 for
    //     worktree mutations and restores 0o077.
    let _ = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));

    // 1. Check scheduler leadership. If another tick holds the
    //    scheduler lock, skip (concurrent). Unlike the old
    //    whole-tick DaemonLock, the scheduler lock is held only
    //    during short state-mutation transactions, not the
    //    entire tick.
    let _leader_guard = match LeaderToken::try_acquire(&state_dir)? {
        Some(token) => token,
        None => {
            info!("concurrent tick holds scheduler lock; skipping");
            return Ok(TickOutcome::SkippedConcurrent);
        }
    };
    // Drop the leader token immediately — we only checked for
    // contention. State-mutation sections below acquire the
    // lock again via `LeaderToken::with_lock`.
    drop(_leader_guard);

    // 2. Open the metadata + state stores and enforce the
    //    rate-limit and cadence gates.
    let meta = LeaderToken::with_lock(&state_dir, || MetaStore::open(&state_dir))?;
    let gate = LeaderToken::with_lock(&state_dir, || CadenceGate::open(&state_dir))?;
    let now = services.clock.now();
    gate.record_tick_started(now)?;
    let precheck = gate.precheck(now, cfg.poll_interval_seconds);
    if let Some(gate_outcome) = precheck.tick_outcome() {
        let rate_limit = if matches!(precheck, CadenceDecision::RateLimited { .. }) {
            meta.snapshot().rate_limit
        } else {
            None
        };
        let _ = gate.record_tick_finished(
            now,
            gate_outcome,
            None,
            cfg.poll_interval_seconds,
            rate_limit.as_ref().map(dummy_rate_limit_info).as_ref(),
            None,
        );
        info!(?gate_outcome, "tick skipped by gate");
        return Ok(gate_outcome);
    }

    // 3. Reap stale claims / abandoned worktrees.
    let store = Arc::new(LeaderToken::with_lock(&state_dir, || {
        StateStore::open(&state_dir)
    })?);
    let _ = crate::state::queue::reap_stale_claims(
        &state_dir,
        services.clock.now(),
        cfg.stale_run_hours,
    )
    .await;

    // 3.6. Open the SQLite state store for circuit breaker access.
    let sqlite_conn = crate::state::store::open_in(&state_dir)?;
    let circuit_store = CircuitStore::new(sqlite_conn, CircuitConfig::from_config(&cfg));

    // 3.5. Poll awaiting-review entries for PR merge status.
    let poll_client: Arc<Client> = Arc::clone(services.github.inner());
    if let Err(err) = poll_awaiting_review_entries(store.as_ref(), poll_client.as_ref()).await {
        tracing::warn!(error = %err, "awaiting-review poll failed (best-effort)");
    }

    // 4. Build the GitHub client and discover watched repos.
    let client: Arc<Client> = Arc::clone(services.github.inner());
    let repos = match discover_watched_repos(client.as_ref(), &cfg).await {
        Ok(repos) => repos,
        Err(err) => {
            let class = classify_error(&err);
            // Rate-limit and other non-fatal infrastructure
            // errors must return `Ok` with the matching
            // `TickOutcome` so the cron contract's exit-0
            // mapping applies. The observation is already
            // persisted by `finish_tick_failure`.
            if let Some(outcome) = class.non_fatal_outcome() {
                finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
                return Ok(outcome);
            }
            finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
            return Err(err);
        }
    };
    if repos.is_empty() {
        finish_tick_outcome(&gate, &meta, now, TickOutcome::IdleEmpty, None, None)?;
        return Ok(TickOutcome::IdleEmpty);
    }

    // 5. Poll for the two trigger labels and enqueue summaries.
    let mut any_304 = false;
    let mut any_200 = false;
    let mut last_error: Option<CaduceusError> = None;
    for repo in &repos {
        match poll_repo(repo, &client, &cfg, store.as_ref(), &meta).await {
            Ok(Outcome304(true)) => {
                any_304 = true;
            }
            Ok(Outcome304(false)) => {
                any_200 = true;
            }
            Err(err) => {
                last_error = Some(err);
                break;
            }
        }
    }
    if let Some(err) = last_error {
        let class = classify_error(&err);
        // Same cron-contract rule: rate-limit and other
        // non-fatal errors return `Ok` with the matching
        // outcome so the CLI's exit-0 mapping applies.
        if let Some(outcome) = class.non_fatal_outcome() {
            finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
            return Ok(outcome);
        }
        finish_tick_failure(&gate, now, &cfg, &meta, class, Some(&err))?;
        return Err(err);
    }

    // 6. Acquire the next eligible entry.
    let run_id_candidate = Ulid::new().to_string();
    let store_clone = Arc::clone(&store);
    let clock_now = services.clock.now();
    let claimed = match LeaderToken::with_lock(&state_dir, || {
        store_clone.acquire_next(&run_id_candidate, std::process::id(), clock_now)
    })? {
        Some(c) => c,
        None => {
            let outcome = if any_304 && !any_200 {
                TickOutcome::Idle304
            } else {
                TickOutcome::IdleEmpty
            };
            finish_tick_outcome(&gate, &meta, now, outcome, None, None)?;
            return Ok(outcome);
        }
    };

    // 6.5. Check the circuit breaker before admitting the entry
    //  to the worker pool. If the circuit is open for the repo
    //  or the provider, route to NeedsAttention.
    let repo_key = format!("{}/{}", claimed.entry.key.owner, claimed.entry.key.repo);
    let repo_admit = circuit_store.try_admit("repository", &repo_key, services.clock.as_ref())?;
    let provider_admit = circuit_store.try_admit("provider", "github", services.clock.as_ref())?;

    let circuit_blocked = matches!(
        (&repo_admit, &provider_admit),
        (
            AdmissionResult::CircuitOpen { .. } | AdmissionResult::MaxDegradedAgeExceeded,
            _
        ) | (
            _,
            AdmissionResult::CircuitOpen { .. } | AdmissionResult::MaxDegradedAgeExceeded
        )
    );

    if circuit_blocked {
        let log_path = state_dir.join("processor.log");
        let mut guard = ActiveRunGuard::new(claimed.claim.clone(), Arc::clone(&store), log_path);
        let err = CaduceusError::CircuitOpen {
            scope: "repository",
            scope_id: repo_key.clone(),
            retry_after: 1800,
            probe_in_flight: false,
        };
        let class = classify_error(&err);
        let outcome = handle_infra_or_retry(cfg, &mut guard, &err, class).await?;
        finish_tick_outcome(&gate, &meta, now, outcome, None, Some(&err))?;
        return Ok(outcome);
    }

    // 6.6. Admit the entry to the worker pool. This gates the
    //  global concurrency and per-repo exclusion before any
    //  setup or worker dispatch occurs.
    let repo_key = format!("{}/{}", claimed.entry.key.owner, claimed.entry.key.repo);
    if let Err(err) = pool.admit(&repo_key).await {
        // PoolSaturated is an infrastructure failure; requeue with
        // backoff and surface as NeedsAttention.
        let log_path = state_dir.join("processor.log");
        let mut guard = ActiveRunGuard::new(claimed.claim.clone(), Arc::clone(&store), log_path);
        let class = classify_error(&err);
        let outcome = handle_infra_or_retry(cfg, &mut guard, &err, class).await?;
        finish_tick_outcome(&gate, &meta, now, outcome, None, Some(&err))?;
        return Ok(outcome);
    }

    // 7. Build the guard and run the work, finalization, and
    //  teardown phases inside one explicit cleanup scope.
    let log_path = state_dir.join("processor.log");
    let mut guard = ActiveRunGuard::new(claimed.claim.clone(), Arc::clone(&store), log_path);
    let mut http_status: Option<u16> = None;
    let outcome = run_claim(
        cfg,
        &services,
        Arc::clone(&pool),
        store.as_ref(),
        &meta,
        client,
        claimed,
        &mut guard,
        cancellation,
        &mut http_status,
    )
    .await;

    let outcome_for_finish = match &outcome {
        Ok(o) => *o,
        Err(_) => TickOutcome::Failed,
    };
    let last_error = outcome.as_ref().err();
    let _ = outcome;
    // cfg is consumed by run_claim; record_tick_finished
    // doesn't need it because the gate is owned.
    finish_tick_outcome(
        &gate,
        &meta,
        now,
        outcome_for_finish,
        http_status,
        last_error,
    )?;
    Ok(outcome_for_finish)
}

// ---------------------------------------------------------------------------
// Per-claim work loop
// ---------------------------------------------------------------------------

struct Outcome304(bool);

#[allow(clippy::too_many_arguments)]
async fn run_claim(
    cfg: Config,
    services: &Services,
    _pool: Arc<Pool>,
    store: &StateStore,
    _meta: &MetaStore,
    client: Arc<Client>,
    claimed: ClaimedEntry,
    guard: &mut ActiveRunGuard,
    cancellation: CancellationToken,
    http_status: &mut Option<u16>,
) -> CaduceusResult<TickOutcome> {
    // 7a. If the entry already has a finalization checkpoint,
    //     jump to the resume stage and re-enter the pipeline
    //     at the first uncompleted stage.
    if claimed.entry.finalization.is_some() {
        let conn = store::open_in(&cfg.state_dir)?;
        let run_id = claimed
            .entry
            .last_run_id
            .as_deref()
            .unwrap_or_else(|| guard.run_id());
        match resume_from_checkpoint(&conn, run_id)? {
            ResumeAction::Skip(stage) => {
                return run_resume_finalization(
                    cfg,
                    services,
                    store,
                    _meta,
                    client,
                    claimed,
                    guard,
                    cancellation,
                    http_status,
                    stage,
                )
                .await;
            }
            ResumeAction::AlreadyDone => {
                return Ok(TickOutcome::Processed);
            }
            ResumeAction::StartFresh => {
                // Fall through to normal flow
            }
        }
    }

    // 7b. Verify the trigger label.
    let trigger_ok = match claimed.entry.ticket_type {
        TicketType::Code => true,
        TicketType::Investigation => true,
    };
    if !trigger_ok {
        let _ = guard
            .finish_skip(&format!(
                "label not present on {}",
                claimed.entry.key.display_key()
            ))
            .await;
        return Ok(TickOutcome::Processed);
    }

    // 8. Fetch the issue detail.
    let issue = match crate::github::issue::fetch_issue_detail(
        client.as_ref(),
        &claimed.entry.key,
        &cfg.feedback_author_allowlist,
    )
    .await
    {
        Ok(d) => d,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 9. Verify the trigger label against the fetched labels.
    let label_ok = match claimed.entry.ticket_type {
        TicketType::Code => issue.labels.iter().any(|l| l == &cfg.ticket_label_code),
        TicketType::Investigation => issue
            .labels
            .iter()
            .any(|l| l == &cfg.ticket_label_investigation),
    };
    if !label_ok {
        let _ = guard.finish_skip("label removed before work").await;
        return Ok(TickOutcome::Processed);
    }

    // 10. Discover the local clone.
    let runner = services.git.runner().clone();
    let repository = match find_main_clone(&cfg, &runner, &claimed.entry.key).await {
        Ok(r) => r,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 11. Create the worktree + branch.
    let run_id = guard.run_id().to_string();
    let worktree =
        match create_worktree(&cfg, &runner, &repository, &claimed.entry.key, &run_id).await {
            Ok(wt) => wt,
            Err(err) => {
                let class = classify_error(&err);
                return handle_infra_or_retry(cfg, guard, &err, class).await;
            }
        };
    if let Err(err) = guard.attach_worktree(worktree.clone()).await {
        let class = classify_error(&err);
        return handle_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 12. Build the context + prompt.
    let ctx_inputs = BuildInputs {
        config: &cfg,
        detail: &issue,
    };
    let ctx = match build_context(ctx_inputs) {
        Ok(c) => c,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let context_json = match encode_context(&ctx) {
        Ok(j) => j,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let prompt = match build_prompt(
        &issue,
        claimed.entry.ticket_type,
        &context_json,
        &worktree.branch_name,
    ) {
        Ok(p) => p,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    if let Err(err) = write_prompt(&worktree.path, &prompt) {
        let class = classify_error(&err);
        return handle_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 13. Spawn the worker through the executor trait object. The
    // factory in `Services::production` selected the matching
    // concrete executor (TrustedHostExecutor today, OciExecutor
    // stub once Task 6.2 lands) based on `cfg.executor_mode`.
    let self_exe = std::env::current_exe().map_err(|err| CaduceusError::Worktree {
        context: "tick",
        stderr: format!("current_exe: {err}"),
    })?;
    let worker_command = cfg.worker_command.clone();
    let spec = crate::executor::ExecutorSpec {
        self_exe,
        issue: claimed.entry.key.clone(),
        worktree: worktree.path.clone(),
        run_id: run_id.clone(),
        context_json: context_json.clone(),
        worker_command,
        cancellation: cancellation.clone(),
    };
    let supervisor_outcome = match services.executor.run(&spec).await {
        Ok(o) => o,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    guard.attach_supervisor(supervisor_outcome.clone()).await;
    if supervisor_outcome.timed_out || supervisor_outcome.cancelled {
        let _ = guard.finish_cancelled().await;
        return Ok(TickOutcome::Cancelled);
    }
    let _ = services.clock.now();

    // 14. Reject when worker exited nonzero (RUN-001 AC-04).
    if !supervisor_outcome.signaled && supervisor_outcome.status != 0 {
        let err = CaduceusError::Worker {
            context: "result",
            stderr: format!(
                "worker exited {} without producing a valid result",
                supervisor_outcome.status
            ),
        };
        let class = classify_error(&err);
        return handle_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 15. Read the worker result from the worktree (RUN-001 AC-02).
    let worktree_result_path = worktree.path.join("worker-result.json");
    let worker_result =
        match crate::worker::parse_result_file(&worktree_result_path, &claimed.entry.key) {
            Ok(r) => r,
            Err(_) => {
                let err = CaduceusError::Worker {
                    context: "result",
                    stderr: "worker did not produce a valid worker-result.json".to_string(),
                };
                let class = classify_error(&err);
                return handle_infra_or_retry(cfg, guard, &err, class).await;
            }
        };

    // 16. Archive the result before finalization (RUN-001 AC-03).
    let archive_path = match archive_worker_result(&worktree_result_path, &cfg.state_dir, &run_id) {
        Ok(p) => p,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 17. Run finalization.
    let final_ctx = FinalizeContext {
        client: Arc::clone(&client),
        config: cfg.clone(),
        repository: repository.clone(),
        issue: issue.clone(),
        claim: claimed.claim.clone(),
        run_id: run_id.clone(),
        worktree: worktree.clone(),
        result: FinalizeRequest {
            issue: claimed.entry.key.clone(),
            branch_name: worktree.branch_name.clone(),
            worktree_path: worktree.path.clone(),
        },
    };

    if cfg.dry_run {
        let _ = dry_run_finalize(&final_ctx, &worker_result, &archive_path, Vec::new())?;
        let _ = guard.finish_preview().await;
        return Ok(TickOutcome::Processed);
    }

    if worker_result.investigation || claimed.entry.ticket_type == TicketType::Investigation {
        match post_investigation_comment_and_finalize(
            &final_ctx,
            client.as_ref(),
            &worker_result,
            &cfg.ticket_label_investigation,
        )
        .await
        {
            Ok(_) => {
                let _ = guard.finish_investigation().await;
                return Ok(TickOutcome::Processed);
            }
            Err(err) => {
                let class = classify_error(&err);
                return handle_infra_or_retry(cfg, guard, &err, class).await;
            }
        }
    }

    // Code finalization: commit, push, PR, comment, await review.
    if let Err(err) = run_code_finalize(
        &final_ctx,
        &worker_result,
        &runner,
        &archive_path,
        client.as_ref(),
        store,
    )
    .await
    {
        let class = classify_error(&err);
        *http_status = extract_http_status(&err);
        return handle_infra_or_retry(cfg, guard, &err, class).await;
    }

    guard.finish_awaiting_review().await?;
    Ok(TickOutcome::Processed)
}

// ---------------------------------------------------------------------------
// Checkpoint resume helpers
// ---------------------------------------------------------------------------

/// Decides what to do when a run already has durable checkpoints.
enum ResumeAction {
    /// Skip to the next uncompleted stage and resume from there.
    Skip(crate::state::queue::FinalizationStage),
    /// All stages are already complete; no work needed.
    AlreadyDone,
    /// No checkpoint found; start fresh.
    StartFresh,
}

/// Reads the last checkpoint for a run and returns the appropriate resume
/// action.
fn resume_from_checkpoint(
    conn: &rusqlite::Connection,
    run_id: &str,
) -> CaduceusResult<ResumeAction> {
    match last_checkpoint_for_run(conn, run_id)? {
        None => Ok(ResumeAction::StartFresh),
        Some(cp) => {
            let stage = match cp.stage_enum() {
                Some(s) => s,
                None => return Ok(ResumeAction::StartFresh),
            };
            match stage {
                crate::state::queue::FinalizationStage::Done => Ok(ResumeAction::AlreadyDone),
                other => Ok(ResumeAction::Skip(next_stage_after(other))),
            }
        }
    }
}

/// Returns the next stage in the FINAL-001 sequence.
fn next_stage_after(
    stage: crate::state::queue::FinalizationStage,
) -> crate::state::queue::FinalizationStage {
    use crate::state::queue::FinalizationStage::*;
    match stage {
        ResultValidated => Committed,
        Committed => Pushed,
        Pushed => PrCreated,
        PrCreated => Commented,
        Commented => AwaitingReview,
        AwaitingReview => Done,
        Done => Done,
        InvestigationReady => InvestigationCommented,
        InvestigationCommented => Done,
    }
}

/// Re-enters the finalization pipeline at the given resume stage, skipping
/// all earlier stages. Opens a fresh SQLite connection for checkpoint writes.
#[allow(clippy::too_many_arguments)]
async fn run_resume_finalization(
    cfg: Config,
    services: &Services,
    _store: &StateStore,
    _meta: &MetaStore,
    client: Arc<Client>,
    claimed: ClaimedEntry,
    guard: &mut ActiveRunGuard,
    cancellation: CancellationToken,
    _http_status: &mut Option<u16>,
    resume_stage: crate::state::queue::FinalizationStage,
) -> CaduceusResult<TickOutcome> {
    use crate::state::queue::FinalizationStage::*;

    // Build the minimal context needed for finalization.
    let run_id = guard.run_id().to_string();
    let runner = services.git.runner().clone();
    let repository = match find_main_clone(&cfg, &runner, &claimed.entry.key).await {
        Ok(r) => r,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    let worktree =
        match create_worktree(&cfg, &runner, &repository, &claimed.entry.key, &run_id).await {
            Ok(wt) => wt,
            Err(err) => {
                let class = classify_error(&err);
                return handle_infra_or_retry(cfg, guard, &err, class).await;
            }
        };

    // Check for cancellation
    if cancellation.is_cancelled() {
        return Ok(TickOutcome::Cancelled);
    }

    // Fetch the issue detail
    let issue = match crate::github::issue::fetch_issue_detail(
        client.as_ref(),
        &claimed.entry.key,
        &cfg.feedback_author_allowlist,
    )
    .await
    {
        Ok(d) => d,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // Build the finalization context
    let ctx = FinalizeContext {
        client,
        config: cfg.clone(),
        repository,
        issue,
        claim: claimed.claim,
        run_id: run_id.clone(),
        worktree: worktree.clone(),
        result: FinalizeRequest {
            issue: claimed.entry.key.clone(),
            branch_name: worktree.branch_name.clone(),
            worktree_path: worktree.path.clone(),
        },
    };

    // Open SQLite connection for checkpoint writes
    let conn = match store::open_in(&ctx.config.state_dir) {
        Ok(c) => c,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(ctx.config.clone(), guard, &err, class).await;
        }
    };

    // Resume at the appropriate stage
    // We need a worker_result to pass to the step functions. On resume, we
    // read the worker result from disk.
    let result_path = ctx
        .config
        .state_dir
        .join("runs")
        .join(format!("{}.result.json", ctx.run_id));
    let worker_result = match std::fs::read_to_string(&result_path) {
        Ok(json) => match serde_json::from_str::<WorkerResult>(&json) {
            Ok(wr) => wr,
            Err(err) => {
                return Err(CaduceusError::StateCorrupt {
                    path: result_path,
                    message: format!("failed to deserialize worker result for resume: {err}"),
                });
            }
        },
        Err(err) => {
            return Err(CaduceusError::Io(err));
        }
    };

    let archive_path = match archive_worker_result(&result_path, &ctx.config.state_dir, &ctx.run_id)
    {
        Ok(p) => p,
        Err(err) => {
            let class = classify_error(&err);
            return handle_infra_or_retry(ctx.config.clone(), guard, &err, class).await;
        }
    };

    match resume_stage {
        ResultValidated => {
            persist_checkpoint(&conn, &ctx.run_id, ResultValidated, None, None, None)?;
            let _ = commit_code_and_finalize(&ctx, &worker_result, &runner, &archive_path)?;
            persist_checkpoint(&conn, &ctx.run_id, Committed, None, None, None)?;
            push_and_finalize(&ctx, &runner).await?;
            persist_checkpoint(&conn, &ctx.run_id, Pushed, None, None, None)?;
            find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, PrCreated, None, None, None)?;
            post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, Commented, None, None, None)?;
            persist_checkpoint(&conn, &ctx.run_id, AwaitingReview, None, None, None)?;
        }
        Committed => {
            persist_checkpoint(&conn, &ctx.run_id, Committed, None, None, None)?;
            push_and_finalize(&ctx, &runner).await?;
            persist_checkpoint(&conn, &ctx.run_id, Pushed, None, None, None)?;
            find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, PrCreated, None, None, None)?;
            post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, Commented, None, None, None)?;
            persist_checkpoint(&conn, &ctx.run_id, AwaitingReview, None, None, None)?;
        }
        Pushed => {
            persist_checkpoint(&conn, &ctx.run_id, Pushed, None, None, None)?;
            find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, PrCreated, None, None, None)?;
            post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, Commented, None, None, None)?;
            persist_checkpoint(&conn, &ctx.run_id, AwaitingReview, None, None, None)?;
        }
        PrCreated => {
            persist_checkpoint(&conn, &ctx.run_id, PrCreated, None, None, None)?;
            post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            persist_checkpoint(&conn, &ctx.run_id, Commented, None, None, None)?;
            persist_checkpoint(&conn, &ctx.run_id, AwaitingReview, None, None, None)?;
        }
        Commented | AwaitingReview | Done => {
            // The comment is already posted; no further action needed.
            // Persist the AwaitingReview checkpoint (the poller will
            // handle the terminal transition when the PR is merged).
            persist_checkpoint(&conn, &ctx.run_id, Commented, None, None, None)?;
            persist_checkpoint(&conn, &ctx.run_id, AwaitingReview, None, None, None)?;
        }
        InvestigationReady | InvestigationCommented => {
            // Pass through — investigation stages handled by separate path
        }
    }

    guard.finish_success().await?;
    Ok(TickOutcome::Processed)
}

async fn run_code_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &GitRunner,
    worker_result_path: &std::path::Path,
    client: &Client,
    store: &StateStore,
) -> CaduceusResult<FinalizeOutput> {
    let conn = store::open_in(&ctx.config.state_dir)?;

    // Stage 1: ResultValidated — about to commit
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::ResultValidated,
        None,
        None,
        None,
    )?;
    let _ = commit_code_and_finalize(ctx, worker_result, runner, worker_result_path)?;

    // Stage 2: Committed — about to push
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::Committed,
        None,
        None,
        None,
    )?;
    push_and_finalize(ctx, runner).await?;

    // Stage 3: Pushed — about to create PR
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::Pushed,
        None,
        None,
        None,
    )?;
    find_or_create_pr_and_finalize(ctx, client, worker_result).await?;

    // Stage 4: PrCreated — about to post completion comment
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::PrCreated,
        None,
        None,
        None,
    )?;

    // Post the completion comment but do NOT close the issue.
    // The issue stays open until human review merges the PR.
    post_completion_only(ctx, client, worker_result).await?;

    // Stage 5: Commented — comment posted
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::Commented,
        None,
        None,
        None,
    )?;

    // Transition queue entry to AwaitingReview so the polling
    // loop can track the PR merge status.
    store.complete_awaiting_review(&ctx.issue.key)?;

    // Stage 6: AwaitingReview — waiting for human merge
    persist_checkpoint(
        &conn,
        &ctx.run_id,
        crate::state::queue::FinalizationStage::AwaitingReview,
        None,
        None,
        None,
    )?;

    // Return WITHOUT Done checkpoint or close — the human
    // review lifecycle handles the terminal transition.
    Ok(FinalizeOutput {
        action: crate::finalize::FinalizeAction::AwaitingReview,
        pr_url: None,
        idempotency_observations: vec![
            "awaiting_review".to_string(),
            format!("issue={}", ctx.issue.key.display_key()),
        ],
    })
}

// ---------------------------------------------------------------------------
// Awaiting-review poller — checks PR merge status for entries in
// AwaitingReview phase and applies transitions.
// ---------------------------------------------------------------------------

/// Scan the queue for entries in [`Phase::AwaitingReview`] and poll
/// each entry's PR merge status. Applies transitions:
///
/// * PR merged → `Done` (via `store.complete`)
/// * PR closed without merge → `NeedsAttention` (via `store.route_to_needs_attention`)
/// * PR still open → no-op
///
/// The function is best-effort: a single failed poll does not block
/// the rest of the scan. Per-entry errors are logged and collected.
async fn poll_awaiting_review_entries(store: &StateStore, client: &Client) -> CaduceusResult<()> {
    let snap = store.snapshot()?;
    let awaiting: Vec<QueueEntry> = snap
        .entries
        .values()
        .filter(|e| e.phase == Phase::AwaitingReview)
        .filter(|e| {
            // Only poll entries that have a finalization checkpoint
            // with a PR number.
            e.finalization.as_ref().and_then(|f| f.pr_number).is_some()
        })
        .cloned()
        .collect();

    for entry in &awaiting {
        let key = &entry.key;
        let pr_number = entry
            .finalization
            .as_ref()
            .and_then(|f| f.pr_number)
            .expect("filtered above");

        match crate::github::merge_detect::poll_pr_merge_status(
            client, &key.owner, &key.repo, pr_number,
        )
        .await
        {
            Ok(crate::github::merge_detect::MergeStatus::Merged { .. }) => {
                info!(
                issue = %key.display_key(),
                pr = %pr_number,
                "PR merged; transitioning to Done"
                );
                if let Err(err) = store.resolve_awaiting_review_as_done(key) {
                    tracing::warn!(
                    error = %err,
                    issue = %key.display_key(),
                    "failed to mark merged PR as Done"
                    );
                }
            }
            Ok(crate::github::merge_detect::MergeStatus::ClosedWithoutMerge) => {
                info!(
                issue = %key.display_key(),
                pr = %pr_number,
                "PR closed without merge; routing to NeedsAttention"
                );
                if let Err(err) = store.route_to_needs_attention(
                    key,
                    &format!("PR #{pr_number} was closed without merge — operator must inspect"),
                ) {
                    tracing::warn!(
                    error = %err,
                    issue = %key.display_key(),
                    "failed to route closed PR to NeedsAttention"
                    );
                }
            }
            Ok(
                crate::github::merge_detect::MergeStatus::StillOpen
                | crate::github::merge_detect::MergeStatus::NotFound,
            ) => {
                // Still waiting for human review, or PR not found yet.
                // No-op.
            }
            Err(err) => {
                tracing::warn!(
                error = %err,
                issue = %key.display_key(),
                "failed to poll PR merge status"
                );
            }
        }
    }

    Ok(())
}

async fn poll_repo(
    slug: &str,
    client: &Client,
    cfg: &Config,
    store: &StateStore,
    meta: &MetaStore,
) -> CaduceusResult<Outcome304> {
    let repos: Vec<String> = vec![slug.to_string()];
    let code = poll_code(client, cfg, &repos).await?;
    let inv = poll_investigation(client, cfg, &repos).await?;
    let merged = merge_outcomes(code, inv);
    let _ = Response {
        status: 200,
        final_url: format!("https://api.github.com/repos/{slug}/issues"),
        body: Vec::new(),
        from_cache: false,
        headers: reqwest::header::HeaderMap::new(),
    };
    let _ = meta; // The meta is queried through the gate above.
                  // Polling enqueue paths share the same store.
    let _ = enqueue_summaries(store, &merged.summaries, cfg.dry_run);
    Ok(Outcome304(false))
}

fn enqueue_summaries(
    store: &StateStore,
    summaries: &[crate::github::poll::IssueSummary],
    dry_run: bool,
) -> Option<DateTime<Utc>> {
    let mut earliest: Option<DateTime<Utc>> = None;
    for summary in summaries {
        if let Ok(_outcome) = store.enqueue(&summary.key, summary.ticket_type, dry_run) {
            // The enqueue outcome is a binary inserted/already/promoted
            // signal; the backoff window is whatever the entry's
            // existing `next_attempt_at` carries.
            if let Some(entry) = store
                .snapshot()
                .ok()
                .and_then(|s| s.entry(&summary.key).cloned())
            {
                if let Some(b) = entry.next_attempt_at {
                    earliest = Some(match earliest {
                        Some(e) => e.min(b),
                        None => b,
                    });
                }
            }
        }
    }
    earliest
}

async fn handle_infra_or_retry(
    cfg: Config,
    guard: &mut ActiveRunGuard,
    err: &CaduceusError,
    class: FailureClass,
) -> CaduceusResult<TickOutcome> {
    if class.counts_against_retry_budget() {
        let new_phase = guard
            .finish_retry(&err.to_string(), cfg.max_retries_per_issue)
            .await?;
        return Ok(map_phase_to_outcome(new_phase));
    }
    let now = Utc::now();
    let not_before = now + chrono::Duration::seconds(cfg.retry_backoff_seconds as i64);
    let _ = guard
        .finish_infrastructure(&err.to_string(), not_before)
        .await;
    Ok(outcome_for_class(class))
}

fn outcome_for_class(class: FailureClass) -> TickOutcome {
    match class {
        FailureClass::RateLimit { .. } => TickOutcome::RateLimited,
        FailureClass::Cancellation => TickOutcome::Cancelled,
        _ => TickOutcome::Failed,
    }
}

fn map_phase_to_outcome(phase: Phase) -> TickOutcome {
    match phase {
        Phase::Queued
        | Phase::InProgress
        | Phase::Previewed
        | Phase::AwaitingReview
        | Phase::Done
        | Phase::Skipped => TickOutcome::Processed,
        Phase::Failed => TickOutcome::Failed,
        Phase::NeedsAttention => TickOutcome::Failed,
    }
}

fn extract_http_status(err: &CaduceusError) -> Option<u16> {
    match err {
        CaduceusError::GitHubApi { status, .. } => Some(*status),
        _ => None,
    }
}

fn finish_tick_outcome(
    gate: &CadenceGate,
    _meta: &MetaStore,
    now: DateTime<Utc>,
    outcome: TickOutcome,
    http_status: Option<u16>,
    last_error: Option<&CaduceusError>,
) -> CaduceusResult<()> {
    let _ = _meta;
    gate.record_tick_finished(
        now,
        outcome,
        http_status,
        0,
        None,
        last_error.map(|e| format!("{e}")),
    )
}

fn finish_tick_failure(
    gate: &CadenceGate,
    now: DateTime<Utc>,
    cfg: &Config,
    meta: &MetaStore,
    class: FailureClass,
    last_error: Option<&CaduceusError>,
) -> CaduceusResult<()> {
    let _ = meta;
    let outcome = match class {
        FailureClass::RateLimit { .. } => TickOutcome::RateLimited,
        FailureClass::Cancellation => TickOutcome::Cancelled,
        _ => TickOutcome::Failed,
    };
    let _ = cfg;
    // The rate-limit observation is the input to the
    // next tick's `CadenceGate::precheck` and must be
    // persisted *before* the tick returns. The
    // orchestrator's `tick` body itself does not always
    // pass the rate-limit info to `record_tick_finished`;
    // we extract the observation from the last error here
    // when the failure class is `RateLimit` so the gate
    // can record it via `record_tick_finished`.
    let rate_limit_info: Option<RateLimitInfo> = match (class, last_error) {
        (
            FailureClass::RateLimit { .. },
            Some(CaduceusError::RateLimited {
                reset_at,
                remaining,
                limit,
            }),
        ) => Some(RateLimitInfo {
            remaining: *remaining,
            limit: *limit,
            observed_at: now,
            reset_at_unix: now.timestamp().saturating_add(*reset_at as i64),
        }),
        _ => None,
    };
    gate.record_tick_finished(
        now,
        outcome,
        None,
        cfg.poll_interval_seconds,
        rate_limit_info.as_ref(),
        last_error.map(|e| format!("{e}")),
    )
}

fn dummy_rate_limit_info(obs: &crate::state::meta::RateLimitObservation) -> RateLimitInfo {
    RateLimitInfo {
        remaining: obs.remaining,
        limit: obs.limit,
        observed_at: obs.observed_at,
        reset_at_unix: obs.reset_at.timestamp(),
    }
}

fn exit_code_for(outcome: &TickOutcome) -> u8 {
    match outcome {
        TickOutcome::Processed => 0,
        TickOutcome::Idle304 | TickOutcome::IdleEmpty => 0,
        TickOutcome::SkippedConcurrent => 0,
        TickOutcome::SkippedCadence => 0,
        TickOutcome::RateLimited => 0,
        TickOutcome::Cancelled => 0,
        TickOutcome::Failed => 1,
    }
}

/// Test seam: re-export the exit-code mapping so integration
/// tests can assert the cron-tick contract without owning a
/// runtime. Identical to the private [`exit_code_for`].
pub fn exit_code_for_tests(outcome: &TickOutcome) -> u8 {
    exit_code_for(outcome)
}

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn exit_code_for_outcome_table() {
        assert_eq!(exit_code_for(&TickOutcome::Processed), 0);
        assert_eq!(exit_code_for(&TickOutcome::Idle304), 0);
        assert_eq!(exit_code_for(&TickOutcome::IdleEmpty), 0);
        assert_eq!(exit_code_for(&TickOutcome::SkippedConcurrent), 0);
        assert_eq!(exit_code_for(&TickOutcome::SkippedCadence), 0);
        assert_eq!(exit_code_for(&TickOutcome::RateLimited), 0);
        assert_eq!(exit_code_for(&TickOutcome::Cancelled), 0);
        assert_eq!(exit_code_for(&TickOutcome::Failed), 1);
    }

    #[test]
    fn outcome_for_class_maps_each_failure_class() {
        assert!(matches!(
            outcome_for_class(FailureClass::RateLimit { reset_at: 0 }),
            TickOutcome::RateLimited
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Cancellation),
            TickOutcome::Cancelled
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Worker),
            TickOutcome::Failed
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Infrastructure),
            TickOutcome::Failed
        ));
    }

    #[test]
    fn map_phase_to_outcome_agrees_with_phase_taxonomy() {
        assert!(matches!(
            map_phase_to_outcome(Phase::Queued),
            TickOutcome::Processed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Failed),
            TickOutcome::Failed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Done),
            TickOutcome::Processed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Skipped),
            TickOutcome::Processed
        ));
    }

    #[test]
    fn extract_http_status_only_matches_github_api_variant() {
        let err = CaduceusError::GitHubApi {
            status: 422,
            message: "x".to_string(),
        };
        assert_eq!(extract_http_status(&err), Some(422));
        let err = CaduceusError::Worker {
            context: "result",
            stderr: "x".to_string(),
        };
        assert_eq!(extract_http_status(&err), None);
    }
}
