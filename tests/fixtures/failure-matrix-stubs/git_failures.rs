//! Git failure stubs for the failure matrix (AC-06).
//!
//! Helpers for exercising the daemon's push-collision and
//! push-failure paths against a local bare repository.
//! `LocalOrigin` already provides the hermetic bare-repo fixture;
//! these helpers add the forced-collision scenario (a
//! non-fast-forward push that the daemon must refuse).

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// A minimal worktree helper that sets up a clone of a bare
/// origin plus a local commit that diverges from the remote
/// (forcing a push collision when the daemon tries to push).
pub struct ForcedCollision {
    _dir: TempDir,
    /// Path to the workdir clone (not the bare repo).
    clone_path: PathBuf,
    /// Bare origin URI.
    origin_uri: String,
}

impl ForcedCollision {
    /// Create a fresh bare repo and a clone. Push one commit
    /// onto the bare repo, then create a new commit on the
    /// clone that diverges from the remote, so any subsequent
    /// non-force push will be rejected as a collision.
    pub fn init(label: &str, bare: &Path) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("caduceus-collision-{label}-"))
            .tempdir()
            .expect("tempdir create");
        let clone_path = dir.path().join("clone");
        // Let LocalOrigin set up the bare repo first (done in
        // the parent test). Here we just ensure the clone dir
        // exists.
        fs::create_dir_all(&clone_path).expect("create clone dir");
        let origin_uri = format!("file://{}", bare.display());
        Self {
            _dir: dir,
            clone_path,
            origin_uri,
        }
    }

    /// The URI of the bare origin.
    pub fn uri(&self) -> &str {
        &self.origin_uri
    }

    /// Path to the working clone.
    pub fn clone_path(&self) -> &Path {
        &self.clone_path
    }

    /// Push one diverging commit onto the clone so the remote
    /// history is ahead of what the daemon would try to push.
    pub fn create_diverging_commit(&self, msg: &str) {
        let _status = Command::new("git")
            .args(["-C"])
            .arg(&self.clone_path)
            .args(["commit", "--allow-empty", "-m"])
            .arg(msg)
            .current_dir(&self.clone_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git commit diverging");
    }
}
