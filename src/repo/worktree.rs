//! Disposable worktrees for per-attempt runs.
//!
//! A `repo::Worktree` is created via `git worktree add --detach`
//! against a `BareMirror` and lives at
//! `<repo_storage_root>/worktrees/<run_id>/`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worktree::GitRunner;

use super::mirror::BareMirror;

/// A disposable worktree created from a daemon-owned bare mirror.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// The bare mirror this worktree was created from.
    pub mirror: BareMirror,
    /// Run ID (used as the worktree directory basename).
    pub run_id: String,
    /// Absolute path to the worktree
    /// (`<storage_root>/worktrees/<run_id>/`).
    pub path: PathBuf,
    /// SHA-1 of the base commit the worktree was created from.
    pub base_oid: String,
    /// When this worktree was created.
    pub created_at: DateTime<Utc>,
}

/// Run ID validation: only ASCII alphanumeric, underscore, dash;
/// non-empty; max 64 chars.
fn validate_run_id(run_id: &str) -> CaduceusResult<()> {
    if run_id.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "repo-create",
            stderr: "run_id must not be empty".to_string(),
        });
    }
    if run_id.len() > 64 {
        return Err(CaduceusError::Worktree {
            context: "repo-create",
            stderr: format!("run_id length {} exceeds 64-char limit", run_id.len()),
        });
    }
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CaduceusError::Worktree {
            context: "repo-create",
            stderr: format!("run_id {run_id:?} contains invalid characters"),
        });
    }
    Ok(())
}

impl Worktree {
    /// Create a new disposable worktree from `mirror`.
    /// The worktree is created at `<storage_root>/worktrees/<run_id>/`.
    ///
    /// The umask is switched to `0o022` for the worktree-add call
    /// and restored to `0o077` afterwards so source file modes
    /// are preserved inside the worktree.
    pub async fn create(
        runner: &GitRunner,
        mirror: &BareMirror,
        run_id: &str,
        base_oid: &str,
    ) -> CaduceusResult<Self> {
        validate_run_id(run_id)?;

        // Resolve the storage root from the mirror path:
        // <storage>/mirrors/<owner>/<repo>.git/
        //  → up to <storage>/worktrees/<run_id>/
        let storage_root = mirror
            .path
            .parent() // <owner>/
            .and_then(|p| p.parent()) // mirrors/
            .and_then(|p| p.parent()) // <storage>/
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                CaduceusError::Config("cannot resolve storage root from mirror path".to_string())
            })?;

        let worktrees_dir = storage_root.join("worktrees");
        let worktree_path = worktrees_dir.join(run_id);

        // Refuse to reuse an existing worktree path
        if worktree_path.exists() {
            return Err(CaduceusError::WorktreeReuseAfterFailure {
                run_id: run_id.to_string(),
                worktree_path,
                last_state: "exists".to_string(),
            });
        }

        std::fs::create_dir_all(&worktrees_dir).map_err(|err| CaduceusError::Worktree {
            context: "repo-create",
            stderr: format!(
                "create worktree parent {} failed: {err}",
                worktrees_dir.display()
            ),
        })?;

        let path_str = worktree_path.to_string_lossy().into_owned();

        // Run git worktree add under umask 0o022 to preserve
        // source-file executable bits. The umask is set before
        // the async call and restored after. The spawn in the
        // runner's `run_in` is synchronous (it calls
        // `command.spawn()` before any await points), so the
        // child process inherits the temporary umask.
        let prev = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
        let add_output = runner
            .run_args(
                "repo-worktree-add",
                [
                    "-C",
                    &mirror.path.to_string_lossy(),
                    "worktree",
                    "add",
                    "--detach",
                    &path_str,
                    base_oid,
                ],
            )
            .await?;
        nix::sys::stat::umask(prev);

        if add_output.cancelled {
            return Err(CaduceusError::Cancelled);
        }
        if add_output.timed_out || add_output.status != Some(0) {
            return Err(CaduceusError::Worktree {
                context: "repo-create",
                stderr: format!(
                    "git worktree add --detach {} {} failed: {}",
                    worktree_path.display(),
                    base_oid,
                    add_output.stderr
                ),
            });
        }

        Ok(Self {
            mirror: mirror.clone(),
            run_id: run_id.to_string(),
            path: worktree_path,
            base_oid: base_oid.to_string(),
            created_at: Utc::now(),
        })
    }

    /// Remove the disposable worktree. Runs `git worktree remove
    /// --force` from the mirror and cleans up the directory.
    pub async fn remove(runner: &GitRunner, worktree: &Self) -> CaduceusResult<()> {
        let path_str = worktree.path.to_string_lossy().into_owned();
        let output = runner
            .run_args(
                "repo-worktree-remove",
                [
                    "-C",
                    &worktree.mirror.path.to_string_lossy(),
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

        // git worktree remove --force returns nonzero for filesystem
        // errors. Clean up what we can regardless.
        if worktree.path.exists() {
            std::fs::remove_dir_all(&worktree.path).map_err(|err| CaduceusError::Worktree {
                context: "repo-remove",
                stderr: format!(
                    "fs remove_dir_all {} failed: {err}",
                    worktree.path.display()
                ),
            })?;
        }

        Ok(())
    }
}
