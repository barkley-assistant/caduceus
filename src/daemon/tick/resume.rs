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
    find_or_create_pr_and_finalize, generate_operation_id, post_completion_only,
    post_investigation_comment_and_finalize, push_and_finalize, FinalizeContext, FinalizeOutput,
    FinalizeRequest,
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
use crate::state::queue::{
    ClaimedEntry, FinalizationStage, Phase, QueueEntry, StateStore, TicketType,
};
use crate::state::store;
use crate::worker::context::{build_context, encode_context, BuildInputs};
use crate::worker::prompt::{build_prompt, write_prompt};
use crate::worker::{WorkerResult, WorkerStatus};
use crate::worktree::{create as create_worktree, find_main_clone, GitRunner};

// Checkpoint resume helpers

/// Decides what to do when a run already has durable checkpoints.
#[derive(Debug)]
pub enum ResumeAction {
    /// Skip to the next uncompleted stage and resume from there.
    Skip(FinalizationStage),
    /// All stages are already complete; no work needed.
    AlreadyDone,
    /// No checkpoint found; start fresh.
    StartFresh,
}

/// Reads the last checkpoint for a run and returns the appropriate resume
/// action.
pub fn resume_from_checkpoint(
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
                FinalizationStage::Done => Ok(ResumeAction::AlreadyDone),
                other => Ok(ResumeAction::Skip(next_stage_after(other))),
            }
        }
    }
}

