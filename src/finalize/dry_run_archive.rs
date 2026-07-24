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
