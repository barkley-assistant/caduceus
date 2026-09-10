//! Per-PR fork quarantine clones (issue #337, Phase 2).
//!
//! Fork PR review needs the fork's head SHA, but the Phase-1
//! single-origin mirror must NEVER carry a second remote (DAR §11.2
//! — `src/repo/mirror.rs` is the single-origin primitive). This
//! module owns the quarantine story: a per-PR EPHEMERAL bare clone
//! under `<state_dir>/fork-quarantine/<owner>/<repo>/<pr>@<head_sha>/`,
//! created from the TRUSTED base repo URL, with the fork's head SHA
//! fetched SHA-anchored from the fork URL (no tracking ref, no
//! wildcard refspec).
//!
//! Threat posture (see `docs/security/fork-trust-posture.md`):
//!
//! - The fork URL only ever touches the quarantine clone, never the
//!   production mirror.
//! - The quarantine clone is removed at the terminal status of the
//!   review run (the `ReviewRunGuard` teardown) or by the per-tick
//!   orphan sweep (`sweep`), with a forensic removal marker under
//!   `<state_dir>/fork-quarantine/.removed/`.
//! - All git subprocesses go through the hardened `GitRunner`, so
//!   the ambient-config neutralisation (`core.hooksPath=/dev/null`,
//!   `GIT_CONFIG_NOSYSTEM=1`) and the credential broker
//!   (`GIT_ASKPASS` + `GIT_ASKPASS_FD`) apply to the fork fetch like
//!   every other daemon git command. The clone additionally persists
//!   `core.hooksPath=/dev/null` in its own config.
//!
//! The directory leaf is the review queue key suffix `{pr}@{head_sha}`
//! (deterministic from the [`ReviewTarget`]), so admission, dispatch,
//! and the sweep can all locate the same quarantine without any
//! side-channel state. The plan pinned `<run_id>`; the run id only
//! exists after claim, while the quarantine must exist from
//! admission (merge-base is computed inside it), so the queue-key
//! suffix is the stable identifier.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::review::ReviewTarget;
use crate::worktree::GitRunner;

use super::mirror::BareMirror;

/// Directory name of the quarantine root under `state_dir`.
pub const FORK_QUARANTINE_DIRNAME: &str = "fork-quarantine";
/// Provenance marker written at clone time (mirrors the
/// `quarantine_corrupt` marker pattern from `state/meta.rs`).
pub const QUARANTINE_MARKER_FILENAME: &str = "QUARANTINE_MARKER";
/// Forensic removal-marker directory under the quarantine root.
pub const REMOVED_DIRNAME: &str = ".removed";
/// Schema version of the quarantine marker.
pub const QUARANTINE_MARKER_SCHEMA_VERSION: u32 = 1;

/// Resolve the quarantine root: `<state_dir>/fork-quarantine`.
pub fn fork_quarantine_root(state_dir: &Path) -> PathBuf {
    state_dir.join(FORK_QUARANTINE_DIRNAME)
}

/// Directory leaf for one fork review target: `{pr}@{head_sha}` —
/// the queue key suffix (`owner/repo#pr@head_sha`), deterministic
/// from the target so admission, dispatch, and the sweep agree.
pub fn quarantine_leaf(pull_request: u64, head_sha: &str) -> String {
    format!("{pull_request}@{head_sha}")
}

/// Full review queue key for a quarantine directory at
/// `<root>/<owner>/<repo>/<leaf>` — `owner/repo#leaf`, lowercased to
/// match `review_state_key`'s normalisation.
pub fn quarantine_queue_key(owner: &str, repo: &str, leaf: &str) -> String {
    format!("{}/{}#{}", owner.to_lowercase(), repo.to_lowercase(), leaf)
}

