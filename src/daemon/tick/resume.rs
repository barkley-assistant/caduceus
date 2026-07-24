#![allow(dead_code, unused_imports)]
use super::*;
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
// Checkpoint resume helpers
// ---------------------------------------------------------------------------

/// Decides what to do when a run already has durable checkpoints.
pub(crate) enum ResumeAction {
    /// Skip to the next uncompleted stage and resume from there.
    Skip(crate::state::queue::FinalizationStage),
    /// All stages are already complete; no work needed.
    AlreadyDone,
    /// No checkpoint found; start fresh.
    StartFresh,
}

/// Reads the last checkpoint for a run and returns the appropriate resume
/// action.
pub(crate) fn resume_from_checkpoint(
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
pub(crate) fn next_stage_after(
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
pub(crate) async fn run_resume_finalization(
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

pub(crate) async fn run_code_finalize(
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
