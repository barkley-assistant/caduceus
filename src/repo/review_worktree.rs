//! Review worktrees: disposable, detached-HEAD checkouts at an exact
//! PR head SHA, created against the daemon-owned bare mirror.
//!
//! The issue-dispatch worktree path (`worktree::Worktree`, branch-based)
//! is deliberately untouched: a review worktree is detached by
//! construction (DAR §6.1 — no synthetic `IssueKey`, no branch name at
//! the type level) and **non-pushable by construction** — no
//! `refs/heads/*` artefact is created and the create flow performs no
//! push-side git operation; the mirror's `origin` remote is only ever
//! fetched.
//!
//! Dir shape (D3): `<repo_storage_root>/worktrees/review/<owner>/<repo>/<run_id>/`
//! — namespaced under a literal `review/` segment so the review sweep
//! (GC) can iterate per mirror and no future co-tenant of the flat
//! `worktrees/` namespace can collide.
//!
//! The metadata sidecar (`review-worktree.json`, written once at
//! materialisation) carries the frozen `ReviewTarget` context
//! (`base_sha`, `base_ref`, the persisted `merge_base`) so the prompt
//! builder (#303) and dispatch (#339) can read it without a live
//! handle — the merge-base (three-dot) diff is
//! `git diff <merge_base> <head_sha>` (DAR §2.2).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::review::{
    RepositoryId, ReviewTarget, MAX_REF_BYTES, MAX_REPO_COMPONENT_BYTES, MAX_RUN_ID_BYTES,
    MAX_SHA_BYTES,
};
use crate::worktree::GitRunner;

use super::mirror::BareMirror;
use super::storage::Storage;
use super::worktree::validate_run_id;

/// Schema version of the review-worktree metadata sidecar.
pub const REVIEW_WORKTREE_SCHEMA_VERSION: u32 = 1;

/// A disposable review worktree: detached HEAD at the exact PR head
/// SHA, created against the daemon-owned bare mirror. Non-pushable by
/// construction (no branch ref exists to push; the create flow
/// performs no push-side git operation).
#[derive(Clone, Debug)]
pub struct ReviewWorktree {
    /// The bare mirror this worktree was created from.
    pub mirror: BareMirror,
    /// Daemon-allocated execution identifier (directory basename).
    pub run_id: String,
    /// `<repo_storage_root>/worktrees/review/<owner>/<repo>/<run_id>/`
    pub path: PathBuf,
    /// The exact PR head SHA checked out (identity, DAR §2.1).
    pub head_sha: String,
    /// Metadata sidecar path: `<path>/review-worktree.json`.
    pub metadata_path: PathBuf,
    /// When this worktree was created.
    pub created_at: DateTime<Utc>,
}

/// Persisted review-worktree metadata (DAR §2.1 context). Written once
/// at materialisation; read by #303 (merge-base diff) and #339.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub struct ReviewWorktreeMetadata {
    /// Sidecar schema version; must equal
    /// [`REVIEW_WORKTREE_SCHEMA_VERSION`].
    pub schema_version: u32,
    pub run_id: String,
    pub repository: RepositoryId,
    pub pull_request: u64,
    /// Identity (DAR §2.1) — the exact checked-out SHA.
    pub head_sha: String,
    /// Context, frozen at admission; never a diff endpoint.
    pub base_sha: String,
    pub base_ref: String,
    /// Persisted merge base (DAR §2.1) — the three-dot diff anchor.
    pub merge_base: String,
    pub created_at: DateTime<Utc>,
}

