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
// Push: idempotent, credential-scoped, remote-aware
// ---------------------------------------------------------------------------

/// Outcome of the push step. The `PushOutcome` is the
/// orchestrator's view: the local branch is durable on
/// the remote, the daemon branch name is canonical, and
/// the `mode` records which of the four contract cases
/// was applied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PushMode {
    /// The remote did not have the ref; `git push` created it.
    Created,
    /// The remote already had the same OID; no work needed.
    AlreadyCurrent,
    /// The remote had an ancestor; `git push` fast-forwarded.
    FastForward,
    /// The remote had a non-ancestor; the orchestrator
    /// reports this as a `CaduceusError::PushCollision`.
    Diverged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushOutcome {
    pub mode: PushMode,
    pub branch: String,
    pub remote_oid: String,
}

/// Push the daemon branch to its remote, idempotently.
///
/// The function:
/// 1. Queries the remote for the current ref using
///    `git ls-remote --heads origin <branch>`. Missing
///    output means the ref is absent on the remote.
/// 2. If absent: `git push origin <local>` to create it.
///    If present and equal: no-op success.
///    If present and an ancestor: `git push origin
///    <local>` fast-forwards it.
///    If present and not an ancestor: returns
///    `CaduceusError::PushCollision`.
/// 3. The push runs through the runner's
///    `git_timeout_seconds`; a hanging remote is killed
///    via the runner's process-group kill (the runner
///    already implements the cancellation contract from
///    Task 4.1).
/// 4. The PAT is **never** placed in arguments, URLs, or
///    environment. The runner's credential allowlist
///    handles authentication; the function only passes
///    the branch ref name.
///
/// `ctx` is the active finalization context. The
/// `remote_url` field of `ctx.repository` is the URL git
/// pushes to; for v0.1 a `file://` URL is used in
/// tests, and a real `https://` URL is used in
/// production with the operator's credential helper.
pub async fn push_daemon_branch(
    ctx: &FinalizeContext,
    runner: &crate::worktree::GitRunner,
) -> CaduceusResult<PushOutcome> {
    let branch = ctx.worktree.branch_name.clone();
    let local_oid = git_rev_in_async(&ctx.worktree.path, "HEAD", runner).await?;
    let remote_url = ctx.repository.remote_url.as_str();
    // 1. Query the remote for the current ref.
    let remote_oid = match ls_remote_branch(remote_url, &branch, runner).await? {
        None => {
            // 2a. Absent — create the ref.
            run_push(
                remote_url,
                &branch,
                &local_oid,
                false,
                &ctx.worktree.path,
                runner,
            )
            .await?;
            // Persist immediately per contract.
            PushOutcome {
                mode: PushMode::Created,
                branch,
                remote_oid: local_oid,
            }
        }
        Some(remote_oid) if remote_oid == local_oid => {
            // 2b. Already current.
            PushOutcome {
                mode: PushMode::AlreadyCurrent,
                branch,
                remote_oid,
            }
        }
        Some(remote_oid) => {
            // Determine ancestor / non-ancestor. The
            // remote is the proposed old-tip; the local
            // is the proposed new-tip. We treat the
            // remote as an ancestor iff `local_oid` is
            // reachable from `remote_oid`.
            if is_ancestor(&ctx.worktree.path, &remote_oid, &local_oid, runner).await? {
                // 2c. Fast-forward.
                run_push(
                    remote_url,
                    &branch,
                    &local_oid,
                    false,
                    &ctx.worktree.path,
                    runner,
                )
                .await?;
                PushOutcome {
                    mode: PushMode::FastForward,
                    branch,
                    remote_oid: local_oid,
                }
            } else {
                // 2d. Diverged — terminal collision. We do
                // *not* force-push; the orchestrator
                // surfaces the collision so the operator
                // can reconcile the branch manually.
                return Err(CaduceusError::PushCollision {
                    branch,
                    remote_oid,
                    local_oid,
                });
            }
        }
    };
    Ok(remote_oid)
}

