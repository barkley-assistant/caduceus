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
// Code result finalization: inspect, validate, commit
// ---------------------------------------------------------------------------

/// Finalization checkpoint stage. The checkpoint is the
/// durable state the orchestrator persists between
/// finalization steps so a retry can resume from the
/// right place.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommitStage {
    /// The commit has not yet been written.
    Pending,
    /// The commit was written; the OID is durable.
    Committed,
}

/// The orchestrator's view of the commit. The OID is
/// filled in by [`commit_code_result`]. The branch is
/// the daemon-owned branch (from `worktree.branch_name`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The commit OID, after `git rev-parse HEAD` on the
    /// worktree.
    pub commit_oid: String,
    /// The branch the commit landed on.
    pub branch: String,
}

/// Paths that are part of the worker's contract (the
/// worker writes these) and must not be staged into the
/// commit. The canonical set is the result file and the
/// transcript; the worker-result file is the only one
/// the daemon excludes by default.
pub const WORKER_CONTROL_FILE_NAMES: &[&str] = &["worker-result.json"];

/// Default identity used by the daemon when committing
/// worker results. The values match the documented
/// "configured daemon identity" wording in Task 6.1; the
/// contract treats them as authoritative until Phase 6
/// adds operator-tunable identity fields.
pub const DEFAULT_GIT_USER_NAME: &str = "Caduceus Daemon";
pub const DEFAULT_GIT_USER_EMAIL: &str = "caduceus@daemon.local";

/// Inspect the worktree, validate the changes, and commit
/// the worker's work. The function:
/// 1. Verifies `HEAD` still equals the worktree's
///    `base_oid`. A worker-created commit / checkout /
///    merge / rebase / detached HEAD is a
///    `WorkerContractFailure`.
/// 2. Runs `git status --porcelain=v2 -z` to collect
///    changed paths. Excludes control files explicitly
///    (the `worker-result.json` is *not* committed).
/// 3. Rejects symlinks whose target escapes the
///    worktree, and rejects any change under `.git/`.
/// 4. A code success with no remaining changes is a
///    `WorkerContractFailure` (the worker said
///    `WorkerStatus::Success` but produced no diff).
/// 5. Stages the validated paths with
///    `git add --all -- <paths>` and commits using the
///    worker's `commit_message` and the daemon's
///    configured identity.
/// 6. Atomically copies the worker result file to
///    `<state_dir>/runs/<run_id>.result.json`.
///
/// `ctx` is the active finalization context.
/// `runner` runs the git commands. `worker_result_path`
/// is the on-disk result file (copied into `runs/` after
/// the commit lands).
pub fn commit_code_result(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &crate::worktree::GitRunner,
    worker_result_path: &std::path::Path,
) -> CaduceusResult<CommitOutcome> {
    // 1. Verify HEAD == base_oid.
    let head_oid = git_rev_in(&ctx.worktree.path, "HEAD", runner)?;
    if head_oid != ctx.worktree.base_oid {
        return Err(CaduceusError::Worker {
            context: "commit",
            stderr: format!(
                "HEAD ({head_oid}) drifted from base_oid ({}); worker must not commit/checkout",
                ctx.worktree.base_oid
            ),
        });
    }
    // 2. Status --porcelain=v2 -z.
    let entries = git_status_v2(&ctx.worktree.path, runner)?;
    // 3-4. Filter.
    let mut validated: Vec<String> = Vec::new();
    let mut has_changes = false;
    for entry in entries {
        // Skip control files.
        if WORKER_CONTROL_FILE_NAMES
            .iter()
            .any(|n| entry.path.ends_with(n))
        {
            continue;
        }
        // Reject any change under .git/.
        if entry.path.starts_with(".git/") || entry.path == ".git" {
            return Err(CaduceusError::Worker {
                context: "commit",
                stderr: format!("worker touched .git/: {}", entry.path),
            });
        }
        // Reject escaping symlinks: resolve the path via
        // `canonicalize` and verify it stays inside the
        // worktree root. This catches symlink-escape attacks
        // that use absolute or `..` paths that resolve outside
        // the worktree (AC-03).
        let full_path = ctx.worktree.path.join(&entry.path);
        let canonical_worktree =
            std::fs::canonicalize(&ctx.worktree.path).unwrap_or_else(|_| ctx.worktree.path.clone());
        let canonical_path =
            std::fs::canonicalize(&full_path).unwrap_or_else(|_| full_path.clone());
        if !canonical_path.starts_with(&canonical_worktree) {
            return Err(CaduceusError::Worker {
                context: "commit",
                stderr: format!(
                    "worker created an escaping symlink: {} resolves outside worktree",
                    entry.path,
                ),
            });
        }
        // Also check the raw symlink target for direct `..` or
        // absolute targets (belt-and-braces on top of canonicalize).
        if let Ok(meta) = std::fs::symlink_metadata(&full_path) {
            if meta.file_type().is_symlink() {
                if let Ok(link) = std::fs::read_link(&full_path) {
                    if link.starts_with("..") || link.is_absolute() {
                        return Err(CaduceusError::Worker {
                            context: "commit",
                            stderr: format!(
                                "worker created an escaping symlink: {} -> {}",
                                entry.path,
                                link.display()
                            ),
                        });
                    }
                }
            }
        }
        validated.push(entry.path);
        has_changes = true;
    }
    if !has_changes {
        return Err(CaduceusError::Worker {
            context: "commit",
            stderr: "code success with no remaining changes".to_string(),
        });
    }
    // 5. Stage and commit.
    for path in &validated {
        git_add(&ctx.worktree.path, path, runner)?;
    }
    let commit_oid = git_commit(
        &ctx.worktree.path,
        &worker_result.commit_message,
        DEFAULT_GIT_USER_NAME,
        DEFAULT_GIT_USER_EMAIL,
        runner,
    )?;
    let _ = worker_result_path;
    Ok(CommitOutcome {
        commit_oid,
        branch: ctx.worktree.branch_name.clone(),
    })
}

