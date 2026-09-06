//! Post-run enforcement of the review worker's read-only contract
//! (DAR §6.4, §10.1, §10.2; issue #306).
//!
//! The security model is deliberate: the RO `.git` shadow prevents
//! git-metadata mutation; tracked source files are technically
//! writable (`/workspace` is hard-coded RW) and are enforced by a
//! POST-RUN check, not prevented; daemon control files are untracked
//! and therefore invisible to the dirty check, so they get an
//! explicit pre/post digest comparison.
//!
//! Three-way separation (DAR §10.2): allowed worker output (the
//! result file — OCI: off-worktree, TrustedHost: untracked in the
//! worktree root) / build artefacts (untracked noise) / forbidden
//! daemon control files (`worker-prompt.md`, `review-worktree.json`).
//!
//! The caller (#339's dispatch loop) runs [`enforce_review_read_only`]
//! after worker exit and before result acceptance, on every post-exit
//! path; on [`CaduceusError::ReviewSourceMutation`] it calls
//! [`finish_mutation_violation`] and must NOT tear down the worktree
//! (the archived worktree is forensic evidence; the hint points at it).

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::infra::error::{scrub, CaduceusError, CaduceusResult};
use crate::repo::review_worktree::REVIEW_WORKTREE_METADATA_FILENAME;
use crate::review::ReviewTarget;
use crate::worker::prompt::PROMPT_FILENAME;
use crate::worktree::GitRunner;

/// DAR §13 event name for the mutation-violation terminal route.
pub const MUTATION_VIOLATION_EVENT: &str = "review_mutation_violation";

/// Stable terminal source tag persisted as `blocked_source`.
pub const MUTATION_VIOLATION_SOURCE: &str = "review/mutation_violation";

/// Violation-evidence cap: first N porcelain lines carried in the
/// error detail (bounded logging surface).
pub const MAX_MUTATION_DETAIL_LINES: usize = 20;

/// Pre/post digests of the daemon-owned control files (DAR §10.2).
/// Fixed file set by design — no config knob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewControlFileDigests {
    pub prompt_sha256: String,
    pub worktree_metadata_sha256: String,
}

/// Post-run tracked-file dirty check (DAR §10.1):
/// `git -C <worktree> status --porcelain --untracked-files=no` with NO
/// config overrides — the worktree's own git config (mirror config +
/// checked-out `.gitattributes`) governs smudge/clean, which is what
/// makes the check filter-aware (no autocrlf/eol false positives).
///
/// Any non-empty porcelain output is a violation: staged changes,
/// worktree changes, deletions, and renames all appear in porcelain.
/// Untracked files (the result file, build artefacts) are excluded by
/// `--untracked-files=no`.
///
/// A git-status FAILURE (nonzero exit / timeout / not-a-repo) is
/// `CaduceusError::Git` — `FailureClass::Infrastructure`, NOT a
/// mutation violation: a check that cannot run has detected nothing
/// (DAR §8.1's Terminal row requires a *detected* mutation).
/// Cancellation propagates as [`CaduceusError::Cancelled`].
pub async fn check_tracked_files_clean(runner: &GitRunner, worktree: &Path) -> CaduceusResult<()> {
    let output = runner
        .run_args(
            "review-dirty-check",
            [
                "-C".as_ref(),
                worktree.as_os_str(),
                "status".as_ref(),
                "--porcelain".as_ref(),
                "--untracked-files=no".as_ref(),
            ],
        )
        .await;
    let output = match output {
        Ok(out) => out,
        // `run` surfaces spawn/wait failures as Err(Git) already; the
        // cancelled/timed_out flags arrive inside the Ok output.
        Err(e) => return Err(e),
    };
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out || output.status != Some(0) {
        return Err(CaduceusError::Git {
            operation: "review-dirty-check",
            stderr: output.stderr,
        });
    }
    let stdout = output.stdout.trim();
    if stdout.is_empty() {
        return Ok(());
    }
    let lines: Vec<&str> = stdout.lines().collect();
    let detail = if lines.len() > MAX_MUTATION_DETAIL_LINES {
        format!(
            "tracked files modified: {} (and {} more entries)",
            lines[..MAX_MUTATION_DETAIL_LINES].join("; "),
            lines.len() - MAX_MUTATION_DETAIL_LINES,
        )
    } else {
        format!("tracked files modified: {}", lines.join("; "))
    };
    Err(CaduceusError::ReviewSourceMutation {
        worktree_path: worktree.to_path_buf(),
        detail,
    })
}

/// SHA-256 of a file's bytes (pre/post control-file digest; the
/// `review_claim_digest` precedent uses the same sha2+hex pairing).
fn sha256_file(path: &Path) -> CaduceusResult<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Capture the pre-run digests of the daemon-owned control files
/// (DAR §10.2). Called after the prompt is written and before the
/// worker is spawned. A missing/unreadable control file here is a
/// daemon-setup bug (`CaduceusError::Io`, Infrastructure) — the worker
/// has not run yet, so no mutation is possible.
pub fn capture_control_file_digests(worktree: &Path) -> CaduceusResult<ReviewControlFileDigests> {
    Ok(ReviewControlFileDigests {
        prompt_sha256: sha256_file(&worktree.join(PROMPT_FILENAME))?,
        worktree_metadata_sha256: sha256_file(&worktree.join(REVIEW_WORKTREE_METADATA_FILENAME))?,
    })
}

