//! Backup retention and state compaction.
//!
//! Prune old backup and corruption-archive files from the state
//! directory, keeping only files within the configured retention
//! window plus active state, claims, checkpoints, and corruption
//! evidence. Also prunes finished-run artifacts (transcripts,
//! archived results, dry-run previews, stale heartbeats) from
//! `<state_dir>/runs/`, with a heartbeat-freshness guard that
//! mirrors the worktree-GC liveness rule so in-flight runs are
//! never touched.

use std::fs;
use std::path::Path;

use crate::infra::error::CaduceusResult;

/// Prune backup and corruption-archive files older than
/// `retention_days`. Preserves:
///
/// - Active queue state (`state.json`, `state.db`)
/// - Active metadata (`state_meta.json`)
/// - Active claims (`claims/`)
/// - Active checkpoints (`checkpoints/`)
/// - Corruption evidence markers (`*.corrupt` without timestamp)
///
/// Eligible for pruning:
///
/// - Timestamped backups (`state.json.bak-<ts>`)
/// - Timestamped corruption archives (`state.json.corrupt-<ts>`,
///   `state.db.corrupt-<ts>`, `state_meta.json.corrupt-<ts>`)
///
/// Returns the number of pruned files.
pub fn prune_backups(state_dir: &Path, retention_days: u64) -> CaduceusResult<u64> {
    // A window reaching back past the epoch (huge
    // `run_retention_days`) means nothing can be older than it:
    // prune nothing. `checked_sub` returns None there; the plain
    // `Sub` impl panics ("overflow when subtracting duration from
    // SystemTime", both profiles — verified rustc 1.97.1), which
    // would crash every tick.
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
        retention_days.saturating_mul(86400),
    )) else {
        return Ok(0);
    };

    let mut pruned = 0u64;

    let Ok(entries) = fs::read_dir(state_dir) else {
        return Ok(0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Only prune timestamped backup/archive files. The classes
        // match the daemon's own writers (migrate.rs install/recover
        // arms, meta.rs quarantine). Operators' manual backups
        // follow the wiki's `state.db.backup-*` convention and must
        // never match. Untimed markers and active files are never
        // touched.
        let is_backup = name.starts_with("state.json.bak-")
            || name.starts_with("state.json.corrupt-")
            || name.starts_with("state.db.corrupt-")
            || name.starts_with("state_meta.json.corrupt-");

        if !is_backup {
            continue;
        }

        // Check file age.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };

        if modified < cutoff {
            let _ = fs::remove_file(&path);
            pruned += 1;
        }
    }

    Ok(pruned)
}

/// Prune finished-run artifacts from `<state_dir>/runs/` older
/// than `retention_days` (issue #403). Eligible classes, by
/// suffix — the run-id prefix is an opaque ULID:
///
/// - Worker transcripts (`<run_id>.log`)
/// - Archived results (`<run_id>.result.json`)
/// - Dry-run previews (`<run_id>.preview.json`,
///   `<run_id>.dry-run.md`)
/// - Heartbeats (`<run_id>.heartbeat`) — only stale ones; see
///   the freshness guard below.
///
/// Safety (mirrors the `worktree/gc.rs` liveness rule): a live
/// run's supervisor rewrites its heartbeat at most once per
/// second and appends to its transcript continuously, so both
/// always carry a fresh mtime. Any file whose mtime is within
/// the last hour is skipped unconditionally — the sweep can
/// never delete an in-flight run's heartbeat and make the
/// worktree GC reap a live worktree (the #177 bug class). A
/// heartbeat older than the cutoff is already inert for GC
/// (it skips stale heartbeats), so pruning it changes no GC
/// decision.
///
/// Preserves: anything not matching the five suffixes (operator
/// drops, foreign files), directories, symlinks, and atomic-write
/// temporaries (they end in `.tmp.<pid>.<nanos>`, never in a
/// matched suffix).
///
/// Returns the number of pruned files. `Ok(0)` when the `runs/`
/// directory is missing.
pub fn prune_run_artifacts(state_dir: &Path, retention_days: u64) -> CaduceusResult<u64> {
    let now = std::time::SystemTime::now();
    // Same huge-window rule as `prune_backups`: a window reaching
    // back past the epoch means nothing can be older than it —
    // prune nothing rather than panicking.
    let Some(cutoff) = now.checked_sub(std::time::Duration::from_secs(
        retention_days.saturating_mul(86400),
    )) else {
        return Ok(0);
    };
    // Heartbeat-freshness floor mirroring `worktree/gc.rs` (1h):
    // files modified within the last hour are never eligible,
    // regardless of the retention window. `checked_sub` again so
    // a broken pre-epoch clock prunes nothing instead of
    // panicking.
    let Some(fresh_floor) = now.checked_sub(std::time::Duration::from_secs(3600)) else {
        return Ok(0);
    };

    let runs_dir = state_dir.join("runs");
    let Ok(entries) = fs::read_dir(&runs_dir) else {
        return Ok(0);
    };

    let mut pruned = 0u64;

    for entry in entries.flatten() {
        let path = entry.path();
        // `entry.file_type()` stats the link itself (no follow):
        // a symlinked artifact is never removed, matching the
        // status reader's symlink rejection. Directories are
        // skipped the same way.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        let is_run_artifact = name.ends_with(".log")
            || name.ends_with(".result.json")
            || name.ends_with(".heartbeat")
            || name.ends_with(".preview.json")
            || name.ends_with(".dry-run.md");

        if !is_run_artifact {
            continue;
        }

        // Check file age.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };

        // In-flight guard: never touch anything a live run may
        // still be writing, regardless of the window.
        if modified >= fresh_floor {
            continue;
        }

        if modified < cutoff {
            let _ = fs::remove_file(&path);
            pruned += 1;
        }
    }

    Ok(pruned)
}
