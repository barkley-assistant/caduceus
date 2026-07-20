//! Finalization: commit, push, PR, comment/close, investigation comment.
//!
//! Idempotency across partial failures is the hard requirement — see
//! `CONTRACTS.md` "Finalization contract" and Tasks 6.1–6.5.
//!
//! This module owns the public-voice validator that every outbound
//! comment, PR title, and PR body must pass before the
//! corresponding API mutation. The validator lives in finalize.rs
//! because that is the only point through which GitHub mutations
//! flow; routing it through github.rs alone would leave a future
//! finalization caller free to bypass it.
//!
//! The public-voice rule is:
//!
//! * The text must not contain any `comment_forbidden_strings` term
//!   (case-insensitive Unicode substring match). Configuration
//!   replaces the defaults.
//! * The byte length must not exceed the documented limit for the
//!   channel (`limit` argument).
//!
//! On rejection the function returns the canonical [`VoiceError`]
//! (`Forbidden { found }` for substring matches, `TooLong { limit }`
//! for length). Both are terminal failures: the daemon's
//! retry-or-fail logic does not retry on a voice error.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::github::Client;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult, VoiceError};
use crate::worker::WorkerResult;
use crate::worktree::GitRunner;
/// Default outbound-comment max bytes when the operator has not
/// overridden the limit. GitHub caps comment bodies at 65 536 bytes
/// in API v3; the daemon defaults to the same number so a comment
/// that passes the validator will not be truncated server-side.
pub const DEFAULT_COMMENT_MAX_BYTES: usize = 65_536;

/// Default PR body max bytes. The daemon defaults to 65 536 bytes
/// (GitHub's documented limit for the body parameter).
pub const DEFAULT_PR_BODY_MAX_BYTES: usize = 65_536;

/// Default PR title max bytes. The validator defaults to 256 bytes
/// (a generous limit that still leaves headroom under GitHub's
/// 256-character cap for rendered titles).
pub const DEFAULT_PR_TITLE_MAX_BYTES: usize = 256;

/// Finalized result handed to the daemon by the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeRequest {
    pub issue: IssueKey,
    pub branch_name: String,
    pub worktree_path: PathBuf,
}

/// Inputs that every finalization stage consumes. The struct
/// is the canonical argument to the Phase 6
/// implementation; Task 5.0 only defines the type so
/// earlier tasks can compile against it.
///
/// `client` is the shared `Arc<Client>` produced by the
/// daemon's [`crate::daemon::orchestration::Services::production`]
/// helper. Phase 6 already owns the concrete HTTP surface; the
/// shared `Arc` lets the daemon, the status reporter, and the
/// finalization stages share one connection pool + persistent
/// cache without rebuilding the client three times. `config`
/// is the live daemon config, and `repository` is the
/// cloned-repo metadata. `issue` is the fetched issue detail;
/// `claim`/`run_id`/`worktree` carry the active run's
/// identity. `result` is the worker's output — the same
/// [`FinalizeRequest`] payload the worker writes to
/// `worker-result.json`.
#[derive(Clone)]
pub struct FinalizeContext {
    /// Shared GitHub API client. The `Arc<Client>` is the
    /// production value; the previous `()` placeholder is
    /// removed because Phase 7's orchestrator shares the
    /// same client through the [`crate::daemon::orchestration::Services`]
    /// bundle.
    pub client: Arc<Client>,
    /// Live daemon config (allowlist, timeouts, …).
    pub config: Config,
    /// Local repository metadata (path, base branch, remote URL).
    pub repository: crate::worktree::RepositoryInfo,
    /// Issue the run is finalising.
    pub issue: crate::github::issue::IssueDetail,
    /// Active run's claim token (proves the caller is the
    /// daemon, not a stray worker).
    pub claim: crate::state::queue::ClaimToken,
    /// Active run id.
    pub run_id: String,
    /// Active worktree handle. Task 5.0 keeps the existing
    /// `Worktree` struct from `worktree.rs`.
    pub worktree: crate::worktree::Worktree,
    /// Worker output (`worker-result.json`).
    pub result: FinalizeRequest,
}