/// Verify the post-run digests against the captured pre-run digests
/// (DAR §10.2). Deletion is modification: a post-run read failure of a
/// control file is a violation, not an Io error (the worker had write
/// access to the worktree). A hash mismatch names the file and both
/// digests.
pub fn verify_control_file_digests(
    worktree: &Path,
    pre: &ReviewControlFileDigests,
) -> CaduceusResult<()> {
    // worker-prompt.md
    let prompt_path = worktree.join(PROMPT_FILENAME);
    let post_prompt = match sha256_file(&prompt_path) {
        Ok(digest) => digest,
        Err(err) => {
            return Err(CaduceusError::ReviewSourceMutation {
                worktree_path: worktree.to_path_buf(),
                detail: format!("daemon control file missing: {} ({err})", PROMPT_FILENAME),
            });
        }
    };
    if post_prompt != pre.prompt_sha256 {
        return Err(CaduceusError::ReviewSourceMutation {
            worktree_path: worktree.to_path_buf(),
            detail: format!(
                "daemon control file modified: {} (sha256 pre {}, post {})",
                PROMPT_FILENAME, pre.prompt_sha256, post_prompt
            ),
        });
    }
    // review-worktree.json
    let metadata_path = worktree.join(REVIEW_WORKTREE_METADATA_FILENAME);
    let post_metadata = match sha256_file(&metadata_path) {
        Ok(digest) => digest,
        Err(err) => {
            return Err(CaduceusError::ReviewSourceMutation {
                worktree_path: worktree.to_path_buf(),
                detail: format!(
                    "daemon control file missing: {REVIEW_WORKTREE_METADATA_FILENAME} ({err})"
                ),
            });
        }
    };
    if post_metadata != pre.worktree_metadata_sha256 {
        return Err(CaduceusError::ReviewSourceMutation {
            worktree_path: worktree.to_path_buf(),
            detail: format!(
                "daemon control file modified: {REVIEW_WORKTREE_METADATA_FILENAME} \
                 (sha256 pre {}, post {})",
                pre.worktree_metadata_sha256, post_metadata
            ),
        });
    }
    Ok(())
}

/// Composed enforcement (DAR §10): the dirty check first, then the
/// control-file digests. #339 calls this after worker exit and before
/// result acceptance, on every post-exit path (including
/// result-missing: a violation is independent of result presence).
pub async fn enforce_review_read_only(
    runner: &GitRunner,
    worktree: &Path,
    pre: &ReviewControlFileDigests,
) -> CaduceusResult<()> {
    check_tracked_files_clean(runner, worktree).await?;
    verify_control_file_digests(worktree, pre)
}

/// Emit the DAR §13 mutation-violation event. Warn level matches the
/// terminal-block precedent — this is a contract violation requiring
/// operator attention.
pub fn emit_review_mutation_violation(target: &ReviewTarget, worktree: &Path, detail: &str) {
    tracing::warn!(
        target: "caduceus",
        event = MUTATION_VIOLATION_EVENT,
        repo = target.repository.full_name(),
        pr = target.pull_request,
        head_sha = target.head_sha,
        worktree = worktree.display().to_string(),
        detail = scrub(detail),
        "review mutation violation: entry moved to NeedsAttention"
    );
}

/// Composed terminal route for a mutation violation (DAR §8.1,
/// §10.1-10.2; AC3/AC4). Emits the DAR §13 event FIRST (the violation
/// was observed regardless of whether routing succeeds), then routes
/// the review queue entry to `NeedsAttention` with
/// `blocked_recovery_hint` pointing at the preserved worktree.
///
/// The worktree is deliberately NOT torn down on this route — it is
/// forensic evidence (contrast with the issue queue's
/// `finish_needs_attention`, which tears down). The review reaper
/// reclaims it by age. Callers (#339) must not tear it down either.
///
/// Returns an error when `err` is not a
/// [`CaduceusError::ReviewSourceMutation`] — this route is only for
/// mutation violations; all other classifications flow through the
/// normal review router (#339).
pub fn finish_mutation_violation(
    store: &crate::state::review::ReviewStore,
    claim: crate::state::review::ReviewClaimToken,
    target: &crate::review::ReviewTarget,
    err: &CaduceusError,
) -> CaduceusResult<()> {
    let (worktree_path, detail) = match err {
        CaduceusError::ReviewSourceMutation {
            worktree_path,
            detail,
        } => (worktree_path, detail),
        _ => {
            return Err(CaduceusError::Config(
                "finish_mutation_violation requires a ReviewSourceMutation error".to_string(),
            ));
        }
    };
    emit_review_mutation_violation(target, worktree_path, detail);
    let hint = format!(
        "review worker violated the read-only contract; the archived \
         review worktree is preserved for inspection at {} (see \
         `git -C {} status` and `git -C {} diff`); it is reclaimed by \
         the review worktree GC",
        worktree_path.display(),
        worktree_path.display(),
        worktree_path.display()
    );
    store.route_review_to_needs_attention(claim, &err.to_string(), MUTATION_VIOLATION_SOURCE, &hint)
}