impl ReviewWorktree {
    /// Materialise the review worktree for `target` (#299 primitive).
    ///
    /// Flow: (1) validate inputs; (2) SHA-anchored re-fetch via
    /// `BareMirror::fetch_sha` (idempotent; surfaces
    /// `HeadShaUnavailable` here — AC 4); (3) resolve the review dir
    /// (D3); (4) refuse existing path (`WorktreeReuseAfterFailure`);
    /// (5) `git worktree add --detach <path> <head_sha>` from the
    /// mirror (umask 0o022 → restore); (6) write the metadata sidecar
    /// (D2); (7) verify `HEAD == head_sha` via
    /// `git -C <path> rev-parse HEAD` (AC 1 enforcement at runtime);
    /// (8) return the handle. A failure between (5) and (7) tears the
    /// worktree down — no half-materialised state.
    pub async fn create_review(
        runner: &GitRunner,
        mirror: &BareMirror,
        run_id: &str,
        target: &ReviewTarget,
    ) -> CaduceusResult<Self> {
        validate_run_id(run_id)?;
        crate::review::validate_review_target(target)?;

        // D5: the SHA-anchored re-fetch happens before any path work;
        // HeadShaUnavailable surfaces at this boundary (AC 4).
        mirror.fetch_sha(runner, &target.head_sha).await?;

        // D3: resolve the namespaced review dir under the storage root.
        let worktree_path = review_worktree_path(
            mirror,
            &target.repository.owner,
            &target.repository.repo,
            run_id,
        )?;

        // D4: refuse to reuse a path from a prior attempt.
        if worktree_path.exists() {
            return Err(CaduceusError::WorktreeReuseAfterFailure {
                run_id: run_id.to_string(),
                worktree_path,
                last_state: "exists".to_string(),
            });
        }

        let parent_dir = worktree_path.parent().ok_or_else(|| CaduceusError::Worktree {
            context: "review-create",
            stderr: format!(
                "worktree path {} has no parent directory",
                worktree_path.display()
            ),
        })?;
        std::fs::create_dir_all(parent_dir).map_err(|err| CaduceusError::Worktree {
            context: "review-create",
            stderr: format!("create review dir {} failed: {err}", parent_dir.display()),
        })?;

        let path_str = worktree_path.to_string_lossy().into_owned();
        let mirror_str = mirror.path.to_string_lossy().into_owned();

        // The umask is switched to 0o022 for the worktree-add call so
        // source-file executable bits are preserved, and restored
        // afterwards. The spawn in the runner's `run_args` is
        // synchronous (command.spawn() before any await points), so
        // the child inherits the temporary umask.
        let prev = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
        let add_result = runner
            .run_args(
                "review-worktree-add",
                [
                    "-C",
                    &mirror_str,
                    "worktree",
                    "add",
                    "--detach",
                    &path_str,
                    &target.head_sha,
                ],
            )
            .await;
        nix::sys::stat::umask(prev);
        let add_output = add_result?;

        if add_output.cancelled {
            return Err(CaduceusError::Cancelled);
        }
        if add_output.timed_out || add_output.status != Some(0) {
            return Err(CaduceusError::Worktree {
                context: "review-create",
                stderr: format!(
                    "git worktree add --detach {} {} failed: {}",
                    worktree_path.display(),
                    target.head_sha,
                    add_output.stderr
                ),
            });
        }

        let created_at = Utc::now();
        let metadata_path = worktree_path.join("review-worktree.json");
        let handle = Self {
            mirror: mirror.clone(),
            run_id: run_id.to_string(),
            path: worktree_path.clone(),
            head_sha: target.head_sha.clone(),
            metadata_path: metadata_path.clone(),
            created_at,
        };

        // D2: write the metadata sidecar. On failure, tear the
        // checkout down — no half-materialised state.
        let metadata = ReviewWorktreeMetadata {
            schema_version: REVIEW_WORKTREE_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            repository: target.repository.clone(),
            pull_request: target.pull_request,
            head_sha: target.head_sha.clone(),
            base_sha: target.base_sha.clone(),
            base_ref: target.base_ref.clone(),
            merge_base: target.merge_base.clone(),
            created_at,
        };
        let bytes = serde_json::to_vec_pretty(&metadata).map_err(|err| CaduceusError::Worktree {
            context: "review-create",
            stderr: format!("serialise review metadata: {err}"),
        })?;
        if let Err(err) = std::fs::write(&metadata_path, bytes) {
            let _ = Self::remove(runner, &handle).await;
            return Err(CaduceusError::Worktree {
                context: "review-create",
                stderr: format!(
                    "write review metadata {} failed: {err}",
                    metadata_path.display()
                ),
            });
        }

        // AC 1 enforcement at runtime: the checked-out HEAD must equal
        // the requested head_sha.
        let head_output = runner
            .run_args(
                "review-head-verify",
                ["-C", &worktree_path.to_string_lossy(), "rev-parse", "HEAD"],
            )
            .await?;
        if head_output.cancelled {
            let _ = Self::remove(runner, &handle).await;
            return Err(CaduceusError::Cancelled);
        }
        if head_output.timed_out || head_output.status != Some(0) {
            let _ = Self::remove(runner, &handle).await;
            return Err(CaduceusError::Worktree {
                context: "review-create",
                stderr: format!("verify review HEAD failed: {}", head_output.stderr),
            });
        }
        let head = head_output.stdout.trim().to_string();
        if head != target.head_sha {
            let _ = Self::remove(runner, &handle).await;
            return Err(CaduceusError::Worktree {
                context: "review-create",
                stderr: format!(
                    "review worktree HEAD {head} != requested head_sha {}",
                    target.head_sha
                ),
            });
        }

        Ok(handle)
    }