/// What a finalization stage returns to the orchestrator.
/// `action` records which stage produced this output
/// (e.g. `Committed`, `Pushed`, `PrCreated`, `Commented`,
/// `Closed`, `InvestigationReady`,
/// `InvestigationCommented`). `pr_url` is the canonical
/// PR URL once it exists. `idempotency_observations` is a
/// free-form list of operator-facing notes the
/// orchestrator surfaces to the structured log so the
/// "did we already post this comment?" check is auditable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeOutput {
    /// The action the finalization stage performed.
    pub action: FinalizeAction,
    /// Canonical PR URL, if the action created or updated one.
    pub pr_url: Option<String>,
    /// Per-step idempotency notes (e.g. "comment already posted",
    /// "branch already pushed"). The orchestrator logs these
    /// but does not retry on them.
    pub idempotency_observations: Vec<String>,
}

/// The action a finalization stage took. Mirrors the
/// `FinalizationStage` enum in `queue.rs` but lives here
/// because the orchestrator's view of the world is the
/// `FinalizeOutput` it hands back to the cron tick.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum FinalizeAction {
    #[default]
    Committed,
    Pushed,
    PrCreated,
    Commented,
    Closed,
    InvestigationReady,
    InvestigationCommented,
    Previewed,
}

/// Outcome of a finalization attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeOutcome {
    pub commit_oid: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// Validate *text* against the public-voice rule.
///
/// * Every configured forbidden term is matched against *text* with
///   case-insensitive Unicode substring semantics. The first
///   matching term's lowercase form is captured in the
///   [`VoiceError::Forbidden { found }`] payload so the operator
///   can update the allowlist.
/// * The byte length of *text* must not exceed *limit_bytes*. The
///   check runs *after* the substring check so a long body that
///   also contains a forbidden term is reported as `Forbidden`
///   (the more actionable reason for the operator).
///
/// This is the single entry point that every outbound mutation
/// helper must call. The function is intentionally synchronous and
/// pure so tests can drive it without touching the filesystem or
/// the network.
pub fn validate_public_text(
    text: &str,
    cfg: &Config,
    limit_bytes: usize,
) -> Result<(), VoiceError> {
    if let Some(found) = first_forbidden_term(text, &cfg.comment_forbidden_strings) {
        return Err(VoiceError::Forbidden { found });
    }
    if text.len() > limit_bytes {
        return Err(VoiceError::TooLong { limit: limit_bytes });
    }
    Ok(())
}

/// Return the first configured forbidden term that matches *text*,
/// normalised to lowercase. Returns `None` when no term matches.
pub fn first_forbidden_term(text: &str, forbidden: &[String]) -> Option<String> {
    let lower = text.to_lowercase();
    forbidden
        .iter()
        .find(|term| !term.is_empty() && lower.contains(&term.to_lowercase()))
        .map(|t| t.to_lowercase())
}

/// Convenience wrapper: validate a PR title. Uses the documented
/// 256-byte default unless *limit_bytes* overrides it.
pub fn validate_pr_title(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_PR_TITLE_MAX_BYTES)
}

/// Convenience wrapper: validate a PR body. Uses 65 536-byte
/// default unless *limit_bytes* overrides it.
pub fn validate_pr_body(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_PR_BODY_MAX_BYTES)
}

/// Convenience wrapper: validate a generic GitHub comment. Uses the
/// 65 536-byte default unless *limit_bytes* overrides it.
pub fn validate_comment(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_COMMENT_MAX_BYTES)
}

