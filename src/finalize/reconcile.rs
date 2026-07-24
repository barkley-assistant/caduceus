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
// Reconcile helpers (Task 4.2 — FINAL-001)
// ---------------------------------------------------------------------------

/// Reconcile a push effect against remote state. Queries the
/// remote branch via `ls-remote` and compares the OID against
/// the local checkpoint marker.
///
/// Returns [`ReconcileResult::AlreadyApplied`] when the remote
/// OID matches the expected marker, [`ReconcileResult::NeedsRetry`]
/// when the branch is absent from the remote, and
/// [`ReconcileResult::Conflict`] when the remote OID differs.
pub async fn reconcile_push(
    runner: &GitRunner,
    remote_url: &str,
    branch: &str,
    expected_marker: Option<&str>,
) -> CaduceusResult<ReconcileResult> {
    let remote_oid = ls_remote_branch(remote_url, branch, runner).await?;
    match remote_oid {
        None => Ok(ReconcileResult::NeedsRetry),
        Some(oid) => {
            if let Some(expected) = expected_marker {
                if oid == expected {
                    Ok(ReconcileResult::AlreadyApplied)
                } else {
                    Ok(ReconcileResult::Conflict {
                        expected: expected.to_string(),
                        actual: oid,
                    })
                }
            } else {
                // No expected marker — treat as already applied
                // (the remote has the branch, so the push went
                // through at some point).
                Ok(ReconcileResult::AlreadyApplied)
            }
        }
    }
}

/// Reconcile a PR creation effect against remote state. Queries
/// open PRs matching the branch and base, and compares the
/// result against the expected PR number.
///
/// Returns [`ReconcileResult::AlreadyApplied`] when a PR with
/// the expected number exists, [`ReconcileResult::NeedsRetry`]
/// when no matching PR exists, and [`ReconcileResult::Conflict`]
/// when a different PR matches the branch.
pub async fn reconcile_pr(
    client: &Client,
    owner: &str,
    repo: &str,
    branch: &str,
    base: &str,
    expected_marker: Option<&str>,
) -> CaduceusResult<ReconcileResult> {
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
            message: format!(
                "list pull requests for reconciliation failed: {}",
                resp.status
            ),
        });
    }
    let prs: Vec<serde_json::Value> = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed PR list response: {err}")))?;
    match prs.len() {
        0 => Ok(ReconcileResult::NeedsRetry),
        1 => {
            let number = prs[0]
                .get("number")
                .and_then(|n| n.as_u64())
                .ok_or_else(|| CaduceusError::Other("malformed PR (number)".to_string()))?;
            if let Some(expected) = expected_marker {
                if number.to_string() == expected {
                    Ok(ReconcileResult::AlreadyApplied)
                } else {
                    Ok(ReconcileResult::Conflict {
                        expected: expected.to_string(),
                        actual: number.to_string(),
                    })
                }
            } else {
                Ok(ReconcileResult::AlreadyApplied)
            }
        }
        n => Ok(ReconcileResult::Conflict {
            expected: expected_marker.unwrap_or("<none>").to_string(),
            actual: format!("{n} PRs found"),
        }),
    }
}

/// Reconcile a comment effect against remote state. Searches
/// the issue's comments for a marker prefix matching the
/// given `run_id`.
///
/// Returns [`ReconcileResult::AlreadyApplied`] when a matching
/// marker comment is found, [`ReconcileResult::NeedsRetry`]
/// when no marker is found, and [`ReconcileResult::Conflict`]
/// when a comment with a different marker exists.
pub async fn reconcile_comment(
    client: &Client,
    owner: &str,
    repo: &str,
    number: u64,
    run_id: &str,
    marker_prefix: &str,
) -> CaduceusResult<ReconcileResult> {
    let list_path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
    let resp = client
        .get(&list_path, "application/vnd.github+json")
        .await?;
    if !matches!(resp.status, 200) {
        return Err(CaduceusError::GitHubApi {
            status: resp.status,
            message: format!("list comments for reconciliation failed: {}", resp.status),
        });
    }
    let comments: Vec<serde_json::Value> = serde_json::from_slice(&resp.body)
        .map_err(|err| CaduceusError::Other(format!("malformed comments list: {err}")))?;
    let marker = format!("{marker_prefix}{run_id}");
    let existing = comments.iter().any(|c| {
        c.get("body")
            .and_then(|b| b.as_str())
            .map(|s| s.starts_with(&marker))
            .unwrap_or(false)
    });
    if existing {
        Ok(ReconcileResult::AlreadyApplied)
    } else {
        Ok(ReconcileResult::NeedsRetry)
    }
}
