use super::handle_infra_or_retry;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::daemon::orchestration::{classify_error, ActiveRunGuard, Services};
use crate::finalize::{
    archive_worker_result, commit_code_and_finalize, find_or_create_pr_and_finalize,
    generate_operation_id, post_completion_only, push_and_finalize, FinalizeContext,
    FinalizeOutput, FinalizeRequest,
};
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::checkpoints::{last_checkpoint_for_run, persist_checkpoint};
use crate::state::meta::{MetaStore, TickOutcome};
use crate::state::queue::{ClaimedEntry, FinalizationStage, StateStore};
use crate::state::store;
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
            // ResultValidated has no external effect to record yet, but
            // the queue entry must durably record the resumed stage so a
            // crash during recovery itself still resumes under the
            // original run_id.
            checkpoint(&conn, &ctx.run_id, ResultValidated, None)?;
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::ResultValidated,
                    commit_oid: None,
                    pr_number: None,
                    pr_url: None,
                },
            )?;

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
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::Committed,
                    commit_oid: commit_out.commit_oid.clone(),
                    pr_number: None,
                    pr_url: None,
                },
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
            // The Pushed resume arm has no commit_out in scope (it starts
            // at push), so commit_oid is None, consistent with its
            // PrCreated save below. commit_oid is not consumed by
            // recovery routing.
            store.save_resumed_finalization(
                &ctx.claim,
                crate::state::queue::FinalizationCheckpoint {
                    run_id: ctx.run_id.clone(),
                    branch_name: ctx.worktree.branch_name.clone(),
                    result_path: result_path.clone(),
                    stage: crate::state::queue::FinalizationStage::Pushed,
                    commit_oid: None,
                    pr_number: None,
                    pr_url: None,
                },
            )?;

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
        // InvestigationCommented / InvestigationReady resume arms were
        // removed in N+1 (issue #331): the reconcile pass terminates
        // every non-terminal Investigation row at store open, so no
        // entry can arrive here carrying an investigation stage. The
        // variants are RETAINED on `FinalizationStage` (parse compat
        // for surviving rows), so this arm keeps the match exhaustive
        // and fails loudly if a stale checkpoint ever slips through.
        InvestigationReady | InvestigationCommented => {
            return Err(CaduceusError::Queue {
                context: "resume",
                stderr: format!(
                    "investigation finalization was removed in N+1 (#331); entry {} \
                     carries a legacy {:?} checkpoint",
                    ctx.issue.key.display_key(),
                    resume_stage
                ),
            });
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

/// Runs code finalization following the durable-checkpoint pattern.
/// (The investigation analogue was removed in N+1, issue #331.)
///
/// The `ResultValidated`, `Committed`, and `Pushed` checkpoints are
/// persisted (SQLite then queue) *before* the next external effect, and
/// `PrCreated` after PR creation succeeds — so a crash after a commit or
/// push leaves a durable record and recovery resumes at the recorded
/// stage under the original `run_id` instead of re-dispatching the
/// worker or creating a duplicate branch/commit.
pub(crate) async fn run_code_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &GitRunner,
    worker_result_path: &std::path::Path,
    client: &Client,
    store: &StateStore,
) -> CaduceusResult<FinalizeOutput> {
    let conn = store::open_in(&ctx.config.state_dir)?;

    // Stage 1: ResultValidated — no external effect to record yet, but
    // the queue entry must durably record that the code run began
    // finalization so recovery does not re-dispatch the worker.
    checkpoint(&conn, &ctx.run_id, FinalizationStage::ResultValidated, None)?;
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: worker_result_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::ResultValidated,
            commit_oid: None,
            pr_number: None,
            pr_url: None,
        },
    )?;

    // Stage 2: Commit the validated changes, then record the commit OID.
    let commit_out = commit_code_and_finalize(ctx, worker_result, runner, worker_result_path)?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::Committed,
        commit_out.commit_oid.as_deref(),
    )?;
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: worker_result_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::Committed,
            commit_oid: commit_out.commit_oid.clone(),
            pr_number: None,
            pr_url: None,
        },
    )?;

    // Stage 3: Push the daemon branch, then record the remote OID.
    let push_out = push_and_finalize(ctx, runner).await?;
    checkpoint(
        &conn,
        &ctx.run_id,
        FinalizationStage::Pushed,
        push_out.pushed_oid.as_deref(),
    )?;
    store.save_finalization(
        &ctx.claim,
        crate::state::queue::FinalizationCheckpoint {
            run_id: ctx.run_id.clone(),
            branch_name: ctx.worktree.branch_name.clone(),
            result_path: worker_result_path.to_path_buf(),
            stage: crate::state::queue::FinalizationStage::Pushed,
            commit_oid: commit_out.commit_oid.clone(),
            pr_number: None,
            pr_url: None,
        },
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

    // Best-effort removal of the trigger label. The entry must still
    // reach AwaitingReview even if GitHub returns 404 or any other
    // failure.
    if ctx.config.remove_label_on_completion {
        match client
            .remove_issue_label(
                ctx.issue.key.owner.as_str(),
                ctx.issue.key.repo.as_str(),
                ctx.issue.key.number,
                &ctx.config.ticket_label_code,
            )
            .await
        {
            Ok(_) => info!(
                issue = %ctx.issue.key.display_key(),
                label = %ctx.config.ticket_label_code,
                "removed trigger label"
            ),
            Err(err) => warn!(
                issue = %ctx.issue.key.display_key(),
                label = %ctx.config.ticket_label_code,
                error = %err,
                "trigger label removal failed; continuing"
            ),
        }
    }

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