    /// Load the persisted metadata sidecar (for #303/#339 and the
    /// reaper's orphan sweep).
    pub fn load_metadata(&self) -> CaduceusResult<ReviewWorktreeMetadata> {
        let raw = std::fs::read_to_string(&self.metadata_path).map_err(|err| {
            CaduceusError::Worktree {
                context: "review-load",
                stderr: format!(
                    "read review metadata {} failed: {err}",
                    self.metadata_path.display()
                ),
            }
        })?;
        let meta: ReviewWorktreeMetadata =
            serde_json::from_str(&raw).map_err(|err| CaduceusError::Worktree {
                context: "review-load",
                stderr: format!(
                    "parse review metadata {} failed: {err}",
                    self.metadata_path.display()
                ),
            })?;
        validate_review_worktree_metadata(&meta)?;
        Ok(meta)
    }

    /// Remove: `git worktree remove --force` from the mirror +
    /// filesystem fallback + `worktree prune`. No branch ref can
    /// exist (nothing created one), so "no leftover refs" (AC 5)
    /// holds by construction; tests assert it anyway.
    pub async fn remove(runner: &GitRunner, worktree: &Self) -> CaduceusResult<()> {
        let path_str = worktree.path.to_string_lossy().into_owned();
        let mirror_str = worktree.mirror.path.to_string_lossy().into_owned();
        let output = runner
            .run_args(
                "review-worktree-remove",
                [
                    "-C",
                    &mirror_str,
                    "worktree",
                    "remove",
                    "--force",
                    &path_str,
                ],
            )
            .await?;

        if output.cancelled {
            return Err(CaduceusError::Cancelled);
        }

        // `git worktree remove --force` returns nonzero for
        // filesystem errors; clean up what we can regardless.
        if worktree.path.exists() {
            std::fs::remove_dir_all(&worktree.path).map_err(|err| CaduceusError::Worktree {
                context: "review-remove",
                stderr: format!(
                    "fs remove_dir_all {} failed: {err}",
                    worktree.path.display()
                ),
            })?;
        }

        // Best-effort registration prune: clears stale entries left
        // when the removal above raced a directory deletion.
        let _ = runner
            .run_args(
                "review-worktree-prune",
                ["-C", &mirror_str, "worktree", "prune"],
            )
            .await;

        Ok(())
    }
}

/// Resolve the review worktree path for `(owner, repo, run_id)` under
/// the storage root derived from the mirror path:
/// `<storage>/mirrors/<owner>/<repo>.git/` → `<storage>` →
/// `<storage>/worktrees/review/<owner>/<repo>/<run_id>/`.
fn review_worktree_path(
    mirror: &BareMirror,
    owner: &str,
    repo: &str,
    run_id: &str,
) -> CaduceusResult<PathBuf> {
    let storage_root = mirror
        .path
        .parent() // <owner>/
        .and_then(|p| p.parent()) // mirrors/
        .and_then(|p| p.parent()) // <storage>/
        .map(|p| p.to_path_buf())
        .ok_or_else(|| {
            CaduceusError::Config("cannot resolve storage root from mirror path".to_string())
        })?;

    // Guard (Risk 2): the mirror must actually live under
    // `<storage>/mirrors/` so the walk cannot land somewhere
    // unexpected.
    let mirrors_dir = storage_root.join("mirrors");
    if !mirror.path.starts_with(&mirrors_dir) {
        return Err(CaduceusError::Config(format!(
            "mirror path {} does not live under {}",
            mirror.path.display(),
            mirrors_dir.display()
        )));
    }

    Ok(storage_root
        .join("worktrees")
        .join("review")
        .join(owner)
        .join(repo)
        .join(run_id))
}