/// Provenance marker written at clone time (schema-versioned, the
/// `state/meta.rs` quarantine-marker pattern).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineMarker {
    pub schema_version: u32,
    /// Base repo identity (also encoded in the directory path).
    pub owner: String,
    pub repo: String,
    pub pull_request: u64,
    pub head_sha: String,
    pub base_sha: String,
    /// The fork's `full_name` (attacker-controlled identity; recorded
    /// for provenance, never trusted for routing).
    pub head_repo: String,
    pub created_at: DateTime<Utc>,
}

impl QuarantineMarker {
    pub fn new(
        owner: &str,
        repo: &str,
        pull_request: u64,
        head_sha: &str,
        base_sha: &str,
        head_repo: &str,
    ) -> Self {
        Self {
            schema_version: QUARANTINE_MARKER_SCHEMA_VERSION,
            owner: owner.to_string(),
            repo: repo.to_string(),
            pull_request,
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            head_repo: head_repo.to_string(),
            created_at: Utc::now(),
        }
    }
}

/// A per-PR fork quarantine clone: a bare mirror of the TRUSTED base
/// repo plus the fork's head SHA, isolated from the production
/// mirror and removed at the terminal status of the review run.
#[derive(Clone, Debug)]
pub struct ForkQuarantine {
    /// `<state_dir>/fork-quarantine/<owner>/<repo>/<pr>@<head_sha>/`
    pub path: PathBuf,
    /// `<state_dir>/fork-quarantine` (removal-marker root).
    pub quarantine_root: PathBuf,
    /// Provenance recorded at clone time (best-effort reloaded).
    pub marker: Option<QuarantineMarker>,
}

