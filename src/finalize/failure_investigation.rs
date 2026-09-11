use super::{FinalizeAction, FinalizeContext, FinalizeOutput};

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worker::WorkerResult;

// Failure finalization
//
// (The investigation findings path was removed in release N+1,
// issue #331. The module name is kept: failure analysis is a
// different, still-active feature.)

/// Marker prefix for the *failure* comment. The marker
/// carries the `run_id` so a retry does not double-post.
pub const FAILURE_MARKER_PREFIX: &str = "<!-- automation-failure:";

/// Build the failure comment body. The comment is
/// generic — it does NOT link the worker's local
/// transcript (which is a local-only path); it just
/// names the `run_id` and the human-readable summary.
/// The voice-rule check runs on `summary` before the
/// comment is posted.
pub fn render_failure_comment(worker_result: &WorkerResult, run_id: &str) -> String {
    format!(
        "{}{run_id}\n\nThe automation run failed.\n\nDetails:\n{summary}\n{run_id} -->\n",
        FAILURE_MARKER_PREFIX,
        summary = worker_result.summary,
    )
}

/// Result of the failure-comment step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureOutcome {
    pub comment_posted: bool,
}

/// Post a generic failure comment (idempotent). The
/// function:
/// 1. Validates `worker_result.summary` through the
///    public-voice rule. A rejected text returns
///    `CaduceusError::Other(public-voice: ...)`; the
///    comment is never posted.
/// 2. Lists the issue's comments. If a comment
///    containing the `run_id`-scoped failure marker is
///    present, no POST is made.
/// 3. Otherwise POSTs the failure comment. A 201 is
///    required.
///
/// **Withdrawal** is the orchestrator's job. The
/// orchestrator checks the issue's trigger-label state
/// before calling this function; if the user has
/// withdrawn, the orchestrator skips the call and
/// transitions the entry to `Skipped`.
pub async fn post_failure_comment(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<FailureOutcome> {
    // 1. Validate.
    crate::finalize::validate_comment(&worker_result.summary, &ctx.config)
        .map_err(crate::finalize::terminal_from_voice)?;
    let issue = &ctx.issue.key;
    let owner = issue.owner.as_str();
    let repo = issue.repo.as_str();
    let number = issue.number;
    let run_id = &ctx.run_id;
    // 2. Look for existing marker.
    let list_path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
    let resp = client
        .get(&list_path, "application/vnd.github+json")
        .await?;
    // A 304 is the ETag-cached GET path replaying the cached
    // representation: the client guarantees `body` is the last
    // cached body (byte-identical to the 200 that stored the ETag),
    // and GitHub mints a fresh ETag on any state change, so a 304
    // proves the cached body is current. Parse it like a 200 instead
    // of failing the stage (issue #396, mirroring the #385 fix in
    // src/github/merge_detect.rs).
    if !matches!(resp.status, 200 | 304) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("list comments failed: {}", resp.status),
        });
    }
    let comments: Vec<serde_json::Value> = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed comments list: {err}")))?;
    let marker = format!("{}{}", FAILURE_MARKER_PREFIX, run_id);
    let existing = comments.iter().any(|c| {
        c.get("body")
            .and_then(|b| b.as_str())
            .map(|s| s.starts_with(&marker))
            .unwrap_or(false)
    });
    // 3. POST if absent.
    if !existing {
        let body = render_failure_comment(worker_result, run_id);
        let body_bytes = serde_json::to_vec(&serde_json::json!({ "body": body }))
            .map_err(|err| CaduceusError::Other(format!("serialize body: {err}")))?;
        let resp = client
            .post(&list_path, "application/vnd.github+json", &body_bytes)
            .await?;
        if !matches!(resp.status, 201) {
            return Err(CaduceusError::GitHubApi {
                status: resp.status,
                message: format!("post failure comment failed: {}", resp.status),
            });
        }
    }
    Ok(FailureOutcome {
        comment_posted: !existing,
    })
}

/// High-level wrapper.
pub async fn post_failure_comment_and_finalize(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<FinalizeOutput> {
    let outcome = post_failure_comment(ctx, client, worker_result).await?;
    Ok(FinalizeOutput {
        action: FinalizeAction::Commented,
        pr_url: None,
        pr_number: None,
        commit_oid: None,
        pushed_oid: None,
        comment_id: None,
        idempotency_observations: vec![format!(
            "failure_comment_posted={}",
            outcome.comment_posted
        )],
    })
}