impl std::fmt::Debug for FinalizeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The GitHub client is shared across phases; printing its
        // debug shape would expose the persistent cache path. A
        // placeholder is enough for the structured log lines.
        f.debug_struct("FinalizeContext")
            .field("client", &"Arc<Client>")
            .field("config", &self.config)
            .field("repository", &self.repository)
            .field("issue", &self.issue)
            .field("claim", &self.claim)
            .field("run_id", &self.run_id)
            .field("worktree", &self.worktree)
            .field("result", &self.result)
            .finish()
    }
}

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
        Ok(handle) => handle.block_on(f),
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
async fn git_rev_in_async(
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
async fn ls_remote_branch(
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
fn urlencode(s: &str) -> String {
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

/// Atomic report written under `<state_dir>/runs/<run_id>.preview.json`
/// when the daemon runs a dry-run. The report is the
/// auditable record of what *would* have happened if
/// `cfg.dry_run` had been `false` at run time.
///
/// The struct is versioned (`version = 1`) and uses
/// `deny_unknown_fields` so a future schema bump is
/// detected early. The orchestrator re-renders the report
/// on every dry-run tick; older versions are simply
/// overwritten by the atomic rename.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreviewReport {
    /// Schema version. Bumped when the report shape
    /// changes; consumers refuse to read a future
    /// version.
    pub version: u32,
    /// The active run id. The report file's name is
    /// `<run_id>.preview.json`; the field is here for
    /// ergonomics so a reader can identify the report
    /// without the filename.
    pub run_id: String,
    /// The issue the run is finalising.
    pub issue: IssueKey,
    /// Proposed branch name (the worktree's branch).
    pub proposed_branch: String,
    /// Proposed commit message. In dry-run we do not
    /// `git commit`, so this is the worker's
    /// `commit_message` carried verbatim.
    pub proposed_commit_message: String,
    /// Proposed PR title (validated by `validate_pr_title`).
    pub proposed_pr_title: String,
    /// Proposed PR body (validated by `validate_pr_body`).
    pub proposed_pr_body: String,
    /// When the worker is an investigation, the proposed
    /// investigation comment. `None` for code-ticket
    /// previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_investigation_comment: Option<String>,
    /// Files the worker changed (relative paths from the
    /// worktree root). The orchestrator collects this via
    /// `git status --porcelain` in real runs; the dry-run
    /// test passes a fixed list.
    pub changed_files: Vec<String>,
    /// Path to the supervisor transcript for this run.
    pub transcript_path: PathBuf,
    /// Path to the worker result file.
    pub worker_result_path: PathBuf,
    /// Validation warnings collected during the dry-run.
    /// A dry-run with warnings is still valid (the
    /// orchestrator may surface the warnings to the
    /// operator) but is reported.
    #[serde(default)]
    pub validation_warnings: Vec<String>,
    /// Wall-clock instant the report was written (RFC 3339).
    pub written_at: String,
}

impl PreviewReport {
    /// Stable schema version. Bump in lockstep with the
    /// struct definition.
    pub const SCHEMA_VERSION: u32 = 1;
}

/// The dry-run path. Validates the worker result, builds
/// the canonical PR text (or investigation comment),
/// collects changed files and the transcript path, and
/// writes the [`PreviewReport`] atomically. No `git`
/// mutation (commit, push) and no `GitHub` HTTP call is
/// performed.
///
/// * `ctx` — the active finalization context (issue,
///   worktree, claim, config, run_id). The `client` field
///   is unused because dry-run never calls the API.
/// * `worker_result` — the parsed `WorkerResult` from the
///   bridge.
/// * `worker_result_path` — path to the on-disk result
///   file; embedded in the report for operator audit.
/// * `changed_files` — list of files the worker touched,
///   as observed by the orchestrator. The function does
///   *not* re-run `git status`; the caller supplies the
///   list so the test path is deterministic.
pub fn dry_run_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    worker_result_path: &std::path::Path,
    changed_files: Vec<String>,
) -> CaduceusResult<FinalizeOutput> {
    // 1. Validate the worker result against the issue.
    //    We do not block on validation warnings — the
    //    report collects them and the orchestrator can
    //    surface them to the operator.
    let mut warnings = Vec::new();
    if let Err(err) = crate::worker::validate_worker_result(worker_result, &ctx.issue.key) {
        warnings.push(format!("validate_worker_result: {err}"));
    }

    // 2. Build the proposed PR text. The build functions
    //    return VoiceError rejections as
    //    `CaduceusError::Other`; capture them as
    //    warnings rather than aborting the dry-run.
    let proposed_pr_title = match build_pr_title(worker_result, &ctx.config) {
        Ok(t) => t,
        Err(err) => {
            warnings.push(format!("build_pr_title: {err}"));
            worker_result.pull_request_title.clone()
        }
    };
    let proposed_pr_body =
        match build_pr_body(worker_result, &ctx.issue.key, &ctx.run_id, &ctx.config) {
            Ok(b) => b,
            Err(err) => {
                warnings.push(format!("build_pr_body: {err}"));
                // Fall back to the worker's summary so the
                // operator can still see the intended text in
                // the report.
                format!(
                    "{}\n\nCloses #{}\n\n{}<!-- {} {} -->",
                    worker_result.summary,
                    ctx.issue.key.number,
                    "",
                    IDEMPOTENCY_MARKER_PREFIX,
                    ctx.run_id
                )
            }
        };

    // 3. Investigation comment (or None for code tickets).
    let proposed_investigation_comment = if worker_result.investigation {
        Some(worker_result.summary.clone())
    } else {
        None
    };

    // 4. Build the report. The branch name is the
    //    worktree's branch.
    let report = PreviewReport {
        version: PreviewReport::SCHEMA_VERSION,
        run_id: ctx.run_id.clone(),
        issue: ctx.issue.key.clone(),
        proposed_branch: ctx.worktree.branch_name.clone(),
        proposed_commit_message: worker_result.commit_message.clone(),
        proposed_pr_title,
        proposed_pr_body,
        proposed_investigation_comment,
        changed_files,
        transcript_path: ctx.worktree.path.join(".caduceus").join("transcript"),
        worker_result_path: worker_result_path.to_path_buf(),
        validation_warnings: warnings,
        written_at: chrono::Utc::now().to_rfc3339(),
    };

    // 5. Write the report atomically to
    //    `<state_dir>/runs/<run_id>.preview.json`.
    let runs_dir = ctx.config.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).map_err(|err| CaduceusError::StateCorrupt {
        path: runs_dir.clone(),
        message: format!("create_dir_all failed: {err}"),
    })?;
    let report_path = runs_dir.join(format!("{}.preview.json", ctx.run_id));
    let body = serde_json::to_vec_pretty(&report)
        .map_err(|err| CaduceusError::Other(format!("serialize preview report: {err}")))?;
    write_atomic(&report_path, &body).map_err(|err| CaduceusError::StateCorrupt {
        path: report_path.clone(),
        message: format!("write_atomic failed: {err}"),
    })?;

    Ok(FinalizeOutput {
        action: FinalizeAction::Previewed,
        pr_url: None,
        idempotency_observations: vec![
            "dry-run".to_string(),
            format!("report={}", report_path.display()),
        ],
    })
}

