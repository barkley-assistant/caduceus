//! Backup retention and state compaction.
//!
//! Prune old backup and corruption-archive files from the state
//! directory, keeping only files within the configured retention
//! window plus active state, claims, checkpoints, and corruption
//! evidence.

use std::fs;
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use crate::error::CaduceusResult;

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
/// - Timestamped backups (`state.json.bak-<ts>`, `state.db.bak-<ts>`)
/// - Timestamped corruption archives (`state.json.corrupt-<ts>`,
///   `state.db.corrupt-<ts>`)
///
/// Returns the number of pruned files.
pub fn prune_backups(state_dir: &Path, retention_days: u64) -> CaduceusResult<u64> {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retention_days * 86400);

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

        // Only prune timestamped backup/archive files.
        let is_backup = name.starts_with("state.json.bak-")
            || name.starts_with("state.db.bak-")
            || name.starts_with("state.json.corrupt-")
            || name.starts_with("state.db.corrupt-");

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("retention-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn prune_removes_old_backups() {
        let d = dir();

        // Create a backup file with an old timestamp.
        let old_backup = d.join("state.json.bak-1000000");
        fs::write(&old_backup, b"old").unwrap();
        // Set its modified time to 30 days ago.
        let old_time = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
        );
        filetime::set_file_mtime(&old_backup, old_time).unwrap();

        // Create a recent backup (within retention window).
        let recent_backup = d.join("state.json.bak-9999999999");
        fs::write(&recent_backup, b"recent").unwrap();

        let count = prune_backups(&d, 7).expect("prune");
        assert_eq!(count, 1, "only old backup should be pruned");

        assert!(!old_backup.exists(), "old backup must be removed");
        assert!(recent_backup.exists(), "recent backup must be kept");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn prune_preserves_active_state() {
        let d = dir();

        // Active state files must never be pruned.
        fs::write(d.join("state.json"), b"active").unwrap();
        fs::write(d.join("state.db"), b"active").unwrap();
        fs::write(d.join("state_meta.json"), b"active").unwrap();

        let count = prune_backups(&d, 7).expect("prune");
        assert_eq!(count, 0, "no backup files to prune");

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn prune_preserves_untimed_corrupt_marker() {
        let d = dir();

        // An untimed corruption marker (no timestamp) must be preserved.
        fs::write(d.join("state.json.corrupt"), b"marker").unwrap();
        // But a timed one can be pruned.
        let old = d.join("state.db.corrupt-1000000");
        fs::write(&old, b"old").unwrap();
        let old_time = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
        );
        filetime::set_file_mtime(&old, old_time).unwrap();

        let count = prune_backups(&d, 7).expect("prune");
        assert_eq!(count, 1, "only timed corrupt archive should be pruned");

        assert!(
            d.join("state.json.corrupt").exists(),
            "untimed marker must be kept"
        );

        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn prune_empty_dir_returns_zero() {
        let d = dir();
        let count = prune_backups(&d, 7).expect("prune empty");
        assert_eq!(count, 0);
        let _ = fs::remove_dir_all(&d);
    }
}
