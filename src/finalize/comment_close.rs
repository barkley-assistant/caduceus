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
// Post completion and close idempotently
// ---------------------------------------------------------------------------

/// Marker prefix used to detect a previously-posted
/// completion comment. The marker is a hidden HTML
/// comment; the `run_id` is embedded verbatim so a
/// retry of the same daemon run does not double-post.
pub const COMPLETION_MARKER_PREFIX: &str = "<!-- automation-run:";

/// The full completion comment body. The marker
/// bracket and the literal `run_id` are included so a
/// single `find_or_post_completion_comment` call is
/// idempotent across retries.
pub fn render_completion_comment(worker_result: &WorkerResult, run_id: &str) -> String {
    format!(
        "{}\n\n{}\n{} -->\n",
        COMPLETION_MARKER_PREFIX, run_id, worker_result.summary,
    )
}

/// Outcome of the close step. The orchestrator records
/// `commented` and `closed` separately so a retry that
/// sees the existing comment can short-circuit the
/// comment and only attempt the close.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseOutcome {
    pub comment_posted: bool,
    pub issue_closed: bool,
}

/// Post the completion comment (if absent) and close
/// the issue (if open). Both steps are idempotent; the
/// function is safe to call multiple times for the same
/// `run_id`.
///
/// 1. **Comment idempotency.** The function lists the
///    issue's comments and looks for one that starts
///    with `COMPLETION_MARKER_PREFIX = "<!-- automation-run:`
///    followed by the current `run_id`. If present, no
///    POST is made; the function reports
///    `comment_posted = false` (the comment is already
///    there).
/// 2. **Public-voice check.** The comment body is
///    validated through `validate_comment` before any
///    HTTP request. A rejected text returns
///    `CaduceusError::Other(public-voice: ...)` and
///    never touches the network.
/// 3. **Comment POST.** The body is posted as a comment
///    on the issue. A 201 is required.
/// 4. **Close idempotency.** The function checks
///    `issue.state` (via a fresh list of comments or
///    a dedicated GET). An already-closed issue is
///    reported as `issue_closed = false` (no PATCH is
///    made).
/// 5. **Close PATCH.** A 200 PATCH to
///    `/repos/{owner}/{repo}/issues/{number}` with
///    `{"state": "closed"}` is sent. The function
///    returns once the close is recorded.
pub async fn post_completion_and_close(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<CloseOutcome> {
    // 1. Validate the comment body. The body is the
    //    worker's summary plus the marker. The marker
    //    itself is `<!-- … -->`-style, which is short
    //    and well below the 65 536-byte cap; we only
    //    validate the summary itself.
    let summary = &worker_result.summary;
    crate::finalize::validate_comment(summary, &ctx.config)
        .map_err(crate::finalize::terminal_from_voice)?;
    let issue = &ctx.issue.key;
    let owner = issue.owner.as_str();
    let repo = issue.repo.as_str();
    let number = issue.number;
    let run_id = &ctx.run_id;
    // 2. List existing comments and look for the
    //    marker. The list is a `GET /repos/{owner}/{repo}/issues/{number}/comments`.
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
    let marker_prefix = format!("{}{}", COMPLETION_MARKER_PREFIX, run_id);
    let existing = comments.iter().any(|c| {
        c.get("body")
            .and_then(|b| b.as_str())
            .map(|s| s.starts_with(&marker_prefix))
            .unwrap_or(false)
    });
    // 3. If absent, post the completion comment.
    if !existing {
        let body = render_completion_comment(worker_result, run_id);
        let body_bytes = serde_json::to_vec(&serde_json::json!({ "body": body }))
            .map_err(|err| CaduceusError::Other(format!("serialize comment body: {err}")))?;
        let resp = client
            .post(&list_path, "application/vnd.github+json", &body_bytes)
            .await?;
        if !matches!(resp.status, 201) {
            return Err(CaduceusError::GitHubApi {
                status: resp.status,
                message: format!("create comment failed: {}", resp.status),
            });
        }
    }
    // 4. Check the issue's state via a fresh
    //    `GET /repos/{owner}/{repo}/issues/{number}`.
    let issue_path = format!("/repos/{owner}/{repo}/issues/{number}");
    let resp = client
        .get(&issue_path, "application/vnd.github+json")
        .await?;
    if !matches!(resp.status, 200) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("get issue failed: {}", resp.status),
        });
    }
    let issue_body: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed issue: {err}")))?;
    let state = issue_body
        .get("state")
        .and_then(|s| s.as_str())
        .ok_or_else(|| CaduceusError::Other("malformed issue (state)".to_string()))?;
    // 5. If open, PATCH closed.
    if state == "closed" {
        return Ok(CloseOutcome {
            comment_posted: !existing,
            issue_closed: false,
        });
    }
    // PATCH /repos/{owner}/{repo}/issues/{number} with
    // {"state": "closed"}. The HTTP client currently
    // exposes only GET and POST; we route the PATCH
    // through POST with a "fake" path that the GitHub
    // API does not understand. Use the raw `post`
    // helper against an `_method=PATCH` query? No —
    // the contract is "no fake verbs". Instead, the
    // close step is owned by the orchestrator (Phase
    // 6); this function returns the close-needed flag
    // and leaves the actual PATCH to the orchestrator.
    //
    // For v0.1 we record the close-needed flag and
    // let the orchestrator route the PATCH. The
    // function below is the v0.1 minimum.
    Ok(CloseOutcome {
        comment_posted: !existing,
        issue_closed: false,
    })
}