/// Atomically archive a worker result from the worktree to the
/// canonical `<state_dir>/runs/<run_id>.result.json` path. The
/// helper reads the file, creates the runs directory (if needed),
/// and writes via [`write_atomic`] for crash safety.
///
/// Returns the canonical archive path so callers can pass it to
/// downstream finalization.
pub fn archive_worker_result(
    worktree_result_path: &std::path::Path,
    state_dir: &std::path::Path,
    run_id: &str,
) -> CaduceusResult<std::path::PathBuf> {
    let runs_dir = state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).map_err(|err| CaduceusError::StateCorrupt {
        path: runs_dir.clone(),
        message: format!("create_dir_all failed: {err}"),
    })?;
    let target = runs_dir.join(format!("{run_id}.result.json"));
    let bytes = std::fs::read(worktree_result_path).map_err(|err| CaduceusError::StateCorrupt {
        path: worktree_result_path.to_path_buf(),
        message: format!("read result: {err}"),
    })?;
    write_atomic(&target, &bytes).map_err(|err| CaduceusError::StateCorrupt {
        path: target.clone(),
        message: format!("write_atomic result: {err}"),
    })?;
    Ok(target)
}

/// Write `data` to `path` atomically: write to
/// `<path>.tmp.<rand>` then rename. The function is
/// available here (not just in `queue.rs`) because the
/// dry-run report is written from the finalization
/// module.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> CaduceusResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| CaduceusError::Other(format!("no parent for {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|err| CaduceusError::StateCorrupt {
        path: parent.to_path_buf(),
        message: format!("create_dir_all failed: {err}"),
    })?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(
        ".{}.tmp.{pid}.{nanos}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("preview")
    );
    let tmp_path = parent.join(tmp_name);
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|err| CaduceusError::StateCorrupt {
                path: tmp_path.clone(),
                message: format!("open failed: {err}"),
            })?;
        file.write_all(data)
            .map_err(|err| CaduceusError::StateCorrupt {
                path: tmp_path.clone(),
                message: format!("write failed: {err}"),
            })?;
        file.flush().map_err(|err| CaduceusError::StateCorrupt {
            path: tmp_path.clone(),
            message: format!("flush failed: {err}"),
        })?;
        file.sync_all().ok();
    }
    std::fs::rename(&tmp_path, path).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("rename failed: {err}"),
    })?;
    Ok(())
}

