//! Claim-side review dispatch (issue #339, DAR §6.1–6.2, §8.1).
//!
//! [`run_review_claim`] is the review counterpart of `run_claim`
//! (`src/daemon/tick/per_claim.rs`): the step-6 drain loop
//! (`src/daemon/tick/mod.rs`) claims review entries via
//! `ReviewStore::acquire_next_review` and spawns this function. It
//! operates on a [`ClaimedReview`] + [`ReviewRunGuard`] — never on
//! the issue-shaped `ActiveRunGuard` (DAR §4.1: review identity never
//! enters `IssueKey`; claim files never mix).
//!
//! Routing contract (DAR §8.1):
//!
//! | Condition | Error | Route |
//! |---|---|---|
//! | Worker exit / missing result / `status:failure` / validation reject | `Worker` / `ReviewSchemaVersion` | retry (budget burns) |
//! | Mutation violation post-run | `ReviewSourceMutation` | Terminal (NeedsAttention, worktree KEPT) |
//! | Head SHA gone at mirror fetch | `HeadShaUnavailable` | fourth route → `finish_skip` |
//! | PR fetch 404 / closed-unmerged | `ReviewGone` | fourth route → `finish_skip` |
//! | Oversized diff | none (not an error) | direct `finish_skip` |
//! | Valid result | none | history append + `complete_review` |
//! | Infra (transport, git, IO, OCI) | `Infrastructure` | `finish_infrastructure` |
//!
//! Result path (DAR §6.2): the daemon reads the worker result
//! EXCLUSIVELY from `ExecutorOutcome.result_path` — OCI:
//! `<state_dir>/oci-runs/<run_id>/output/worker-result.json`;
//! TrustedHost: `<worktree>/worker-result.json`. No legacy fallback,
//! no synthesized result: a missing result file is an execution
//! failure through the retry budget.

use std::sync::Arc;

use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::daemon::orchestration::{classify_error, ReviewRunGuard, Services};
use crate::github::pr::fetch_pull_request;
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::repo::{capture_control_file_digests, BareMirror, ReviewWorktree};
use crate::review::{ExecutionStatus, ReviewTarget, Verdict};
use crate::state::meta::TickOutcome;
use crate::state::review::{ClaimedReview, ReviewHistoryRow, ReviewPhase, ReviewStore};
use crate::worker::prompt::write_prompt;
use crate::worker::review_prompt::{
    build_review_prompt, emit_oversized_pr_skip, ReviewPromptInput, ReviewPromptOutcome,
};

use super::awaiting_review::{outcome_for_class, quiet_skip_kind, QuietSkipKind};

/// Git remote URL resolver for one review claim — the tick passes
/// `git_https_remote(&cfg.api_base, ..)`; tests inject a `file://`
/// resolver (mirror of `poll_review_step`'s seam). Owned so it can
/// ride into the spawned drain task ('static).
pub type RemoteResolver = Arc<dyn Fn(&str, &str) -> CaduceusResult<String> + Send + Sync>;

// ---------------------------------------------------------------------------
// DAR §13 dispatch events (owners: #339)
// ---------------------------------------------------------------------------

/// DAR §13 dispatch event: the review run was dispatched to the
/// executor.
pub const REVIEW_STARTED_EVENT: &str = "review_started";
/// DAR §13 dispatch event: the worker completed a valid result.
pub const REVIEW_WORKER_COMPLETED_EVENT: &str = "review_worker_completed";
/// DAR §13 dispatch event: execution failed (worker-attributable).
pub const REVIEW_EXECUTION_FAILED_EVENT: &str = "review_execution_failed";
/// DAR §13 dispatch event: a retry was scheduled (attempts < budget).
pub const REVIEW_RETRY_SCHEDULED_EVENT: &str = "review_retry_scheduled";
/// DAR §13 dispatch event: the review passed (verdict Pass).
pub const REVIEW_PASSED_EVENT: &str = "review_passed";
/// DAR §13 dispatch event: the review failed (verdict Fail) —
/// deliberately distinct from `review_execution_failed`.
pub const REVIEW_FAILED_VERDICT_EVENT: &str = "review_failed_verdict";
/// DAR §13 dispatch event: head SHA unavailable → quiet skip.
pub const REVIEW_SKIPPED_HEAD_SHA_UNAVAILABLE_EVENT: &str = "review_skipped_head_sha_unavailable";
/// Builder-chosen event (DAR §13 does not name it): PR 404 /
/// closed-unmerged between admission and claim → quiet skip. The
/// `reason` field is `pr_not_found` | `closed_unmerged`.
pub const REVIEW_SKIPPED_PR_GONE_EVENT: &str = "review_skipped_pr_gone";