/// Required string: non-empty and at most `max` bytes (same shape as
/// `review::validate_review_target`'s helper; the review-module
/// helpers are private so this is a local copy).
fn required_string(scope: &str, field: &str, value: &str, max: usize) -> CaduceusResult<()> {
    if value.is_empty() {
        return Err(CaduceusError::Config(format!(
            "{scope}: {field} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(CaduceusError::Config(format!(
            "{scope}: {field} exceeds limit of {max} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

/// Cap validation for a [`ReviewWorktreeMetadata`] document.
fn validate_review_worktree_metadata(meta: &ReviewWorktreeMetadata) -> CaduceusResult<()> {
    if meta.schema_version != REVIEW_WORKTREE_SCHEMA_VERSION {
        return Err(CaduceusError::Config(format!(
            "review worktree metadata: schema_version {} is not supported \
             (this daemon accepts {REVIEW_WORKTREE_SCHEMA_VERSION})",
            meta.schema_version
        )));
    }
    required_string("review worktree metadata", "run_id", &meta.run_id, MAX_RUN_ID_BYTES)?;
    required_string(
        "review worktree metadata",
        "repository.owner",
        &meta.repository.owner,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    required_string(
        "review worktree metadata",
        "repository.repo",
        &meta.repository.repo,
        MAX_REPO_COMPONENT_BYTES,
    )?;
    required_string("review worktree metadata", "head_sha", &meta.head_sha, MAX_SHA_BYTES)?;
    required_string("review worktree metadata", "base_sha", &meta.base_sha, MAX_SHA_BYTES)?;
    required_string("review worktree metadata", "base_ref", &meta.base_ref, MAX_REF_BYTES)?;
    required_string(
        "review worktree metadata",
        "merge_base",
        &meta.merge_base,
        MAX_SHA_BYTES,
    )?;
    Ok(())
}

/// Sweep stale review worktrees across the configured repositories
/// (DAR §15, AC 5 second half).
///
/// For each watched repo the sweep:
///
/// 1. resolves `worktrees/review/<owner>/<repo>/` under
///    `cfg.repo_storage_root` (skip when absent);
/// 2. reads `git worktree list --porcelain` from the matching mirror
///    and parses the detached entries (`worktree <path>` + `HEAD
///    <sha>` + `detached` triples — a review-side parser, NOT the
///    state-dir parser which deliberately drops detached entries);
/// 3. protects in-use entries: any `.claim` file whose
///    `worktree_path` matches, or a fresh
///    `<state_dir>/runs/<run_id>.heartbeat` whose run_id matches the
///    worktree directory basename;
/// 4. removes entries whose directory mtime is older than
///    `older_than_days` via [`ReviewWorktree::remove`] (sidecar load
///    is best-effort — a missing sidecar does not block sweeping);
/// 5. sweeps orphan directories that git no longer registers, with
///    the same age + in-use + symlink checks (refuses symlinks).
///
/// `dry_run = true` reports what would be removed without mutating
/// state. Returns the number of worktrees actually removed.
pub async fn gc_review_worktrees(
    cfg: &Config,
    runner: &GitRunner,
    older_than_days: u64,
    dry_run: bool,
) -> CaduceusResult<u64> {
    // TOCTOU: validate the storage root before any sweep work.
    Storage::new(cfg.repo_storage_root.clone()).validate_root()?;

    let now = Utc::now();
    let age_cutoff = now - chrono::Duration::days(older_than_days as i64);

    let mut total_removed: u64 = 0;
    for repo in &cfg.watched_repos {
        let (owner, repo_name) = match parse_watched_repo(repo) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    entry = %repo,
                    "review worktree gc: invalid watched_repos entry"
                );
                continue;
            }
        };

        let review_repo_dir = cfg
            .repo_storage_root
            .join("worktrees")
            .join("review")
            .join(&owner)
            .join(&repo_name);
        if !review_repo_dir.is_dir() {
            continue;
        }

        let mirror_path = cfg
            .repo_storage_root
            .join("mirrors")
            .join(&owner)
            .join(format!("{repo_name}.git"));
        if !mirror_path.is_dir() {
            continue;
        }
        let mirror = BareMirror {
            path: mirror_path,
            remote_url: String::new(),
        };

        let in_use = collect_review_in_use_paths(cfg, &owner, &repo_name)?;

        let entries = list_review_worktrees_porcelain(runner, &mirror.path).await?;

        // Registered canonical paths for the orphan sweep.
        let registered_paths: std::collections::HashSet<PathBuf> = entries
            .iter()
            .map(|e| canonical_or_raw(&e.path))
            .collect();

        for entry in &entries {
            // Only detached review entries are swept; a branch-based
            // worktree in the review dir is an anomaly — refuse, do
            // not sweep.
            if !entry.detached {
                tracing::warn!(
                    path = %entry.path.display(),
                    "review worktree gc: skipping non-detached entry"
                );
                continue;
            }

            let canonical = canonical_or_raw(&entry.path);
            if in_use.contains(&canonical) {
                continue;
            }

            let mtime = match mtime_of(&entry.path) {
                Some(t) => t,
                None => continue, // can't tell → leave alone
            };
            if mtime > age_cutoff {
                continue;
            }

            if dry_run {
                println!(
                    "would remove review worktree {} (head {})",
                    entry.path.display(),
                    entry.head
                );
                continue;
            }

            let run_id = entry
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let handle = ReviewWorktree {
                mirror: mirror.clone(),
                run_id,
                path: entry.path.clone(),
                head_sha: entry.head.clone(),
                metadata_path: entry.path.join("review-worktree.json"),
                created_at: mtime,
            };
            // Sidecar load is best-effort: a missing/corrupt sidecar
            // (crash between checkout and metadata write) must not
            // block sweeping.
            let _ = handle.load_metadata();
            match ReviewWorktree::remove(runner, &handle).await {
                Ok(()) => {
                    total_removed += 1;
                    println!(
                        "removed review worktree {} (head {})",
                        entry.path.display(),
                        entry.head
                    );
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: remove {} failed: {err}",
                        entry.path.display()
                    );
                }
            }
        }

        // Orphan sweep: unregistered directories under the per-repo
        // review dir. Git cannot remove what it does not know, so
        // orphans are removed with a direct `fs::remove_dir_all`.
        let orphans = collect_review_orphans(&review_repo_dir, &registered_paths, &in_use, age_cutoff);
        for orphan in orphans {
            if dry_run {
                println!("would remove review orphan {}", orphan.display());
                continue;
            }
            match std::fs::remove_dir_all(&orphan) {
                Ok(()) => {
                    total_removed += 1;
                    println!("removed review orphan {}", orphan.display());
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: review orphan remove {} failed: {err}",
                        orphan.display()
                    );
                }
            }
        }
    }
    Ok(total_removed)
}

/// Parse `git worktree list --porcelain` output into detached review
/// entries, keeping the `worktree <path>` + `HEAD <sha>` + `detached`
/// triple. Entries without `detached` are returned with
/// `detached == false` so the caller can refuse (not sweep) them.
fn parse_review_porcelain(stdout: &str) -> Vec<ReviewPorcelainEntry> {
    let mut entries: Vec<ReviewPorcelainEntry> = Vec::new();
    let mut current: Option<ReviewPorcelainEntry> = None;
    for line in stdout.lines() {
        if line.is_empty() {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(ReviewPorcelainEntry {
                path: PathBuf::from(rest.trim()),
                head: String::new(),
                detached: false,
            });
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            if let Some(e) = current.as_mut() {
                e.head = rest.trim().to_string();
            }
        } else if line.strip_prefix("detached").is_some() {
            if let Some(e) = current.as_mut() {
                e.detached = true;
            }
        }
    }
    if let Some(e) = current.take() {
        entries.push(e);
    }
    entries
}

/// One detached review entry of `git worktree list --porcelain`.
#[derive(Debug, Clone)]
struct ReviewPorcelainEntry {
    path: PathBuf,
    head: String,
    detached: bool,
}

/// Read `git worktree list --porcelain` for a mirror and return the
/// parsed entries.
async fn list_review_worktrees_porcelain(
    runner: &GitRunner,
    mirror_path: &Path,
) -> CaduceusResult<Vec<ReviewPorcelainEntry>> {
    let mirror_str = mirror_path.to_string_lossy().into_owned();
    let output = runner
        .run_args(
            "review-worktree-list",
            ["-C", &mirror_str, "worktree", "list", "--porcelain"],
        )
        .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out || output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "review-gc",
            stderr: format!("git worktree list --porcelain failed: {}", output.stderr),
        });
    }
    Ok(parse_review_porcelain(&output.stdout))
}

