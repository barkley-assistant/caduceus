//! Attic for preserved failed-run worktrees.
//!
//! When `cfg.archive_on_retry` is enabled, the daemon archives the
//! working tree of a previous failed attempt before removing it.
//! The attic lives under `<state_dir>/attic`. A retention sweep runs
//! on every daemon pulse and removes archives older than
//! `cfg.attic_retention_days`.
//!
//! Archives are plain `.tar` files. The repository does not depend on
//! `zstd`, so compression is intentionally omitted per the issue #177
//! scope ("use plain `.tar` if `zstd` is not already a dependency").

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Return the daemon's attic directory: `<state_dir>/attic`.
pub(crate) fn attic_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir.join("attic")
}

/// Build an archive file name for a previous attempt of an issue.
fn archive_name(owner: &str, repo: &str, issue_number: u64, run_id: &str, now: u64) -> String {
    format!("{owner}-{repo}-{issue_number}-{run_id}-{now}.tar")
}

/// Archive a working tree to the attic.
///
/// The resulting file is named `<owner>-<repo>-<issue>-<run_id>-<unix_ts>.tar`
/// and lives under `<state_dir>/attic`. The source tree is captured
/// under a top-level directory named after its basename (the run id)
/// so extraction recreates the original layout.
pub async fn archive(
    cfg: &Config,
    owner: &str,
    repo: &str,
    issue_number: u64,
    run_id: &str,
    source: &Path,
) -> CaduceusResult<PathBuf> {
    let dir = attic_dir(cfg);
    fs::create_dir_all(&dir).map_err(|err| CaduceusError::Worktree {
        context: "attic-archive",
        stderr: format!("create attic dir {}: {err}", dir.display()),
    })?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let name = archive_name(owner, repo, issue_number, run_id, now);
    let dest = dir.join(&name);

    let file = fs::File::create(&dest).map_err(|err| CaduceusError::Worktree {
        context: "attic-archive",
        stderr: format!("create archive {}: {err}", dest.display()),
    })?;

    let mut builder = tar::Builder::new(file);
    let basename = source
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("worktree"));
    builder
        .append_dir_all(basename, source)
        .map_err(|err| CaduceusError::Worktree {
            context: "attic-archive",
            stderr: format!("archive {} -> {}: {err}", source.display(), dest.display()),
        })?;
    builder.finish().map_err(|err| CaduceusError::Worktree {
        context: "attic-archive",
        stderr: format!("finish archive {}: {err}", dest.display()),
    })?;

    Ok(dest)
}

/// Remove attic archives older than `cfg.attic_retention_days`.
///
/// Returns the number of files removed. Non-file entries are ignored.
/// Errors deleting individual archives are logged but do not abort the
/// sweep.
pub async fn sweep(cfg: &Config) -> CaduceusResult<usize> {
    let dir = attic_dir(cfg);
    if !dir.is_dir() {
        return Ok(0);
    }

    let cutoff = Utc::now() - chrono::Duration::days(cfg.attic_retention_days as i64);
    let mut removed = 0usize;

    let entries = fs::read_dir(&dir).map_err(|err| CaduceusError::Worktree {
        context: "attic-sweep",
        stderr: format!("read attic dir {}: {err}", dir.display()),
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(modified) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        else {
            continue;
        };
        let mtime =
            DateTime::<Utc>::from_timestamp(modified.as_secs() as i64, 0).unwrap_or_else(Utc::now);
        if mtime < cutoff {
            if let Err(err) = fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), error = %err, "failed to remove stale attic archive");
            } else {
                removed += 1;
                tracing::info!(path = %path.display(), "removed stale attic archive");
            }
        }
    }

    Ok(removed)
}
