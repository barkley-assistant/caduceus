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
// Per-claim work loop
// ---------------------------------------------------------------------------

pub(crate) struct Outcome304(pub(crate) bool);

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_claim(
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
        network_profile: None,
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
