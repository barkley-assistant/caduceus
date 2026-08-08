//! Backup retention and state compaction.
//!
//! Prune old backup and corruption-archive files from the state
//! directory, keeping only files within the configured retention
//! window plus active state, claims, checkpoints, and corruption
//! evidence.

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