/// High-level wrapper that returns a `FinalizeOutput`
/// for the orchestrator.
pub async fn push_and_finalize(
    ctx: &FinalizeContext,
    runner: &crate::worktree::GitRunner,
) -> CaduceusResult<FinalizeOutput> {
    let outcome = push_daemon_branch(ctx, runner).await?;
    Ok(FinalizeOutput {
        action: FinalizeAction::Pushed,
        pr_url: None,
        idempotency_observations: vec![
            "pushed".to_string(),
            format!("branch={}", outcome.branch),
            format!("oid={}", outcome.remote_oid),
            format!("mode={:?}", outcome.mode),
        ],
    })
}

/// `git ls-remote --heads <remote> <branch>`. Returns the
/// remote OID if the ref is present, or `None` if
/// absent. Errors are wrapped in `CaduceusError::Push`
/// with the redacted stderr.
pub(crate) async fn ls_remote_branch(
    remote_url: &str,
    branch: &str,
    runner: &crate::worktree::GitRunner,
) -> CaduceusResult<Option<String>> {
    let args_vec: Vec<std::ffi::OsString> = vec![
        "ls-remote".into(),
        "--heads".into(),
        remote_url.into(),
        branch.into(),
    ];
    let borrowed: Vec<&std::ffi::OsStr> = args_vec.iter().map(|s| s.as_os_str()).collect();
    let output = runner
        .run_in(
            &Config::test_defaults(std::path::Path::new("/tmp")),
            "ls-remote",
            &borrowed,
            None,
        )
        .await?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::Push {
            context: "ls-remote",
            stderr: crate::infra::error::scrub(&output.stderr),
        });
    }
    let stdout = &output.stdout;
    // The output is a single line `<oid>\trefs/heads/<branch>`.
    for line in stdout.lines() {
        if line.contains(&format!("refs/heads/{branch}")) {
            if let Some(oid) = line.split_whitespace().next() {
                return Ok(Some(oid.to_string()));
            }
        }
    }
    Ok(None)
}

/// `git push <remote> <local>:<remote>` (or
/// `git push <remote> <local>` when `force` is `false`).
/// The PAT is never in the URL or any argument; the
/// runner's credential allowlist is the only auth path.
async fn run_push(
    remote_url: &str,
    local_branch: &str,
    local_oid: &str,
    force: bool,
    workdir: &std::path::Path,
    runner: &crate::worktree::GitRunner,
) -> CaduceusResult<()> {
    let refspec = if force {
        format!("+{local_branch}:refs/heads/{local_branch}")
    } else {
        format!("{local_branch}:refs/heads/{local_branch}")
    };
    let args_vec: Vec<std::ffi::OsString> = vec!["push".into(), remote_url.into(), refspec.into()];
    let borrowed: Vec<&std::ffi::OsStr> = args_vec.iter().map(|s| s.as_os_str()).collect();
    let output = runner
        .run_in(
            &Config::test_defaults(std::path::Path::new("/tmp")),
            "push",
            &borrowed,
            Some(workdir),
        )
        .await?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::Push {
            context: "push",
            stderr: crate::infra::error::scrub(&output.stderr),
        });
    }
    let _ = local_oid; // kept for logging symmetry
    Ok(())
}

/// `git merge-base --is-ancestor <remote_oid> <local_oid>`.
/// True iff `<local_oid>` is reachable from `<remote_oid>`
/// (i.e. the remote is an ancestor of the local and the
/// push is a fast-forward).
async fn is_ancestor(
    workdir: &std::path::Path,
    remote_oid: &str,
    local_oid: &str,
    runner: &GitRunner,
) -> CaduceusResult<bool> {
    let args: &[&std::ffi::OsStr] = &[
        std::ffi::OsStr::new("merge-base"),
        std::ffi::OsStr::new("--is-ancestor"),
        std::ffi::OsStr::new(remote_oid),
        std::ffi::OsStr::new(local_oid),
    ];
    let output = runner
        .run_in(
            &Config::test_defaults(std::path::Path::new("/tmp")),
            "merge-base",
            args,
            Some(workdir),
        )
        .await?;
    Ok(output.status == Some(0))
}
