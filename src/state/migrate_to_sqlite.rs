//! SQLite migration support — `caduceus migrate-state --to-sqlite`.
//!
//! This module adds the v1.0 migration path that reads the current
//! JSON queue state and metadata and imports them into the SQLite
//! store in one transaction.

use std::path::Path;

use chrono::Utc;
use rusqlite::params;

use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::store;

/// Whether to acquire the daemon lock during migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPolicy {
    /// Acquire the daemon lock (normal operation).
    Acquire,
    /// Skip the lock (test-only, requires a lock guard from the caller).
    Skip,
}

/// Outcome of a SQLite migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteMigrationOutcome {
    /// Entries were migrated to SQLite.
    Migrated { entries: u64 },
    /// Dry-run: nothing was modified.
    DryRun { would_migrate: u64 },
    /// The SQLite store is already current.
    AlreadyCurrent,
}

/// Result of [`migrate_to_sqlite`].
#[derive(Debug, Clone)]
pub struct SqliteMigrationReport {
    pub outcome: SqliteMigrationOutcome,
}

/// Migrate the JSON state in `state_dir` to the SQLite store.
///
/// 1. Acquires the daemon lock (unless `lock_policy` is `Skip`).
/// 2. Reads the current JSON queue state and metadata.
/// 3. Opens the SQLite store (creates schema if needed).
/// 4. Imports queue entries and metadata in a single transaction.
///
/// On success, the SQLite store is the active backend and the
/// JSON files are left untouched (they serve as a validated backup).
pub fn migrate_to_sqlite(
    state_dir: &Path,
    dry_run: bool,
    lock_policy: LockPolicy,
) -> CaduceusResult<SqliteMigrationReport> {
    // Acquire the daemon lock to prevent concurrent ticks.
    if lock_policy == LockPolicy::Acquire && !dry_run {
        let _lock = crate::state::queue::DaemonLock::try_acquire(state_dir)?.ok_or_else(|| {
            CaduceusError::Queue {
                context: "migrate-to-sqlite",
                stderr: "another tick holds daemon.lock; refusing to migrate".to_string(),
            }
        })?;
    }

    // Read JSON queue state.
    let state_path = state_dir.join(crate::state::queue::STATE_FILENAME);
    let json_entries: Vec<(String, serde_json::Value)> = if state_path.exists() {
        let body = std::fs::read(&state_path).map_err(|e| CaduceusError::StateCorrupt {
            path: state_path.clone(),
            message: format!("cannot read queue state: {e}"),
        })?;
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| CaduceusError::StateCorrupt {
                path: state_path.clone(),
                message: format!("cannot parse queue state JSON: {e}"),
            })?;
        parsed
            .get("entries")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Read JSON metadata.
    let meta_path = state_dir.join("state_meta.json");
    let json_meta: Vec<(String, String)> = if meta_path.exists() {
        let body = std::fs::read(&meta_path).map_err(|e| CaduceusError::StateCorrupt {
            path: meta_path.clone(),
            message: format!("cannot read state metadata: {e}"),
        })?;
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| CaduceusError::StateCorrupt {
                path: meta_path.clone(),
                message: format!("cannot parse state metadata JSON: {e}"),
            })?;
        parsed
            .as_object()
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), serde_json::to_string(v).unwrap_or_default()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    if dry_run {
        return Ok(SqliteMigrationReport {
            outcome: SqliteMigrationOutcome::DryRun {
                would_migrate: json_entries.len() as u64,
            },
        });
    }

    if json_entries.is_empty() && json_meta.is_empty() {
        return Ok(SqliteMigrationReport {
            outcome: SqliteMigrationOutcome::AlreadyCurrent,
        });
    }

    // Open the SQLite store.
    let conn = store::open_in(state_dir)?;

    // Import in a single transaction.
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CaduceusError::StateCorrupt {
            path: state_dir.join(store::DB_FILENAME),
            message: format!("cannot start migration transaction: {e}"),
        })?;

    let now = Utc::now().to_rfc3339();

    for (key, value) in &json_entries {
        let phase = value
            .get("phase")
            .and_then(|v| v.as_str())
            .unwrap_or("queued")
            .to_string();
        let ticket_type = value
            .get("ticket_type")
            .and_then(|v| v.as_str())
            .unwrap_or("code")
            .to_string();
        let attempts: i64 = value.get("attempts").and_then(|v| v.as_i64()).unwrap_or(0);
        let last_error = value
            .get("last_error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let last_run_id = value
            .get("last_run_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let next_attempt_at = value
            .get("next_attempt_at")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let finalization = value.get("finalization").map(|v| v.to_string());
        let queued_at = value
            .get("queued_at")
            .and_then(|v| v.as_str())
            .unwrap_or(&now)
            .to_string();
        let updated_at = value
            .get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or(&now)
            .to_string();

        tx.execute(
            "INSERT OR REPLACE INTO queue_entries
             (issue_key, phase, ticket_type, attempts, last_error, last_run_id,
              next_attempt_at, finalization, queued_at, updated_at, generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                key,
                phase,
                ticket_type,
                attempts,
                last_error,
                last_run_id,
                next_attempt_at,
                finalization,
                queued_at,
                updated_at,
                1_i64,
            ],
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: state_dir.join(store::DB_FILENAME),
            message: format!("cannot insert queue entry {key}: {e}"),
        })?;
    }

    for (k, v) in &json_meta {
        tx.execute(
            "INSERT OR REPLACE INTO state_meta (key, value) VALUES (?1, ?2)",
            params![k, v],
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: state_dir.join(store::DB_FILENAME),
            message: format!("cannot insert meta key {k}: {e}"),
        })?;
    }

    tx.commit().map_err(|e| CaduceusError::StateCorrupt {
        path: state_dir.join(store::DB_FILENAME),
        message: format!("cannot commit migration transaction: {e}"),
    })?;

    Ok(SqliteMigrationReport {
        outcome: SqliteMigrationOutcome::Migrated {
            entries: json_entries.len() as u64,
        },
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn state_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("sqlite-migrate-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_state_json(dir: &Path, body: &str) {
        fs::write(dir.join(crate::state::queue::STATE_FILENAME), body).expect("write state.json");
    }

    #[test]
    fn migrate_empty_state_is_already_current() {
        let dir = state_dir();
        let report = migrate_to_sqlite(&dir, false, LockPolicy::Skip).expect("migrate empty state");
        assert_eq!(report.outcome, SqliteMigrationOutcome::AlreadyCurrent);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_dry_run_reports_count() {
        let dir = state_dir();
        let state_body = serde_json::json!({
            "version": 1,
            "entries": {
                "owner/repo#1": {
                    "phase": "queued", "ticket_type": "code", "attempts": 0,
                    "queued_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
                }
            }
        })
        .to_string();
        write_state_json(&dir, &state_body);

        let report = migrate_to_sqlite(&dir, true, LockPolicy::Skip).expect("dry run");
        assert_eq!(
            report.outcome,
            SqliteMigrationOutcome::DryRun { would_migrate: 1 }
        );
        assert!(
            !dir.join(store::DB_FILENAME).exists(),
            "SQLite store must not exist after dry run"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_populated_state_creates_sqlite_store() {
        let dir = state_dir();
        let state_body = serde_json::json!({
            "version": 1,
            "entries": {
                "owner/repo#1": {
                    "phase": "queued", "ticket_type": "code", "attempts": 0,
                    "queued_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
                },
                "owner/repo#2": {
                    "phase": "in_progress", "ticket_type": "investigation", "attempts": 1,
                    "last_error": "timeout",
                    "queued_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z"
                }
            }
        })
        .to_string();
        write_state_json(&dir, &state_body);

        let report = migrate_to_sqlite(&dir, false, LockPolicy::Skip).expect("migrate");
        assert_eq!(
            report.outcome,
            SqliteMigrationOutcome::Migrated { entries: 2 }
        );

        assert!(
            dir.join(store::DB_FILENAME).is_file(),
            "SQLite store must exist"
        );
        let conn = store::open_in(&dir).expect("open store");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue_entries", [], |row| row.get(0))
            .expect("count entries");
        assert_eq!(count, 2, "must have 2 queue entries in SQLite");

        let phase: String = conn
            .query_row(
                "SELECT phase FROM queue_entries WHERE issue_key = ?1",
                params!["owner/repo#1"],
                |row| row.get(0),
            )
            .expect("read phase");
        assert_eq!(phase, "queued");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_preserves_json_state_files() {
        let dir = state_dir();
        let state_body = serde_json::json!({
            "version": 1,
            "entries": {
                "owner/repo#1": {
                    "phase": "queued", "ticket_type": "code", "attempts": 0,
                    "queued_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
                }
            }
        })
        .to_string();
        write_state_json(&dir, &state_body);

        migrate_to_sqlite(&dir, false, LockPolicy::Skip).expect("migrate");
        assert!(
            dir.join(crate::state::queue::STATE_FILENAME).is_file(),
            "JSON state must be preserved as backup"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_when_already_current_is_noop() {
        let dir = state_dir();
        let report = migrate_to_sqlite(&dir, false, LockPolicy::Skip).expect("migrate empty");
        assert_eq!(report.outcome, SqliteMigrationOutcome::AlreadyCurrent);
        let _ = fs::remove_dir_all(&dir);
    }
}