/// Returns the next stage in the finalization sequence.
pub(crate) fn next_stage_after(stage: FinalizationStage) -> FinalizationStage {
    use FinalizationStage::*;
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

/// Persist a checkpoint with a deterministic operation_id. The marker is
/// the durable remote effect produced by the stage; it must be `None` when
/// the stage has no external effect to record.
fn checkpoint(
    conn: &rusqlite::Connection,
    run_id: &str,
    stage: FinalizationStage,
    marker: Option<&str>,
) -> CaduceusResult<()> {
    persist_checkpoint(
        conn,
        run_id,
        stage,
        None,
        Some(&generate_operation_id(run_id, stage.as_str())),
        marker,
    )
}

/// Re-enters the finalization pipeline at the given resume stage, skipping
/// all earlier stages. Opens a fresh SQLite connection for checkpoint writes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_resume_finalization(
    cfg: Config,
    services: &Services,
    store: &StateStore,
    _meta: &MetaStore,
    client: Arc<Client>,
    claimed: ClaimedEntry,
    guard: &mut ActiveRunGuard,
    cancellation: CancellationToken,
    _http_status: &mut Option<u16>,
    resume_stage: crate::state::queue::FinalizationStage,
) -> CaduceusResult<TickOutcome> {
    use crate::state::queue::FinalizationStage::*;

    let resume_checkpoint =
        claimed
            .entry
            .finalization
            .clone()
            .ok_or_else(|| CaduceusError::StateCorrupt {
                path: cfg.state_dir.join("state.json"),
                message: "resume requested without a finalization checkpoint".to_string(),
            })?;

    // Build the minimal context needed for finalization.
    let run_id = resume_checkpoint.run_id.clone();
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
    if worktree.branch_name != resume_checkpoint.branch_name {
        if let Err(err) = crate::worktree::remove(&worktree).await {
            tracing::warn!(
                error = %err,
                worktree = %worktree.path.display(),
                "failed to clean up mismatched resume worktree"
            );
        }
        return Err(CaduceusError::StateCorrupt {
            path: cfg.state_dir.join("state.json"),
            message: format!(
                "resume checkpoint branch {:?} does not match reconstructed branch {:?}",
                resume_checkpoint.branch_name, worktree.branch_name
            ),
        });
    }

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
    // read the archived worker result through the canonical parser so the
    // same read-side invariants (O_NOFOLLOW, size cap, field validation)
    // and the `WorkerStatus::Failure` retry contract apply as on the
    // fresh path (issue #118). Bypassing them let a malformed or
    // failure-status archived result silently finalize.
    let result_path = resume_checkpoint.result_path.clone();
    let worker_result = match crate::worker::parse_result_file(&result_path, &ctx.issue.key) {
        Ok(wr) => wr,
        Err(err) => {
            return Err(CaduceusError::Worker {
                context: "resume",
                stderr: format!("{}: {err}", result_path.display()),
            });
        }
    };
    if worker_result.status == WorkerStatus::Failure {
        return Err(CaduceusError::Worker {
            context: "resume",
            stderr: "archived worker result declared failure; refusing to resume finalization"
                .to_string(),
        });
    }

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
            // ResultValidated has no external effect to record yet.
            checkpoint(&conn, &ctx.run_id, ResultValidated, None)?;

            let commit_out =
                commit_code_and_finalize(&ctx, &worker_result, &runner, &archive_path)?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Committed,
                commit_out.commit_oid.as_deref(),
            )?;

            let push_out = push_and_finalize(&ctx, &runner).await?;
            checkpoint(&conn, &ctx.run_id, Pushed, push_out.pushed_oid.as_deref())?;

            let pr_output =
                find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::PrCreated,
                    commit_oid: commit_out.commit_oid.clone(),
                    pr_number: pr_output.pr_number,
                    pr_url: pr_output.pr_url,
                },
            )?;
            checkpoint(
                &conn,
                &ctx.run_id,
                PrCreated,
                pr_output.pr_number.map(|n| n.to_string()).as_deref(),
            )?;

            let comment_out =
                post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Commented,
                comment_out.comment_id.map(|n| n.to_string()).as_deref(),
            )?;

            // AwaitingReview is a stage advance, no new external effect.
            checkpoint(&conn, &ctx.run_id, AwaitingReview, None)?;
        }
        Committed => {
            // Re-run the commit effect (idempotent), then record the checkpoint.
            let commit_out =
                commit_code_and_finalize(&ctx, &worker_result, &runner, &archive_path)?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Committed,
                commit_out.commit_oid.as_deref(),
            )?;

            let push_out = push_and_finalize(&ctx, &runner).await?;
            checkpoint(&conn, &ctx.run_id, Pushed, push_out.pushed_oid.as_deref())?;

            let pr_output =
                find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::PrCreated,
                    commit_oid: commit_out.commit_oid.clone(),
                    pr_number: pr_output.pr_number,
                    pr_url: pr_output.pr_url,
                },
            )?;
            checkpoint(
                &conn,
                &ctx.run_id,
                PrCreated,
                pr_output.pr_number.map(|n| n.to_string()).as_deref(),
            )?;

            let comment_out =
                post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Commented,
                comment_out.comment_id.map(|n| n.to_string()).as_deref(),
            )?;

            checkpoint(&conn, &ctx.run_id, AwaitingReview, None)?;
        }
        Pushed => {
            // Re-run the push effect (idempotent), then record the checkpoint.
            let push_out = push_and_finalize(&ctx, &runner).await?;
            checkpoint(&conn, &ctx.run_id, Pushed, push_out.pushed_oid.as_deref())?;

            let pr_output =
                find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::PrCreated,
                    commit_oid: None,
                    pr_number: pr_output.pr_number,
                    pr_url: pr_output.pr_url,
                },
            )?;
            checkpoint(
                &conn,
                &ctx.run_id,
                PrCreated,
                pr_output.pr_number.map(|n| n.to_string()).as_deref(),
            )?;

            let comment_out =
                post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Commented,
                comment_out.comment_id.map(|n| n.to_string()).as_deref(),
            )?;

            checkpoint(&conn, &ctx.run_id, AwaitingReview, None)?;
        }
        PrCreated => {
            // Re-run PR create-or-reuse (idempotent), then record the checkpoint.
            let pr_output =
                find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &worker_result).await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::PrCreated,
                    commit_oid: None,
                    pr_number: pr_output.pr_number,
                    pr_url: pr_output.pr_url,
                },
            )?;
            checkpoint(
                &conn,
                &ctx.run_id,
                PrCreated,
                pr_output.pr_number.map(|n| n.to_string()).as_deref(),
            )?;

            let comment_out =
                post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Commented,
                comment_out.comment_id.map(|n| n.to_string()).as_deref(),
            )?;

            checkpoint(&conn, &ctx.run_id, AwaitingReview, None)?;
        }
        Commented | AwaitingReview | Done => {
            // Re-run the comment post (idempotent marker check), then
            // persist the Commented and AwaitingReview checkpoints.
            // Debug-only invariant: the per_claim `InvestigationCommented`
            // short-circuit is the actual production guard against
            // investigation entries reaching this code-path arm.
            debug_assert!(
                claimed.entry.ticket_type != TicketType::Investigation,
                "investigation entries must resume via the InvestigationCommented arm"
            );
            let comment_out =
                post_completion_only(&ctx, ctx.client.as_ref(), &worker_result).await?;
            checkpoint(
                &conn,
                &ctx.run_id,
                Commented,
                comment_out.comment_id.map(|n| n.to_string()).as_deref(),
            )?;
            checkpoint(&conn, &ctx.run_id, AwaitingReview, None)?;
        }
        InvestigationCommented => {
            // Resume target after an InvestigationReady crash: the
            // findings comment was never durably recorded on the queue
            // entry, so re-post it (idempotent — the marker carries the
            // original run_id, so the existing-comment check suppresses
            // a duplicate POST), persist the InvestigationCommented
            // checkpoint, then finish the entry.
            post_investigation_comment_and_finalize(
                &ctx,
                ctx.client.as_ref(),
                &worker_result,
                &ctx.config.ticket_label_investigation,
            )
            .await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::InvestigationCommented,
                    commit_oid: None,
                    pr_number: None,
                    pr_url: None,
                },
            )?;
            checkpoint(&conn, &ctx.run_id, InvestigationCommented, None)?;
            guard.finish_investigation().await?;
            return Ok(TickOutcome::Processed);
        }
        InvestigationReady => {
            // Defensive: nothing maps to InvestigationReady as a resume
            // target (`next_stage_after` has no predecessor producing
            // it), but keep the same body as InvestigationCommented so
            // a future stage-graph change cannot silently drop the
            // findings comment.
            post_investigation_comment_and_finalize(
                &ctx,
                ctx.client.as_ref(),
                &worker_result,
                &ctx.config.ticket_label_investigation,
            )
            .await?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::InvestigationCommented,
                    commit_oid: None,
                    pr_number: None,
                    pr_url: None,
                },
            )?;
            checkpoint(&conn, &ctx.run_id, InvestigationCommented, None)?;
            guard.finish_investigation().await?;
            return Ok(TickOutcome::Processed);
        }
    }

    // Code-ticket resume arms (ResultValidated..AwaitingReview, plus
    // the no-op Done arm) must leave the entry in AwaitingReview —
    // the merge poller owns the AwaitingReview -> Done transition.
    // Resume must not call finish_success() here, which would mark an
    // unmerged PR Done (issue #118 audit).
    guard.finish_awaiting_review().await?;
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

    // Stage 1: ResultValidated — no external effect to record yet.
    checkpoint(&conn, &ctx.run_id, FinalizationStage::ResultValidated, None)?;

    // Stage 2: Commit the validated changes, then record the commit OID.
    let commit_out = commit_code_and_finalize(ctx, worker_result, runner, worker_result_path)?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::Committed,
        commit_out.commit_oid.as_deref(),
    )?;

    // Stage 3: Push the daemon branch, then record the remote OID.
    let push_out = push_and_finalize(ctx, runner).await?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::Pushed,
        push_out.pushed_oid.as_deref(),
    )?;

    // Stage 4: Create or reuse the PR, then record the PR number.
    let pr_output = find_or_create_pr_and_finalize(ctx, client, worker_result).await?;
    // Persist the durable finalization checkpoint so the awaiting-review
    // poller can satisfy its `finalization.pr_number.is_some()` filter.
    // The PR number is the only durable link from queue entry → PR.
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: worker_result_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::PrCreated,
            commit_oid: commit_out.commit_oid.clone(),
            pr_number: pr_output.pr_number,
            pr_url: pr_output.pr_url,
        },
    )?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::PrCreated,
        pr_output.pr_number.map(|n| n.to_string()).as_deref(),
    )?;

    // Stage 5: Post the completion comment but do NOT close the issue.
    // The issue stays open until human review merges the PR.
    let comment_out = post_completion_only(ctx, client, worker_result).await?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::Commented,
        comment_out.comment_id.map(|n| n.to_string()).as_deref(),
    )?;

    // Transition queue entry to AwaitingReview so the polling
    // loop can track the PR merge status.
    store.complete_awaiting_review(&ctx.issue.key)?;

    // Stage 6: AwaitingReview — waiting for human merge; no new external effect.
    checkpoint(&conn, &ctx.run_id, FinalizationStage::AwaitingReview, None)?;

    // Return WITHOUT Done checkpoint or close — the human
    // review lifecycle handles the terminal transition.
    Ok(FinalizeOutput {
        action: crate::finalize::FinalizeAction::AwaitingReview,
        pr_url: None,
        pr_number: None,
        commit_oid: None,
        pushed_oid: None,
        comment_id: None,
        idempotency_observations: vec![
            "awaiting_review".to_string(),
            format!("issue={}", ctx.issue.key.display_key()),
        ],
    })
}

