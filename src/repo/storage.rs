//! Root-level storage coordination for daemon-owned repositories.
//!
//! Owns the `repo_storage_root` path and provides TOCTOU-resistant
//! validation and directory initialisation.

use std::path::{Path, PathBuf};

use crate::infra::error::{CaduceusError, CaduceusResult};

/// Root-level storage coordinator for daemon-owned repositories.
#[derive(Clone, Debug)]
pub struct Storage {
    /// The configured `repo_storage_root`.
    pub repo_storage_root: PathBuf,
    /// `mirrors/` subdirectory under the root.
    pub mirrors_dir: PathBuf,
    /// `worktrees/` subdirectory under the root.
    pub worktrees_dir: PathBuf,
}

impl Storage {
    /// Build a `Storage` from the configured `repo_storage_root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            mirrors_dir: root.join("mirrors"),
            worktrees_dir: root.join("worktrees"),
            repo_storage_root: root,
        }
    }

    /// Validate that `repo_storage_root` is not now a symlink
    /// (TOCTOU countermeasure — called at startup AND on every
    /// storage access). Returns `SymlinkedStorageRoot` when the
    /// root or any controlled subdirectory has been replaced with
    /// a symlink.
    pub fn validate_root(&self) -> CaduceusResult<()> {
        if is_symlink(&self.repo_storage_root) {
            return Err(CaduceusError::SymlinkedStorageRoot {
                path: self.repo_storage_root.clone(),
            });
        }
        if self.mirrors_dir.exists() && is_symlink(&self.mirrors_dir) {
            return Err(CaduceusError::SymlinkedStorageRoot {
                path: self.mirrors_dir.clone(),
            });
        }
        if self.worktrees_dir.exists() && is_symlink(&self.worktrees_dir) {
            return Err(CaduceusError::SymlinkedStorageRoot {
                path: self.worktrees_dir.clone(),
            });
        }
        Ok(())
    }

    /// Create `mirrors/` and `worktrees/` with mode `0700`.
    /// Idempotent: existing directories are left unchanged, but
    /// their mode is verified.
    pub fn ensure_dirs(&self) -> CaduceusResult<()> {
        self.validate_root()?;
        // Ensure the repo_storage_root itself has mode 0700.
        std::fs::create_dir_all(&self.repo_storage_root).map_err(CaduceusError::Io)?;
        set_mode_0700(&self.repo_storage_root)?;
        for dir in [&self.mirrors_dir, &self.worktrees_dir] {
            std::fs::create_dir_all(dir).map_err(CaduceusError::Io)?;
            set_mode_0700(dir)?;
        }
        Ok(())
    }
}

/// Check if `path` is a symlink (uses `symlink_metadata` to
/// avoid following the link).
fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Public helper so other modules can check symlink status on
/// any path.
pub fn path_is_symlink(path: &Path) -> bool {
    is_symlink(path)
}

/// Set a directory's permissions to `0700`. Returns
/// `ModeNotPreserved` if the filesystem does not honour the
/// mode (e.g. FAT mount, FUSE backend).
fn set_mode_0700(path: &Path) -> CaduceusResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, perms).map_err(CaduceusError::Io)?;
    let meta = std::fs::metadata(path).map_err(CaduceusError::Io)?;
    let observed = meta.permissions().mode() & 0o777;
    if observed != 0o700 {
        return Err(CaduceusError::ModeNotPreserved {
            path: path.to_path_buf(),
            expected: 0o700,
            observed,
        });
    }
    Ok(())
}