fn emit_started(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_STARTED_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review run dispatched"
    );
}

fn emit_worker_completed(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_WORKER_COMPLETED_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review worker completed a valid result"
    );
}

fn emit_execution_failed(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_EXECUTION_FAILED_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review execution failed; routing through the retry budget"
    );
}

fn emit_retry_scheduled(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_RETRY_SCHEDULED_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review retry scheduled with backoff"
    );
}

fn emit_passed(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_PASSED_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review passed"
    );
}

fn emit_failed_verdict(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_FAILED_VERDICT_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review failed the code (verdict fail)"
    );
}

fn emit_skipped_head_sha_unavailable(target: &ReviewTarget, run_id: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_SKIPPED_HEAD_SHA_UNAVAILABLE_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        "review skipped: head SHA unavailable (force-push + GC); \
         the successor SHA is admitted by the next poll"
    );
}

fn emit_skipped_pr_gone(target: &ReviewTarget, run_id: &str, reason: &str) {
    info!(
        target: "caduceus",
        event = REVIEW_SKIPPED_PR_GONE_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        run_id = run_id,
        reason = reason,
        "review skipped: PR gone (404 or closed-unmerged)"
    );
}

// ---------------------------------------------------------------------------
// Review-side router (the DAR §8.1 routing table, review guard)
// ---------------------------------------------------------------------------

/// The review counterpart of `handle_infra_or_retry`. Same
/// four-route fan-out, operating on a [`ReviewRunGuard`]:
///
/// 1. fourth route — `HeadShaUnavailable` / `ReviewGone` → quiet
///    `finish_skip` with the structured event (NOT NeedsAttention,
///    NOT retry-budget-consuming);
/// 2. terminal — mutation violation → `finish_mutation_violation`
///    (worktree KEPT); unknown terminal → `finish_needs_attention`;
/// 3. retry-budget — `finish_retry` (+ `review_execution_failed` /
///    `review_retry_scheduled`);
/// 4. infrastructure — `finish_infrastructure` (unchanged).
pub(crate) async fn handle_review_infra_or_retry(
    cfg: Config,
    guard: &mut ReviewRunGuard,
    err: &CaduceusError,
    class: crate::daemon::orchestration::FailureClass,
) -> CaduceusResult<TickOutcome> {
    // Capture the run id BEFORE any `finish_*` call consumes the
    // claim: every emit below fires around a transition that moves
    // the claim out of the guard (`run_id()` panics after `take`).
    let run_id = guard.run_id().to_string();

    // Fourth route (issue #339): quiet skip BEFORE the three-route
    // fan-out (DAR §8.1). The variant match is shared with the issue
    // router (`quiet_skip_kind`) so the skip conditions live in ONE
    // place.
    if let Some(kind) = quiet_skip_kind(err) {
        match kind {
            QuietSkipKind::HeadShaUnavailable => {
                emit_skipped_head_sha_unavailable(guard.target(), &run_id);
            }
            QuietSkipKind::PrGone { reason } => {
                emit_skipped_pr_gone(guard.target(), &run_id, &reason);
            }
        }
        let _ = guard.finish_skip(&err.to_string()).await;
        return Ok(TickOutcome::Processed);
    }
    if class.is_terminal() {
        let error_text = err.to_string();
        if matches!(err, CaduceusError::ReviewSourceMutation { .. }) {
            // Emits `review_mutation_violation` + routes to
            // NeedsAttention; the worktree is kept as forensic
            // evidence (DAR §8.1). Attempts NOT incremented.
            guard.finish_mutation_violation(err).await?;
        } else {
            guard
                .finish_needs_attention(&error_text, "terminal/unknown", &error_text)
                .await?;
        }
        return Ok(TickOutcome::Failed);
    }
    if class.counts_against_retry_budget() {
        emit_execution_failed(guard.target(), &run_id);
        let new_phase = guard
            .finish_retry(&err.to_string(), cfg.max_retries_per_issue)
            .await?;
        if new_phase == ReviewPhase::Queued {
            emit_retry_scheduled(guard.target(), &run_id);
        }
        return Ok(map_review_phase_to_outcome(new_phase));
    }
    let now = Utc::now();
    let not_before = now + chrono::Duration::seconds(cfg.retry_backoff_seconds as i64);
    let _ = guard
        .finish_infrastructure(&err.to_string(), not_before)
        .await;
    Ok(outcome_for_class(class))
}