impl ForkQuarantine {
    /// Create the quarantine clone for one fork review target.
    /// Idempotent: an existing clone (same leaf) is reused, matching
    /// the mirror's lazy-bootstrap contract. The base objects come
    /// from `git clone --bare --no-tags <base_url>` (the TRUSTED
    /// origin); the fork URL is never passed here — it only enters
    /// through [`ForkQuarantine::fetch_fork_sha`].
    #[allow(clippy::too_many_arguments)] // fixed 9-arg create contract (mirror.ensure shape)
    pub async fn create(
        runner: &GitRunner,
        state_dir: &Path,
        owner: &str,
        repo: &str,
        pull_request: u64,
        head_sha: &str,
        base_sha: &str,
        base_url: &str,
        head_repo: &str,
    ) -> CaduceusResult<Self> {
        let leaf = quarantine_leaf(pull_request, head_sha);
        // Path-safety guard: the leaf is `{digits}@{hex}`; anything
        // else (a '/' or '..' smuggled through a malformed SHA) is
        // refused before any filesystem work.
        if leaf.contains('/') || leaf.split('/').any(|c| c == "..") {
            return Err(CaduceusError::Config(format!(
                "fork quarantine leaf {leaf:?} is not path-safe"
            )));
        }
        let path = fork_quarantine_root(state_dir)
            .join(owner)
            .join(repo)
            .join(&leaf);

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(CaduceusError::Io)?;
            }
        }

        // Clone once (idempotent). `--bare --no-tags` matches the
        // mirror primitive; no refspec beyond the base branch is
        // fetched, so the quarantine starts from trusted objects only.
        if !path.join("HEAD").exists() {
            let output = runner
                .run_args(
                    "fork-quarantine-clone",
                    [
                        "clone",
                        "--bare",
                        "--no-tags",
                        base_url,
                        &path.to_string_lossy(),
                    ],
                )
                .await?;
            if output.cancelled {
                return Err(CaduceusError::Cancelled);
            }
            if output.timed_out || output.status != Some(0) {
                return Err(CaduceusError::Git {
                    operation: "fork-quarantine-clone",
                    stderr: output.stderr,
                });
            }
            // Enforce the daemon's private-storage mode policy.
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }

        // Durable ambient-config neutralisation INSIDE the clone:
        // even a direct `git` invocation on this path (never done by
        // the daemon, but belt-and-braces) cannot fire a hook.
        let _ = runner
            .run_args(
                "fork-quarantine-hooks",
                [
                    "-C",
                    &path.to_string_lossy(),
                    "config",
                    "core.hooksPath",
                    "/dev/null",
                ],
            )
            .await;

        let marker =
            QuarantineMarker::new(owner, repo, pull_request, head_sha, base_sha, head_repo);
        Self::write_marker(&path, &marker)?;

        Ok(Self {
            path,
            quarantine_root: fork_quarantine_root(state_dir),
            marker: Some(marker),
        })
    }

    /// Fetch the fork's head SHA into the quarantine clone:
    /// `git fetch <fork_url> <head_sha>` — SHA-anchored, no tracking
    /// ref, no wildcard refspec. The fork URL is the ONLY thing that
    /// ever touches the quarantine clone.
    ///
    /// Reject-on-unavailable mirrors `BareMirror::fetch_sha`: a
    /// fetch failure for a SHA that is not already present locally
    /// surfaces as [`CaduceusError::HeadShaUnavailable`].
    pub async fn fetch_fork_sha(
        &self,
        runner: &GitRunner,
        fork_url: &str,
        head_sha: &str,
    ) -> CaduceusResult<()> {
        let output = runner
            .run_args(
                "fork-quarantine-fetch",
                [
                    "-C",
                    &self.path.to_string_lossy(),
                    "fetch",
                    fork_url,
                    head_sha,
                ],
            )
            .await?;
        if output.cancelled {
            return Err(CaduceusError::Cancelled);
        }
        if output.timed_out || output.status != Some(0) {
            if self.sha_present(runner, head_sha).await? {
                // Already fetched (idempotent re-fetch); the
                // transport failure is transient.
                return Ok(());
            }
            return Err(CaduceusError::HeadShaUnavailable {
                sha: head_sha.to_string(),
            });
        }
        Ok(())
    }

    /// Compute the merge base INSIDE the quarantine clone:
    /// `git merge-base <base_sha> <head_sha>`. Unrelated histories
    /// fail as [`CaduceusError::Git`] (same contract as
    /// `BareMirror::merge_base`).
    pub async fn merge_base(
        &self,
        runner: &GitRunner,
        base_sha: &str,
        head_sha: &str,
    ) -> CaduceusResult<String> {
        let output = runner
            .run_args(
                "fork-quarantine-merge-base",
                [
                    "-C",
                    &self.path.to_string_lossy(),
                    "merge-base",
                    base_sha,
                    head_sha,
                ],
            )
            .await?;
        if output.cancelled {
            return Err(CaduceusError::Cancelled);
        }
        if output.timed_out || output.status != Some(0) {
            return Err(CaduceusError::Git {
                operation: "fork-quarantine-merge-base",
                stderr: output.stderr,
            });
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Present the quarantine clone as a [`BareMirror`] so the
    /// existing review-worktree machinery (`create_review` /
    /// `ReviewWorktree::remove`) works against it unchanged. The
    /// `remote_url` is deliberately empty — the quarantine has no
    /// production-origin remote contract.
    pub fn as_bare_mirror(&self) -> BareMirror {
        BareMirror {
            path: self.path.clone(),
            remote_url: String::new(),
        }
    }

    /// The quarantine directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Find the quarantine for a review target, if one exists. The
    /// directory's existence is the authority (its leaf encodes the
    /// target's `pr@head_sha`); the marker is provenance only.
    pub fn find_for_target(state_dir: &Path, target: &ReviewTarget) -> Option<Self> {
        let leaf = quarantine_leaf(target.pull_request, &target.head_sha);
        let path = fork_quarantine_root(state_dir)
            .join(&target.repository.owner)
            .join(&target.repository.repo)
            .join(&leaf);
        if !path.join("HEAD").exists() {
            return None;
        }
        Some(Self {
            path: path.clone(),
            quarantine_root: fork_quarantine_root(state_dir),
            marker: Self::load_marker(&path).ok(),
        })
    }

    /// Remove the quarantine clone: registered worktrees first
    /// (`git worktree remove --force`), then `rm -rf` of the clone,
    /// then a forensic removal marker under
    /// `<root>/.removed/`. Idempotent — a missing path is a no-op.
    pub async fn remove(&self, runner: &GitRunner) -> CaduceusResult<()> {
        if !self.path.exists() {
            return Ok(());
        }
        // Tear down any registered worktrees first (the review
        // guard normally does this, but the sweep / crash path may
        // arrive with a live registration).
        let worktrees = self.list_worktrees(runner).await;
        for worktree_path in worktrees {
            let _ = runner
                .run_args(
                    "fork-quarantine-worktree-remove",
                    [
                        "-C",
                        &self.path.to_string_lossy(),
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ],
                )
                .await;
        }
        std::fs::remove_dir_all(&self.path).map_err(|err| CaduceusError::Worktree {
            context: "fork-quarantine-remove",
            stderr: format!("remove_dir_all {} failed: {err}", self.path.display()),
        })?;
        self.write_removal_marker().await;
        Ok(())
    }

    /// Sweep orphaned quarantine clones: remove every clone whose
    /// review queue key (`owner/repo#pr@head_sha`) is not in
    /// `active_keys` (the union of queued + in-progress review
    /// entries), with a forensic removal marker. Returns the number
    /// removed. Best-effort per clone: a removal failure is logged
    /// and does not abort the sweep.
    pub async fn sweep(
        state_dir: &Path,
        runner: &GitRunner,
        active_keys: &[String],
    ) -> CaduceusResult<u64> {
        let root = fork_quarantine_root(state_dir);
        if !root.is_dir() {
            return Ok(0);
        }
        let mut removed: u64 = 0;
        let mut owner_dirs: Vec<_> = std::fs::read_dir(&root)
            .map_err(CaduceusError::Io)?
            .filter_map(|e| e.ok())
            .collect();
        owner_dirs.sort_by_key(|e| e.file_name());
        for owner_entry in owner_dirs {
            let owner = owner_entry.file_name();
            let owner_path = owner_entry.path();
            if owner == REMOVED_DIRNAME || !owner_path.is_dir() {
                continue;
            }
            let mut repo_dirs: Vec<_> = std::fs::read_dir(&owner_path)
                .map_err(CaduceusError::Io)?
                .filter_map(|e| e.ok())
                .collect();
            repo_dirs.sort_by_key(|e| e.file_name());
            for repo_entry in repo_dirs {
                let repo_path = repo_entry.path();
                if !repo_path.is_dir() {
                    continue;
                }
                let mut leaf_dirs: Vec<_> = std::fs::read_dir(&repo_path)
                    .map_err(CaduceusError::Io)?
                    .filter_map(|e| e.ok())
                    .collect();
                leaf_dirs.sort_by_key(|e| e.file_name());
                for leaf_entry in leaf_dirs {
                    let leaf = leaf_entry.file_name().to_string_lossy().into_owned();
                    let clone_path = leaf_entry.path();
                    if !clone_path.is_dir() {
                        continue;
                    }
                    let key = quarantine_queue_key(
                        &owner.to_string_lossy(),
                        &repo_entry.file_name().to_string_lossy(),
                        &leaf,
                    );
                    if active_keys.iter().any(|k| k == &key) {
                        continue;
                    }
                    let quarantine = Self {
                        path: clone_path.clone(),
                        quarantine_root: root.clone(),
                        marker: Self::load_marker(&clone_path).ok(),
                    };
                    match quarantine.remove(runner).await {
                        Ok(()) => removed += 1,
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                path = %clone_path.display(),
                                "fork quarantine sweep: removal failed; continuing"
                            );
                        }
                    }
                }
            }
        }
        Ok(removed)
    }

    async fn list_worktrees(&self, runner: &GitRunner) -> Vec<PathBuf> {
        let Ok(output) = runner
            .run_args(
                "fork-quarantine-worktree-list",
                [
                    "-C",
                    &self.path.to_string_lossy(),
                    "worktree",
                    "list",
                    "--porcelain",
                ],
            )
            .await
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in output.stdout.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                let p = PathBuf::from(rest.trim());
                if p != self.path {
                    out.push(p);
                }
            }
        }
        out
    }

    /// `git cat-file -e <sha>^{commit}` — local presence check.
    async fn sha_present(&self, runner: &GitRunner, sha: &str) -> CaduceusResult<bool> {
        let output = runner
            .run_args(
                "fork-quarantine-cat-file",
                [
                    "-C",
                    &self.path.to_string_lossy(),
                    "cat-file",
                    "-e",
                    &format!("{sha}^{{commit}}"),
                ],
            )
            .await?;
        Ok(output.status == Some(0))
    }

    fn write_marker(path: &Path, marker: &QuarantineMarker) -> CaduceusResult<()> {
        let bytes = serde_json::to_vec_pretty(marker)
            .map_err(|err| CaduceusError::Other(format!("serialise quarantine marker: {err}")))?;
        std::fs::write(path.join(QUARANTINE_MARKER_FILENAME), bytes).map_err(|err| {
            CaduceusError::Other(format!(
                "write quarantine marker {} failed: {err}",
                path.join(QUARANTINE_MARKER_FILENAME).display()
            ))
        })
    }

    fn load_marker(path: &Path) -> CaduceusResult<QuarantineMarker> {
        let raw =
            std::fs::read_to_string(path.join(QUARANTINE_MARKER_FILENAME)).map_err(|err| {
                CaduceusError::Other(format!(
                    "read quarantine marker {} failed: {err}",
                    path.join(QUARANTINE_MARKER_FILENAME).display()
                ))
            })?;
        let marker: QuarantineMarker = serde_json::from_str(&raw)
            .map_err(|err| CaduceusError::Other(format!("parse quarantine marker: {err}")))?;
        if marker.schema_version != QUARANTINE_MARKER_SCHEMA_VERSION {
            return Err(CaduceusError::Other(format!(
                "quarantine marker schema_version {} is not supported (this daemon \
                 accepts {QUARANTINE_MARKER_SCHEMA_VERSION})",
                marker.schema_version
            )));
        }
        Ok(marker)
    }

    async fn write_removal_marker(&self) {
        let removed_dir = self.quarantine_root.join(REMOVED_DIRNAME);
        if let Err(err) = std::fs::create_dir_all(&removed_dir) {
            tracing::warn!(
                error = %err,
                path = %removed_dir.display(),
                "fork quarantine: removal marker dir create failed"
            );
            return;
        }
        let ts = Utc::now().timestamp();
        let leaf = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let marker_path = removed_dir.join(format!("{ts}-{leaf}.removed"));
        let marker = match &self.marker {
            Some(m) => serde_json::json!({
                "removed_at_unix": ts,
                "leaf": leaf,
                "head_repo": m.head_repo,
                "head_sha": m.head_sha,
                "base_sha": m.base_sha,
                "pull_request": m.pull_request,
                "owner": m.owner,
                "repo": m.repo,
            }),
            None => serde_json::json!({
                "removed_at_unix": ts,
                "leaf": leaf,
                "note": "marker unreadable; provenance from directory path only",
            }),
        };
        if let Err(err) = std::fs::write(
            &marker_path,
            serde_json::to_string_pretty(&marker).unwrap_or_default(),
        ) {
            tracing::warn!(
                error = %err,
                path = %marker_path.display(),
                "fork quarantine: removal marker write failed"
            );
        }
        tracing::info!(
            target: "caduceus",
            event = "fork_quarantine_removed",
            path = %self.path.display(),
            "fork quarantine clone removed at terminal status"
        );
    }
}