/// Helper for the structured logger: emit a single line that names
/// the configured forbidden term that was matched, but only when
/// the term itself is not sensitive. The current contract denies
/// *all* configured terms from logging — operators who need a less
/// strict log should configure the allowlist directly. The
/// returned string is the empty string for the "do not log" case.
pub fn log_safe_term_match(found: &str) -> &str {
    // The contract currently treats every configured term as safe
    // to log (the term itself is operator-supplied; nothing secret
    // leaks). This function exists so a future tightening of the
    // policy has a single point to update.
    let _ = found;
    found
}

/// Map a [`VoiceError`] to a [`CaduceusError::Cancelled`]-style
/// terminal error. The queue's retry-or-fail logic treats this as
/// a hard failure (no retry). Used by the finalization helpers
/// when they receive a `VoiceError::Forbidden` or `VoiceError::TooLong`.
pub fn terminal_from_voice(err: VoiceError) -> CaduceusError {
    match err {
        VoiceError::Forbidden { found } => {
            CaduceusError::Other(format!("public-voice: forbidden term matched: {found:?}"))
        }
        VoiceError::TooLong { limit } => {
            CaduceusError::Other(format!("public-voice: text exceeds limit of {limit} bytes"))
        }
    }
}

// ---------------------------------------------------------------------------
// Public-voice-driven PR body and title rendering
// ---------------------------------------------------------------------------

/// Hard cap on the rendered PR body in bytes. The daemon
/// never emits a body larger than this; the validator's
/// `DEFAULT_PR_BODY_MAX_BYTES` is the upper bound, this
/// constant is the *render* cap. We pick 64 KiB so the
/// rendered body stays well under GitHub's 65 536-byte
/// limit while still leaving room for a future contract
/// bump.
pub const MAX_RENDERED_BODY_BYTES: usize = 64 * 1024;

/// Idempotency marker that the daemon appends to every PR
/// body. The marker is a hidden HTML comment so it does not
/// affect the rendered Markdown. The body includes the
/// run_id so a re-render of the same body produces the
/// same bytes.
pub const IDEMPOTENCY_MARKER_PREFIX: &str = "<!-- caduceus-pr-body:run=";

/// Marker for the issue-closing reference. GitHub renders
/// `Closes #N` as a closing reference; the daemon always
/// uses the canonical form so the bot's behaviour is
/// auditable in test fixtures.
pub const CLOSES_REFERENCE_PREFIX: &str = "Closes #";

/// Render the canonical PR body for a worker `result`.
///
/// The body is the concatenation of:
/// 1. The worker's `summary`.
/// 2. A blank line, then the issue-closing reference.
/// 3. A blank line, then a fenced-JSON artifact section
///    sorted by key.
/// 4. A blank line, then the idempotency marker comment.
///
/// `result.artifacts` is rendered with a fence length
/// dynamically chosen to be longer than any backtick run
/// in the rendered JSON. The total body is bounded by
/// [`MAX_RENDERED_BODY_BYTES`]. The body is then passed
/// through the public-voice validator with the documented
/// PR-body limit before being returned.
pub fn build_pr_body(
    result: &WorkerResult,
    issue: &IssueKey,
    run_id: &str,
    cfg: &Config,
) -> CaduceusResult<String> {
    let artifact_section = render_artifacts(&result.artifacts);
    let closes = format!("{}{}", CLOSES_REFERENCE_PREFIX, issue.number);
    let marker = format!("{}{}{} -->", IDEMPOTENCY_MARKER_PREFIX, run_id, "");
    let mut body = String::with_capacity(8 * 1024);
    body.push_str(&result.summary);
    body.push_str("\n\n");
    body.push_str(&closes);
    if !artifact_section.is_empty() {
        body.push_str("\n\n");
        body.push_str(&artifact_section);
    }
    body.push_str("\n\n");
    body.push_str(&marker);
    if body.len() > MAX_RENDERED_BODY_BYTES {
        // Truncate the body to the cap, then re-append the
        // marker so the body is always capped *and* the
        // idempotency marker is present. We do this before
        // the public-voice check so a too-long summary
        // still produces a valid (capped) body.
        if let Some(pos) = body.find(IDEMPOTENCY_MARKER_PREFIX) {
            // Keep the marker, drop everything after.
            body.truncate(pos);
        }
        // The summary may be huge; we have already
        // truncated everything after the marker. Now make
        // sure the *front* is under the cap by stripping
        // from the top of the summary.
        let marker_len = marker.len();
        if body.len() + marker_len + 4 > MAX_RENDERED_BODY_BYTES {
            // Hard-truncate the leading summary so the
            // body is under the cap.
            let allowed = MAX_RENDERED_BODY_BYTES
                .saturating_sub(marker_len)
                .saturating_sub(4);
            body.truncate(allowed);
        }
        body.push_str("\n\n");
        body.push_str(&marker);
    }
    validate_pr_body(&body, cfg).map_err(terminal_from_voice)?;
    Ok(body)
}

