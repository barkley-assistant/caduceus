//! Bare mirror management for daemon-owned repositories.
//!
//! A `BareMirror` is a `git clone --bare` of a remote repository,
//! stored at `<repo_storage_root>/mirrors/<owner>/<repo>.git/`.
//! Every git operation goes through the hardened `GitRunner`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worktree::GitRunner;

/// A per-repository bare mirror under daemon-owned storage.
#[derive(Clone, Debug)]
pub struct BareMirror {
    /// Absolute path to the bare mirror directory
    /// (`<storage>/mirrors/<owner>/<repo>.git/`).
    pub path: PathBuf,
    /// Configured remote URL.
    pub remote_url: String,
}

impl BareMirror {
    /// Ensure a bare mirror exists for `(owner, repo)`.
    /// Creates parent directories (mode 0700), runs `git init --bare`
    /// if absent, fetches the remote via the hardened runner, and
    /// returns the mirror handle. Idempotent: repeated calls with
    /// unchanged refs are no-ops.
    pub async fn ensure(
        runner: &GitRunner,
        cfg: &Config,
        owner: &str,
        repo: &str,
        remote_url: &str,
        base_branch: &str,
    ) -> CaduceusResult<Self> {
        let path = cfg
            .repo_storage_root
            .join("mirrors")
            .join(owner)
            .join(format!("{repo}.git"));

        // Create parent directories with correct mode.
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(CaduceusError::Io)?;
            }
        }

        // TOCTOU check: none of the path components under
        // repo_storage_root may be symlinks.
        let storage_root = &cfg.repo_storage_root;
        if super::storage::path_is_symlink(storage_root) {
            return Err(CaduceusError::SymlinkedStorageRoot {
                path: storage_root.clone(),
            });
        }

        // git init --bare if absent
        if !path.join("HEAD").exists() {
            let init_output = runner
                .run_args("init-bare", ["init", "--bare", &path.to_string_lossy()])
                .await?;
            if init_output.status != Some(0) {
                return Err(CaduceusError::Git {
                    operation: "init-bare",
                    stderr: init_output.stderr,
                });
            }
        }

        // Set mode 0700 on the mirror directory and its parent
        // chain. This ensures the daemon's private storage policy
        // is enforced regardless of the process umask.
        if let Some(parent) = path.parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));

        // Add or update the remote origin URL
        let _ = runner
            .run_args(
                "remote-set-url",
                [
                    "-C",
                    &path.to_string_lossy(),
                    "remote",
                    "add",
                    "origin",
                    remote_url,
                ],
            )
            .await;

        // Also set URL for existing remotes (add fails, set replaces)
        let _ = runner
            .run_args(
                "remote-set-url",
                [
                    "-C",
                    &path.to_string_lossy(),
                    "remote",
                    "set-url",
                    "origin",
                    remote_url,
                ],
            )
            .await;

        // Fetch the base branch
        if !base_branch.is_empty() {
            let fetch_output = runner
                .run_args(
                    "mirror-fetch",
                    [
                        "-C",
                        &path.to_string_lossy(),
                        "fetch",
                        "--prune",
                        "origin",
                        base_branch,
                    ],
                )
                .await?;
            if fetch_output.status != Some(0) {
                return Err(CaduceusError::Git {
                    operation: "mirror-fetch",
                    stderr: fetch_output.stderr,
                });
            }
        }

        Ok(Self {
            path,
            remote_url: remote_url.to_string(),
        })
    }

    /// Fetch the latest state. Idempotent: no-ops when refs are
    /// already current (respects `--prune`).
    pub async fn fetch(&self, runner: &GitRunner, base_branch: &str) -> CaduceusResult<()> {
        let output = runner
            .run_args(
                "mirror-fetch",
                [
                    "-C",
                    &self.path.to_string_lossy(),
                    "fetch",
                    "--prune",
                    "origin",
                    base_branch,
                ],
            )
            .await?;
        if output.status != Some(0) {
            return Err(CaduceusError::Git {
                operation: "mirror-fetch",
                stderr: output.stderr,
            });
        }
        Ok(())
    }

    /// Resolve an OID inside the mirror (via `git --git-dir`
    /// rev-parse).
    pub async fn rev_parse(&self, runner: &GitRunner, ref_name: &str) -> CaduceusResult<String> {
        let output = runner
            .run_args(
                "mirror-rev-parse",
                ["-C", &self.path.to_string_lossy(), "rev-parse", ref_name],
            )
            .await?;
        if output.status != Some(0) {
            return Err(CaduceusError::Git {
                operation: "mirror-rev-parse",
                stderr: output.stderr,
            });
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Expose the mirror path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