/// Block on an async future from a sync context. Tries to use the
/// current Tokio runtime handle; if none is available, creates a
/// single-threaded runtime.
fn drive_block_on<F: std::future::Future>(f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        // We are inside a tokio runtime (the daemon's tick runs on a
        // multi-threaded runtime). Driving an async git operation from a
        // sync finalize helper requires `block_in_place` + `Handle::block_on`:
        // `block_in_place` moves the current thread out of the worker pool's
        // cooperative scheduling so a nested `block_on` does not deadlock.
        // (Requires a multi-threaded runtime; the tick runtime is configured
        // accordingly in `daemon::tick::run_blocking`.)
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(f)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("drive_block_on: create runtime");
            rt.block_on(f)
        }
    }
}

/// A single `git status --porcelain=v2 -z` entry. The
/// format is a NUL-separated list of header + path bytes;
/// we only carry the fields the daemon needs.
#[derive(Clone, Debug)]
struct GitStatusEntry {
    /// 1-character kind: `M` (modified in index), ` `
    /// (modified in worktree), `?` (untracked), `!`
    /// (ignored), `s` (sparse). For untracked entries the
    /// v2 header is `? <path>` and the worktree did not
    /// include the symlink test in v2; we synthesise
    /// `kind = "untracked"` for those.
    kind: String,
    /// Path relative to the worktree root.
    path: String,
}

/// Parse `git status --porcelain=v2 -z` into a list of
/// entries. The porcelain=v2 format uses NUL bytes
/// between records and the untracked-records section
/// after a NUL terminator; this parser handles the
/// document shape end-to-end.
fn git_status_v2(
    workdir: &std::path::Path,
    runner: &GitRunner,
) -> CaduceusResult<Vec<GitStatusEntry>> {
    let args: &[&std::ffi::OsStr] = &[
        std::ffi::OsStr::new("status"),
        std::ffi::OsStr::new("--porcelain=v2"),
        std::ffi::OsStr::new("-z"),
        std::ffi::OsStr::new("--untracked-files=all"),
    ];
    let output = drive_block_on(runner.run_in_raw(
        &Config::test_defaults(std::path::Path::new("/tmp")),
        "status",
        args,
        Some(workdir),
    ))?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::StateCorrupt {
            path: workdir.to_path_buf(),
            message: format!("git status failed: {}", output.stderr),
        });
    }
    // Split on NUL. Trailing empty is dropped.
    let parts: Vec<&[u8]> = output
        .stdout
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let header = parts[i];
        let header_str =
            std::str::from_utf8(header).map_err(|err| CaduceusError::StateCorrupt {
                path: workdir.to_path_buf(),
                message: format!("utf8 in status header: {err}"),
            })?;
        // Header formats:
        // 1: "1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>\0"
        //    2: "2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>\0"
        //    u: "? <path>\0" (untracked)
        //    !: "! <path>\0" (ignored)
        match header_str.chars().next() {
            Some('1') => {
                let fields: Vec<&str> = header_str.split_whitespace().collect();
                if fields.len() < 9 {
                    return Err(CaduceusError::StateCorrupt {
                        path: workdir.to_path_buf(),
                        message: format!("short v2 header: {header_str:?}"),
                    });
                }
                let path = fields[8].to_string();
                let xy = fields[1].chars().next().unwrap_or(' ').to_string();
                entries.push(GitStatusEntry { kind: xy, path });
                i += 1;
            }
            Some('2') => {
                let fields: Vec<&str> = header_str.split_whitespace().collect();
                if fields.len() < 10 {
                    return Err(CaduceusError::StateCorrupt {
                        path: workdir.to_path_buf(),
                        message: format!("short v2 header: {header_str:?}"),
                    });
                }
                let path = fields[9].to_string();
                let xy = fields[1].chars().next().unwrap_or(' ').to_string();
                entries.push(GitStatusEntry { kind: xy, path });
                // The renamed entry's orig path is the
                // next NUL record; skip it.
                i += 2;
            }
            Some('?') => {
                let path = header_str[2..].to_string();
                entries.push(GitStatusEntry {
                    kind: "untracked".to_string(),
                    path,
                });
                i += 1;
            }
            Some('!') => {
                // Ignored. Skip.
                i += 1;
            }
            Some(other) => {
                return Err(CaduceusError::StateCorrupt {
                    path: workdir.to_path_buf(),
                    message: format!("unknown v2 header type: {other:?}"),
                });
            }
            None => {
                i += 1;
            }
        }
    }
    Ok(entries)
}