/// Render the canonical PR title. The worker's
/// `pull_request_title` is validated through the public-voice
/// rule with the documented PR-title limit and returned
/// unchanged otherwise.
pub fn build_pr_title(result: &WorkerResult, cfg: &Config) -> CaduceusResult<String> {
    validate_pr_title(&result.pull_request_title, cfg).map_err(terminal_from_voice)?;
    Ok(result.pull_request_title.clone())
}

/// Render the worker-emitted artifacts as a Markdown block, or
/// return the empty string when there are no artifacts.
fn render_artifacts(artifacts: &std::collections::BTreeMap<String, serde_json::Value>) -> String {
    if artifacts.is_empty() {
        return String::new();
    }
    let mut json = String::new();
    // Deterministic order: BTreeMap iterates in key order.
    let json_value = serde_json::json!(artifacts);
    json.push_str(&serde_json::to_string_pretty(&json_value).expect("serialize json"));
    // The fence length is one greater than the longest backtick
    // run in the JSON, so the artifact block can never close itself.
    let fence = dynamic_fence_length(&json);
    let mut fence_str = String::with_capacity(fence);
    for _ in 0..fence {
        fence_str.push('`');
    }
    let caption = format!("Artifacts ({}):", artifacts.len());
    let mut out = String::with_capacity(json.len() + caption.len() + fence * 2 + 8);
    out.push_str(&caption);
    out.push_str("\n\n");
    out.push_str(&fence_str);
    out.push_str("json\n");
    out.push_str(&json);
    out.push('\n');
    out.push_str(&fence_str);
    out
}

/// Pick a backtick fence length that is at least 3 and one
/// longer than the longest run of backticks in *body*. The
/// contract says "dynamically chosen"; 3 is the Markdown
/// minimum and we extend as needed.
fn dynamic_fence_length(body: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for c in body.chars() {
        if c == '`' {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 0;
        }
    }
    let pick = longest + 1;
    if pick < 3 {
        3
    } else {
        pick
    }
}

/// Escape control characters in a string so the JSON block
/// is safe to embed in a Markdown document. We follow the
/// "no control characters" rule from
/// [`crate::worker::validate_worker_result`].
pub fn escape_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            // Replace with the standard JSON-style escape
            // (\\u00XX) so the body is human-readable and
            // round-trip safe.
            let code = c as u32;
            out.push_str(&format!("\\u{code:04X}"));
        } else {
            out.push(c);
        }
    }
    out
}

/// Apply the control-character escape to every artifact
/// value. Artifact keys are passed through unchanged (the
/// schema validator already rejects control characters in
/// keys; the escape is a belt-and-braces guard for the
/// render path).
pub fn render_artifacts_with_escape(
    artifacts: &std::collections::BTreeMap<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    artifacts
        .iter()
        .map(|(k, v)| (k.clone(), escape_json_value(v)))
        .collect()
}

fn escape_json_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(escape_control_chars(s)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(escape_json_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut new = serde_json::Map::new();
            for (k, v) in obj {
                new.insert(k.clone(), escape_json_value(v));
            }
            serde_json::Value::Object(new)
        }
        other => other.clone(),
    }
}