/// Map a review queue phase to the tick outcome the drain folds into
/// the outer tick state (mirror of the issue `map_phase_to_outcome`).
fn map_review_phase_to_outcome(phase: ReviewPhase) -> TickOutcome {
    match phase {
        ReviewPhase::Queued
        | ReviewPhase::InProgress
        | ReviewPhase::Done
        | ReviewPhase::Skipped => TickOutcome::Processed,
        ReviewPhase::Failed | ReviewPhase::NeedsAttention => TickOutcome::Failed,
    }
}

// ---------------------------------------------------------------------------
// Claim-side pipeline
// ---------------------------------------------------------------------------

/// Run one review claim end-to-end (DAR §6.1–6.2, §8.1). The drain
/// loop has already claimed `claimed` (the entry is `InProgress` and
/// the claim file exists); every fallible step classifies and routes
/// through [`handle_review_infra_or_retry`] so the entry always
/// reaches a terminal transition or a backoff requeue.
///
/// `resolve_remote` derives the git remote URL for a repo — the tick
/// passes `git_https_remote(&cfg.api_base, ..)`; tests inject a
/// `file://` resolver (mirror of `poll_review_step`'s seam).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_review_claim(
    cfg: Config,
    services: &Services,
    client: Arc<Client>,
    review_store: &ReviewStore,
    claimed: ClaimedReview,
    guard: &mut ReviewRunGuard,
    cancellation: CancellationToken,
    _http_status: &mut Option<u16>,
    _admit: crate::scheduler::Admission,
    resolve_remote: RemoteResolver,
) -> CaduceusResult<TickOutcome> {
    let target = claimed.entry.target.clone();
    let run_id = guard.run_id().to_string();
    let runner = services.git.runner().clone();

    // 1. Fetch the PR wire row. 404 maps to `Ok(None)` in the
    //    transport (auto-review gone-state B, §9.3) → gone skip;
    //    closed-unmerged → gone skip; every other error classifies
    //    and routes normally.
    let pr = match fetch_pull_request(
        client.as_ref(),
        &target.repository.owner,
        &target.repository.repo,
        target.pull_request,
    )
    .await
    {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            let err = CaduceusError::ReviewGone {
                reason: "pr_not_found".to_string(),
            };
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    if pr.state.as_deref() == Some("closed") && pr.merged != Some(true) {
        let err = CaduceusError::ReviewGone {
            reason: "closed_unmerged".to_string(),
        };
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 2. Ensure the daemon-owned bare mirror (lazy bootstrap, DAR
    //    §6.3 host-path discipline). The SHA-anchored fetch happens
    //    inside `ReviewWorktree::create_review` and surfaces
    //    `HeadShaUnavailable` there (DAR §8.1 fourth route).
    let remote = match resolve_remote(&target.repository.owner, &target.repository.repo) {
        Ok(remote) => remote,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let mirror = match BareMirror::ensure(
        &runner,
        &cfg,
        &target.repository.owner,
        &target.repository.repo,
        &remote,
        &target.base_ref,
    )
    .await
    {
        Ok(mirror) => mirror,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 3. Materialise the review worktree: detached HEAD at the exact
    //    head SHA (DAR §2.3). `HeadShaUnavailable` surfaces at the
    //    SHA-anchored fetch inside `create_review`.
    let worktree = match ReviewWorktree::create_review(&runner, &mirror, &run_id, &target).await {
        Ok(wt) => wt,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    guard.attach_worktree(worktree.clone()).await;

    // 4. Compute the merge-base diff (DAR §2.2): review scope is
    //    ALWAYS `git diff <merge_base> <head_sha>`. The RAW runner
    //    variant is required — `run_args`'s stdout is capped at
    //    `GIT_OUTPUT_BYTE_CAP` (32 KiB), and the prompt builder's
    //    oversized detection needs the TRUE diff length.
    let diff_output = match runner
        .run_in_raw(
            &cfg,
            "review-diff",
            &[
                "diff".as_ref(),
                target.merge_base.as_ref(),
                target.head_sha.as_ref(),
            ],
            Some(worktree.path.as_path()),
        )
        .await
    {
        Ok(out) => out,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    if diff_output.cancelled {
        let err = CaduceusError::Cancelled;
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }
    if diff_output.timed_out || diff_output.status != Some(0) {
        let err = CaduceusError::Git {
            operation: "review-diff",
            stderr: diff_output.stderr,
        };
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }
    let diff = String::from_utf8_lossy(&diff_output.stdout).into_owned();

    // 5. Assemble the PR discussion window (oldest → newest, capped;
    //    the prompt builder tail-samples over its budget, DAR §7.1).
    let discussion = match fetch_review_discussion(
        client.as_ref(),
        &target.repository.owner,
        &target.repository.repo,
        target.pull_request,
    )
    .await
    {
        Ok(d) => d,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 6. Build the review prompt. An oversized diff is NOT an error:
    //    deterministically unreviewable → direct skip (DAR §7.1),
    //    never the retry path.
    let prompt_input = ReviewPromptInput {
        target: &target,
        pr: &pr,
        diff: &diff,
        // Minimal repo-context assembly for #339: an empty section
        // renders as a header-only block; per-file excerpt sampling
        // is a Phase-2 extension seam (§17).
        repo_context: "",
        discussion: &discussion,
        worker_instruction: cfg.worker_instruction.as_str(),
    };
    let prompt = match build_review_prompt(&prompt_input) {
        Ok(ReviewPromptOutcome::Bounded { prompt }) => prompt,
        Ok(ReviewPromptOutcome::Oversized { diff_bytes, budget }) => {
            emit_oversized_pr_skip(
                &target.repository.full_name(),
                target.pull_request,
                &target.head_sha,
                diff_bytes,
                budget,
            );
            let _ = guard
                .finish_skip("oversized review diff (deterministically unreviewable)")
                .await;
            return Ok(TickOutcome::Processed);
        }
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    if let Err(err) = write_prompt(&worktree.path, &prompt) {
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 7. Capture the pre-run control-file digests (DAR §10.2).
    let pre_digests = match capture_control_file_digests(&worktree.path) {
        Ok(d) => d,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };

    // 8. Dispatch through the configured executor (OCI in production;
    //    the same trait-object seam as the issue path).
    let self_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            let err = CaduceusError::Worktree {
                context: "tick",
                stderr: format!("current_exe: {err}"),
            };
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let spec = crate::executor::ExecutorSpec {
        self_exe,
        target: crate::executor::WorkTarget::PullRequest(target.clone()),
        worktree: worktree.path.clone(),
        run_id: run_id.clone(),
        // Reviews carry no issue context (DAR §6.1): the PR arm
        // passes `context_json` through to CADUCEUS_CONTEXT_JSON
        // verbatim, and the empty object is the documented default.
        context_json: "{}".to_string(),
        worker_command: cfg.worker_command.clone(),
        cancellation: cancellation.clone(),
    };
    emit_started(&target, &run_id);
    let exec_outcome = match services.executor.run(&spec).await {
        Ok(o) => o,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let supervisor_outcome = exec_outcome.outcome.clone();
    if supervisor_outcome.timed_out
        || supervisor_outcome.cancelled
        || supervisor_outcome.disk_pressure
    {
        let _ = guard.finish_cancelled().await;
        return Ok(TickOutcome::Cancelled);
    }
    let _ = services.clock.now();
    let host_result_path = exec_outcome.result_path.clone();

    // 9. Enforce the read-only contract on EVERY post-exit path,
    //    BEFORE result acceptance (DAR §10). A detected mutation is
    //    Terminal — the worktree is kept as forensic evidence.
    if let Err(err) =
        crate::repo::enforce_review_read_only(&runner, &worktree.path, &pre_digests).await
    {
        if matches!(err, CaduceusError::ReviewSourceMutation { .. }) {
            guard.finish_mutation_violation(&err).await?;
            return Ok(TickOutcome::Failed);
        }
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 10. Read the worker result EXCLUSIVELY from the mode-correct
    //     path (DAR §6.2). No legacy fallback, no synthesized result:
    //     a missing/unparseable result is an execution failure
    //     through the retry budget. A `status: failure` document is a
    //     VALID parse whose execution failed → retry (DAR §8).
    let result = match crate::worker::parse_review_result_file(&host_result_path) {
        Ok(result) => result,
        Err(err) => {
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    if result.status == ExecutionStatus::Failure {
        let err = CaduceusError::Worker {
            context: "result",
            stderr: format!(
                "worker reported execution failure in {}",
                host_result_path.display()
            ),
        };
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 11. Persist the durable result (DAR §4.3): the history row is
    //     written BEFORE any publication. The 5.6 poller derives due
    //     finalizations from history + state, so no explicit enqueue
    //     is needed. The finalizer updates ReviewState and publishes
    //     the sticky comment (#310).
    let result_json = match serde_json::to_string(&result) {
        Ok(json) => json,
        Err(err) => {
            let err = CaduceusError::Other(format!("serialize ReviewResult: {err}"));
            let class = classify_error(&err);
            return handle_review_infra_or_retry(cfg, guard, &err, class).await;
        }
    };
    let row = ReviewHistoryRow {
        review_run_id: run_id.clone(),
        repository: target.repository.clone(),
        pull_request: target.pull_request,
        head_sha: target.head_sha.clone(),
        review_generation: claimed.entry.review_generation,
        completed_at: services.clock.now(),
        result_json,
    };
    if let Err(err) = review_store.append_history(row) {
        let class = classify_error(&err);
        return handle_review_infra_or_retry(cfg, guard, &err, class).await;
    }

    // 11.5. Persist the completion observation BEFORE the terminal
    //      transition (gate #333 finding 1). Discovery dedup reads
    //      `ReviewState.last_reviewed_head_sha` (DAR §4.3), which the
    //      finalizer only writes at publication. Without this write,
    //      the moment an entry completes (Done + history row) the NEXT
    //      tick's 5.5 re-admits the identical head SHA — generation
    //      bump + publication reset — `due_finalizations` suppresses
    //      the completed row, and the worker re-runs forever with no
    //      sticky comment ever published. Writing here makes dedup
    //      hold from completion onward: `save_review_state`
    //      CAS-rejects generation regressions, so a late-completing
    //      older run cannot regress the pointer (DAR §9.4); the
    //      finalizer's later saves are idempotent overwrites.
    if let Some(mut current) = review_store.review_state(&target.repository, target.pull_request)? {
        if current.review_generation == claimed.entry.review_generation {
            current.last_reviewed_head_sha = Some(target.head_sha.clone());
            current.last_reviewed_at = Some(services.clock.now());
            current.last_run_id = Some(run_id.clone());
            current.last_verdict = result.review.as_ref().map(|r| r.verdict);
            if let Err(err) = review_store.save_review_state(&current) {
                let class = classify_error(&err);
                return handle_review_infra_or_retry(cfg, guard, &err, class).await;
            }
        }
    }

    // 12. Terminal success: the entry moves to Done and the claim is
    //     released. The verdict drives the distinct completion event
    //     (`review_passed` vs `review_failed_verdict` — never
    //     conflatable with execution failure).
    guard.finish_done().await?;
    emit_worker_completed(&target, &run_id);
    match result.review.as_ref().map(|review| review.verdict) {
        Some(Verdict::Pass) => emit_passed(&target, &run_id),
        Some(Verdict::Fail) => emit_failed_verdict(&target, &run_id),
        // status == Success guarantees `review` is present (the
        // #305 validator enforces the iff-rule at parse time).
        None => {}
    }
    Ok(TickOutcome::Processed)
}

/// Assemble the PR discussion window for the prompt (DAR §7 section
/// 6): the PR's issue-comments, oldest → newest, capped at the same
/// retention limit as the issue path, rendered as
/// `@author:\n<body>\n\n` blocks. The prompt builder tail-samples
/// over `MAX_REVIEW_DISCUSSION_BYTES`, so no truncation happens here.
async fn fetch_review_discussion(
    client: &Client,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> CaduceusResult<String> {
    let path = format!("/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let response = client.get(&path, crate::github::ACCEPT_VALUE).await?;
    let wire: Vec<ReviewCommentWire> = serde_json::from_slice(&response.body).map_err(|err| {
        CaduceusError::Other(format!(
            "PR {pr_number} discussion JSON parse for {owner}/{repo}: {err}"
        ))
    })?;
    let mut comments: Vec<crate::github::issue::IssueComment> = wire
        .into_iter()
        .map(|c| crate::github::issue::IssueComment {
            author: c.user.and_then(|u| u.login).unwrap_or_default(),
            body: c.body.unwrap_or_default(),
            created_at: c.created_at.unwrap_or_else(Utc::now),
        })
        .collect();
    comments.sort_by_key(|c| c.created_at);
    if comments.len() > crate::github::issue::MAX_RETAINED_COMMENTS {
        let drop = comments.len() - crate::github::issue::MAX_RETAINED_COMMENTS;
        comments.drain(..drop);
    }
    let mut out = String::new();
    for comment in comments {
        use std::fmt::Write as _;
        let _ = writeln!(out, "@{}:\n{}\n", comment.author, comment.body);
    }
    Ok(out)
}

/// Minimal wire shape for the PR discussion endpoint (the issue
/// path's `CommentWire` is private; PR comments share the same wire
/// contract — `user.login`, `body`, `created_at`).
#[derive(Clone, Debug, serde::Deserialize)]
struct ReviewCommentWire {
    #[serde(rename = "user")]
    user: Option<ReviewCommentUserWire>,
    body: Option<String>,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct ReviewCommentUserWire {
    login: Option<String>,
}

/// Test seam: the review-side router with an explicit config,
/// mirroring `handle_infra_or_retry_for_tests` for the issue guard.
/// Public so integration tests can prove the DAR §8.1 routing
/// boundaries without owning a runtime.
pub async fn handle_review_infra_or_retry_for_tests(
    cfg: Config,
    guard: &mut ReviewRunGuard,
    err: &CaduceusError,
    class: crate::daemon::orchestration::FailureClass,
) -> CaduceusResult<TickOutcome> {
    handle_review_infra_or_retry(cfg, guard, err, class).await
}

/// Test seam: the claim-side review dispatch with an injected git
/// remote resolver (mirror of `poll_review_step_for_tests`), so
/// integration tests drive the pipeline against a real local remote
/// without owning a GitHub-shaped `api_base`.
#[allow(clippy::too_many_arguments)]
pub async fn run_review_claim_for_tests(
    cfg: Config,
    services: &Services,
    client: Arc<Client>,
    review_store: &ReviewStore,
    claimed: ClaimedReview,
    guard: &mut ReviewRunGuard,
    cancellation: CancellationToken,
    http_status: &mut Option<u16>,
    admit: crate::scheduler::Admission,
    resolve_remote: RemoteResolver,
) -> CaduceusResult<TickOutcome> {
    run_review_claim(
        cfg,
        services,
        client,
        review_store,
        claimed,
        guard,
        cancellation,
        http_status,
        admit,
        resolve_remote,
    )
    .await
}