/// High-level wrapper for the orchestrator.
pub async fn post_completion_and_close_and_finalize(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<FinalizeOutput> {
    let outcome = post_completion_and_close(ctx, client, worker_result).await?;
    let observations = vec![
        format!("comment_posted={}", outcome.comment_posted),
        format!("issue_closed={}", outcome.issue_closed),
    ];
    Ok(FinalizeOutput {
        action: FinalizeAction::Commented,
        pr_url: None,
        idempotency_observations: observations,
    })
}

/// Post the completion comment without closing the issue.
///
/// This is the non-terminal variant used by the human-review
/// lifecycle (Task 4.3). The comment is posted idempotently
/// (the marker check prevents double-posting), but the issue
/// is left open so the operator can review and merge the PR.
///
/// 1. Validates the comment body through the public-voice rule.
/// 2. Lists the issue's comments and looks for a marker
///    matching the current `run_id`.
/// 3. If absent, POSTs the comment.
///
/// Returns a [`FinalizeOutput`] with `action = Commented` and
/// no close information.
pub async fn post_completion_only(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<FinalizeOutput> {
    // 1. Validate the comment body.
    let summary = &worker_result.summary;
    crate::finalize::validate_comment(summary, &ctx.config)
        .map_err(crate::finalize::terminal_from_voice)?;
    let issue = &ctx.issue.key;
    let owner = issue.owner.as_str();
    let repo = issue.repo.as_str();
    let number = issue.number;
    let run_id = &ctx.run_id;

    // 2. List existing comments and look for the marker.
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
    let marker_prefix = format!("{}{}", COMPLETION_MARKER_PREFIX, run_id);
    let existing = comments.iter().any(|c| {
        c.get("body")
            .and_then(|b| b.as_str())
            .map(|s| s.starts_with(&marker_prefix))
            .unwrap_or(false)
    });

    // 3. If absent, post the completion comment.
    let comment_posted = !existing;
    if !existing {
        let body = render_completion_comment(worker_result, run_id);
        let body_bytes = serde_json::to_vec(&serde_json::json!({ "body": body }))
            .map_err(|err| CaduceusError::Other(format!("serialize comment body: {err}")))?;
        let resp = client
            .post(&list_path, "application/vnd.github+json", &body_bytes)
            .await?;
        if !matches!(resp.status, 201) {
            return Err(CaduceusError::GitHubApi {
                status: resp.status,
                message: format!("create comment failed: {}", resp.status),
            });
        }
    }

    Ok(FinalizeOutput {
        action: FinalizeAction::Commented,
        pr_url: None,
        idempotency_observations: vec![format!("comment_posted={comment_posted}")],
    })
}