/// Canonical worktree paths currently referenced by an active claim
/// file or a fresh heartbeat, scoped to one `(owner, repo)`.
/// Freshness for heartbeats is mtime within the last hour (twice the
/// documented 30-minute heartbeat interval) — same semantics as the
/// state-dir GC.
fn collect_review_in_use_paths(
    cfg: &Config,
    owner: &str,
    repo: &str,
) -> CaduceusResult<std::collections::HashSet<PathBuf>> {
    let mut paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    // Claims: every `.claim` file under `<state_dir>/claims/` whose
    // parsed `worktree_path` matches. Review claims don't have an
    // IssueKey digest — the GC reads all claim files, path-keyed.
    let claims_dir = cfg.state_dir.join("claims");
    if claims_dir.is_dir() {
        let entries = std::fs::read_dir(&claims_dir).map_err(|err| CaduceusError::Worktree {
            context: "review-gc",
            stderr: format!("read_dir {}: {err}", claims_dir.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".claim"))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(body) =
                    serde_json::from_slice::<crate::state::queue::ClaimFileBody>(&bytes)
                {
                    if let Some(wt) = body.worktree_path {
                        paths.insert(canonical_or_raw(&wt));
                    }
                }
            }
        }
    }

    // Heartbeats: a fresh `<state_dir>/runs/<run_id>.heartbeat`
    // protects `worktrees/review/<owner>/<repo>/<run_id>`.
    let runs = cfg.state_dir.join("runs");
    if runs.is_dir() {
        let heartbeat_fresh_cutoff = Utc::now() - chrono::Duration::hours(1);
        let entries = std::fs::read_dir(&runs).map_err(|err| CaduceusError::Worktree {
            context: "review-gc",
            stderr: format!("read_dir {}: {err}", runs.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.ends_with(".heartbeat") {
                continue;
            }
            let mtime = match mtime_of(&path) {
                Some(t) => t,
                None => continue,
            };
            if mtime < heartbeat_fresh_cutoff {
                continue;
            }
            let run_id = name.trim_end_matches(".heartbeat");
            let candidate = cfg
                .repo_storage_root
                .join("worktrees")
                .join("review")
                .join(owner)
                .join(repo)
                .join(run_id);
            if candidate.is_dir() {
                paths.insert(canonical_or_raw(&candidate));
            }
        }
    }
    Ok(paths)
}

/// Find unregistered directories under the per-repo review dir that
/// the sweep may remove: regular dirs (not symlinks), not registered,
/// not in use, old enough.
fn collect_review_orphans(
    review_repo_dir: &Path,
    registered_paths: &std::collections::HashSet<PathBuf>,
    in_use: &std::collections::HashSet<PathBuf>,
    age_cutoff: DateTime<Utc>,
) -> Vec<PathBuf> {
    if !review_repo_dir.is_dir() {
        return Vec::new();
    }
    let mut orphans = Vec::new();
    let entries = match std::fs::read_dir(review_repo_dir) {
        Ok(rd) => rd,
        Err(_) => return orphans,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks. The daemon never follows them.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            eprintln!(
                "caduceus worktree-gc: refusing review orphan {}: is a symlink",
                path.display()
            );
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let canonical = canonical_or_raw(&path);
        if registered_paths.contains(&canonical) {
            continue;
        }
        if in_use.contains(&canonical) {
            continue;
        }
        let mtime = match mtime_of(&canonical) {
            Some(t) => t,
            None => continue,
        };
        if mtime > age_cutoff {
            continue;
        }
        orphans.push(path);
    }
    orphans
}

/// mtime as a `DateTime<Utc>`, or `None` if it cannot be determined.
fn mtime_of(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.into())
}

/// Canonicalise a path, falling back to the raw path when the
/// canonical form is unavailable.
fn canonical_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Parse a `watched_repos` entry like `owner/repo` into
/// `(owner, repo)`.
fn parse_watched_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}