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
// Pull request: find or create, idempotent
// ---------------------------------------------------------------------------

/// Find or create the pull request for the daemon branch.
///
/// 1. Validate every public-text field (title, body) through
///    the public-voice rule. A rejected text returns
///    `CaduceusError::Other(public-voice: ...)` before any
///    HTTP request is made.
/// 2. Query `GET /repos/{owner}/{repo}/pulls?state=open&head={owner}:{branch}&base={base}`.
///    * **Zero matches** → POST the PR.
///    * **One match** → reuse the existing PR. The function
///      returns the existing `number` and `url` with
///      `reused = true`; no POST is made.
///    * **Multiple matches** → return
///      `CaduceusError::Other(multiple PRs match)`. The
///      operator must reconcile.
/// 3. A retry after a lost POST response re-queries the
///    open-PRs list before posting. The function does this
///    transparently: the POST only happens when the query
///    returns zero matches.
///
/// The function does **not** call `gh` or use the operator's
/// local `git`; the only HTTP client is the typed
/// `caduceus::github::Client`.
pub async fn find_or_create_pull_request(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<crate::github::PullRequest> {
    let issue = &ctx.issue.key;
    let owner = issue.owner.as_str();
    let repo = issue.repo.as_str();
    let branch = ctx.worktree.branch_name.as_str();
    let base = "main";
    // 1. Validate public text.
    let title = crate::finalize::build_pr_title(worker_result, &ctx.config)?;
    let body = crate::finalize::build_pr_body(worker_result, issue, &ctx.run_id, &ctx.config)?;
    // 2. Query open PRs.
    let list_path = format!("/repos/{owner}/{repo}/pulls");
    let query = format!(
        "state=open&head={owner}:{branch}&base={base}",
        owner = urlencode(owner),
        branch = urlencode(branch),
        base = urlencode(base),
    );
    let list_url = format!("{list_path}?{query}");
    let resp = client.get(&list_url, "application/vnd.github+json").await?;
    if !matches!(resp.status, 200) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("list pull requests failed: {}", resp.status),
        });
    }
    let prs: Vec<serde_json::Value> = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed PR list response: {err}")))?;
    match prs.len() {
        0 => {}
        1 => {
            let pr = &prs[0];
            let number = pr.get("number").and_then(|n| n.as_u64()).ok_or_else(|| {
                CaduceusError::Other("malformed PR list response (number)".to_string())
            })?;
            let url = pr
                .get("html_url")
                .and_then(|s| s.as_str())
                .ok_or_else(|| {
                    CaduceusError::Other("malformed PR list response (url)".to_string())
                })?
                .to_string();
            return Ok(crate::github::PullRequest {
                number,
                url,
                reused: true,
            });
        }
        n => {
            return Err(CaduceusError::Other(format!(
                "multiple PRs match head={owner}:{branch} base={base}: {n} found"
            )));
        }
    }
    // 3. POST a new PR.
    let body_json = serde_json::json!({
        "title": title,
        "body": body,
        "head": format!("{owner}:{branch}"),
        "base": base,
    });
    let body_bytes = serde_json::to_vec(&body_json)
        .map_err(|err| CaduceusError::Other(format!("serialize PR body: {err}")))?;
    let resp = client
        .post(&list_path, "application/vnd.github+json", &body_bytes)
        .await?;
    if !matches!(resp.status, 201) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("create pull request failed: {}", resp.status),
        });
    }
    let body: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed PR create response: {err}")))?;
    let number = body
        .get("number")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| CaduceusError::Other("malformed PR create response (number)".to_string()))?;
    let url = body
        .get("html_url")
        .and_then(|s| s.as_str())
        .ok_or_else(|| CaduceusError::Other("malformed PR create response (url)".to_string()))?
        .to_string();
    Ok(crate::github::PullRequest {
        number,
        url,
        reused: false,
    })
}

/// High-level wrapper for the orchestrator.
pub async fn find_or_create_pr_and_finalize(
    ctx: &FinalizeContext,
    client: &crate::github::Client,
    worker_result: &WorkerResult,
) -> CaduceusResult<FinalizeOutput> {
    let pr = find_or_create_pull_request(ctx, client, worker_result).await?;
    let observations = vec![
        "pr_created".to_string(),
        format!("number={}", pr.number),
        format!("url={}", pr.url),
        format!("reused={}", pr.reused),
    ];
    Ok(FinalizeOutput {
        action: FinalizeAction::PrCreated,
        pr_url: Some(pr.url.clone()),
        idempotency_observations: observations,
    })
}

/// URL-encode a string for a query parameter. The
/// implementation is small and conservative: every
/// non-alphanumeric / non-`_-./~` byte becomes `%XX`.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}