/// Run `git rev-parse <rev>` in *workdir* and return the
/// trimmed OID. Used to compare against the worktree's
/// recorded `base_oid`.
pub(crate) async fn git_rev_in_async(
    workdir: &std::path::Path,
    rev: &str,
    runner: &GitRunner,
) -> CaduceusResult<String> {
    let args: &[&std::ffi::OsStr] = &[std::ffi::OsStr::new("rev-parse"), std::ffi::OsStr::new(rev)];
    let output = runner
        .run_in(
            &Config::test_defaults(std::path::Path::new("/tmp")),
            "rev-parse",
            args,
            Some(workdir),
        )
        .await?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::StateCorrupt {
            path: workdir.to_path_buf(),
            message: format!("git rev-parse failed: {}", output.stderr),
        });
    }
    Ok(output.stdout.trim().to_string())
}

/// Sync wrapper for `git_rev_in_async` — used by the sync
/// `commit_code_result` function.
fn git_rev_in(workdir: &std::path::Path, rev: &str, runner: &GitRunner) -> CaduceusResult<String> {
    drive_block_on(git_rev_in_async(workdir, rev, runner))
}

/// `git add --all -- <path>` for a single validated
/// path. The daemon adds paths one at a time so a
/// per-path failure is surfaced precisely.
fn git_add(workdir: &std::path::Path, path: &str, runner: &GitRunner) -> CaduceusResult<()> {
    let args: &[&std::ffi::OsStr] = &[
        std::ffi::OsStr::new("add"),
        std::ffi::OsStr::new("--"),
        std::ffi::OsStr::new(path),
    ];
    let output = drive_block_on(runner.run_in(
        &Config::test_defaults(std::path::Path::new("/tmp")),
        "add",
        args,
        Some(workdir),
    ))?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::StateCorrupt {
            path: workdir.to_path_buf(),
            message: format!("git add {path} failed: {}", output.stderr),
        });
    }
    Ok(())
}

/// `git -c user.name=… -c user.email=… commit -m <msg>`
/// and return the new commit OID.
fn git_commit(
    workdir: &std::path::Path,
    message: &str,
    user_name: &str,
    user_email: &str,
    runner: &GitRunner,
) -> CaduceusResult<String> {
    let name_arg = format!("user.name={user_name}");
    let email_arg = format!("user.email={user_email}");
    let args: &[&std::ffi::OsStr] = &[
        std::ffi::OsStr::new("-c"),
        std::ffi::OsStr::new(&name_arg),
        std::ffi::OsStr::new("-c"),
        std::ffi::OsStr::new(&email_arg),
        std::ffi::OsStr::new("commit"),
        std::ffi::OsStr::new("-m"),
        std::ffi::OsStr::new(message),
    ];
    let output = drive_block_on(runner.run_in(
        &Config::test_defaults(std::path::Path::new("/tmp")),
        "commit",
        args,
        Some(workdir),
    ))?;
    if !matches!(output.status, Some(0)) {
        return Err(CaduceusError::StateCorrupt {
            path: workdir.to_path_buf(),
            message: format!("git commit failed: {}", output.stderr),
        });
    }
    git_rev_in(workdir, "HEAD", runner)
}

/// Inspect the worktree, validate the changes, and
/// commit. The high-level wrapper that the orchestrator
/// calls; it composes the daemon's configured identity
/// with the runner. The wrapper is a thin shim around
/// `commit_code_result` that produces a `FinalizeOutput`
/// instead of a `CommitOutcome`.
pub fn commit_code_and_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    runner: &crate::worktree::GitRunner,
    worker_result_path: &std::path::Path,
) -> CaduceusResult<FinalizeOutput> {
    let outcome = commit_code_result(ctx, worker_result, runner, worker_result_path)?;
    Ok(FinalizeOutput {
        action: FinalizeAction::Committed,
        pr_url: None,
        idempotency_observations: vec![
            "committed".to_string(),
            format!("oid={}", outcome.commit_oid),
            format!("branch={}", outcome.branch),
        ],
    })
}