/// Runs investigation finalization with the same durable-checkpoint
/// pattern as [`run_code_finalize`], minus the git pipeline:
/// investigation tickets commit/push nothing.
///
/// The `InvestigationReady` checkpoint is persisted (SQLite then queue)
/// *before* the findings comment POST, and `InvestigationCommented`
/// *after* it succeeds — so a crash after the comment leaves a durable
/// record and recovery never re-dispatches the worker or duplicates the
/// comment (the resume path re-posts idempotently under the original
/// `run_id` marker instead).
pub(crate) async fn run_investigation_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    archive_path: &std::path::Path,
    client: &Client,
    store: &StateStore,
    investigation_label: &str,
) -> CaduceusResult<FinalizeOutput> {
    let conn = store::open_in(&ctx.config.state_dir)?;

    // Stage 1: InvestigationReady — no external effect yet, but the
    // queue entry must durably record that the investigation run began
    // finalization so recovery does not re-dispatch the worker.
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::InvestigationReady,
        None,
    )?;
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: archive_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::InvestigationReady,
            commit_oid: None,
            pr_number: None,
            pr_url: None,
        },
    )?;

    // Stage 2: Post the findings comment (idempotent marker check).
    let output =
        post_investigation_comment_and_finalize(ctx, client, worker_result, investigation_label)
            .await?;

    // Stage 3: InvestigationCommented — the comment is now durable
    // remote state; record it so recovery finishes the entry without
    // re-posting.
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::InvestigationCommented,
        None,
    )?;
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: archive_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::InvestigationCommented,
            commit_oid: None,
            pr_number: None,
            pr_url: None,
        },
    )?;

    Ok(output)
}
