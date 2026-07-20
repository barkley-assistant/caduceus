//! Poll GitHub for PR merge status.
//!
//! Uses `GET /repos/{owner}/{repo}/pulls/{number}`. Checks the
//! `merged` boolean and `state` field. Never calls the merge API.

use crate::github::client::Client;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Outcome of polling a PR's merge status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeStatus {
    /// The PR was merged. Carries the merge commit SHA.
    Merged { merge_commit_sha: String },
    /// The PR was closed without being merged.
    ClosedWithoutMerge,
    /// The PR is still open.
    StillOpen,
    /// The PR does not exist (404 or invalid response).
    NotFound,
}

/// Poll the GitHub API for a pull request's merge status.
///
/// Uses `GET /repos/{owner}/{repo}/pulls/{number}`. The
/// response body contains `merged: bool` and `state: String`
/// fields. This function never calls the merge API.
pub async fn poll_pr_merge_status(
    client: &Client,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> CaduceusResult<MergeStatus> {
    let path = format!("/repos/{owner}/{repo}/pulls/{pr_number}");
    let resp = client.get(&path, "application/vnd.github+json").await?;

    if resp.status == 404 {
        return Ok(MergeStatus::NotFound);
    }
    if !matches!(resp.status, 200) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("poll pull request failed: {}", resp.status),
        });
    }

    let body: serde_json::Value = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed PR response: {err}")))?;

    let merged = body
        .get("merged")
        .and_then(|m| m.as_bool())
        .unwrap_or(false);
    let state = body
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");

    if merged {
        let sha = body
            .get("merge_commit_sha")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(MergeStatus::Merged {
            merge_commit_sha: sha,
        });
    }

    match state {
        "closed" => Ok(MergeStatus::ClosedWithoutMerge),
        "open" => Ok(MergeStatus::StillOpen),
        _ => Ok(MergeStatus::StillOpen), // default to still open for safety
    }
}
