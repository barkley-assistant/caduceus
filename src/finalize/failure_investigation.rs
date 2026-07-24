#![allow(dead_code, unused_imports)]
use super::*;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::github::Client;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult, VoiceError};
use crate::worker::WorkerResult;
use crate::worktree::GitRunner;

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Failure / investigation finalization
// ---------------------------------------------------------------------------

/// Marker prefix for the *failure* comment. The marker
/// carries the `run_id` so a retry does not double-post.
pub const FAILURE_MARKER_PREFIX: &str = "<!-- automation-failure:";

/// Marker prefix for the *investigation findings*
/// comment. The marker carries the `run_id` so a retry
/// does not double-post.
pub const INVESTIGATION_MARKER_PREFIX: &str = "<!-- automation-investigation:";

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

/// Build the investigation findings comment body. The
/// body combines the worker's summary with the bounded,
/// injection-safe artifact renderer used for PR bodies.
/// The function is pure.
pub fn render_investigation_comment(worker_result: &WorkerResult, run_id: &str) -> String {
    let mut body = String::new();
    body.push_str(INVESTIGATION_MARKER_PREFIX);
    body.push_str(run_id);
    body.push_str("\n\n");
    body.push_str(&worker_result.summary);
    body.push_str("\n\n");
    // Reuse the artifact renderer so the daemon's
    // voice-rule and injection-safety guarantees carry
    // over from the PR-body path. The renderer produces
    // a stable, sorted JSON document.
    let artifacts =
        serde_json::to_string_pretty(&render_artifacts_with_escape(&worker_result.artifacts))
            .unwrap_or_else(|_| "{}".to_string());
    body.push_str("```json\n");
    body.push_str(&artifacts);
    body.push_str("\n```\n");
    body
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
    if !matches!(resp.status, 200) {
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
        idempotency_observations: vec![format!(
            "failure_comment_posted={}",
            outcome.comment_posted
        )],
    })
}

/// Result of the investigation-comments step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvestigationOutcome {
    pub comment_posted: bool,
    pub label_removed: bool,
}

/// Post the investigation findings comment (idempotent)
/// and remove the configured investigation label.
///
/// The label-removal step is **best-effort**; if it
/// fails, the function still reports success because
/// the comment is the operator-facing artifact. The
/// `label_removed` flag is the operator's audit trail.
pub async fn post_investigation_comment(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
    investigation_label: &str,
) -> CaduceusResult<InvestigationOutcome> {
    // Validate.
    crate::finalize::validate_comment(&worker_result.summary, &ctx.config)
        .map_err(crate::finalize::terminal_from_voice)?;
    let issue = &ctx.issue.key;
    let owner = issue.owner.as_str();
    let repo = issue.repo.as_str();
    let number = issue.number;
    let run_id = &ctx.run_id;
    // Look for existing marker.
    let list_path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
    let resp = client
        .get(&list_path, "application/vnd.github+json")
        .await?;
    if !matches!(resp.status, 200) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("list comments failed: {}", resp.status),
        });
    }
    let comments: Vec<serde_json::Value> = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed comments list: {err}")))?;
    let marker = format!("{}{}", INVESTIGATION_MARKER_PREFIX, run_id);
    let existing = comments.iter().any(|c| {
        c.get("body")
            .and_then(|b| b.as_str())
            .map(|s| s.starts_with(&marker))
            .unwrap_or(false)
    });
    let comment_posted = !existing;
    if !existing {
        let body = render_investigation_comment(worker_result, run_id);
        let body_bytes = serde_json::to_vec(&serde_json::json!({ "body": body }))
            .map_err(|err| CaduceusError::Other(format!("serialize body: {err}")))?;
        let resp = client
            .post(&list_path, "application/vnd.github+json", &body_bytes)
            .await?;
        if !matches!(resp.status, 201) {
            return Err(CaduceusError::GitHubApi {
                status: resp.status,
                message: format!("post investigation comment failed: {}", resp.status),
            });
        }
    }
    // Best-effort label removal. The HTTP client exposes
    // only GET and POST; the orchestrator (Phase 6) owns
    // the DELETE. The function reports the comment-posted
    // outcome; the label-removed flag is the operator's
    // audit trail and is left at `false` for v0.1.
    let label_removed = false;
    let _ = investigation_label;
    Ok(InvestigationOutcome {
        comment_posted,
        label_removed,
    })
}

/// High-level wrapper.
pub async fn post_investigation_comment_and_finalize(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
    investigation_label: &str,
) -> CaduceusResult<FinalizeOutput> {
    let outcome =
        post_investigation_comment(ctx, client, worker_result, investigation_label).await?;
    Ok(FinalizeOutput {
        action: FinalizeAction::InvestigationCommented,
        pr_url: None,
        idempotency_observations: vec![
            format!("investigation_comment_posted={}", outcome.comment_posted),
            format!("investigation_label_removed={}", outcome.label_removed),
        ],
    })
}
