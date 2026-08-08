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
    cfg_path: Option<&Path>,
) -> CaduceusResult<SqliteMigrationReport> {
    // Acquire the daemon lock to prevent concurrent ticks. Bind the
    // guard to function scope so it is dropped only after the migration
    // report is built and all I/O has completed.
    let _lock = if lock_policy == LockPolicy::Acquire && !dry_run {
        Some(
            crate::state::queue::DaemonLock::try_acquire(state_dir)?.ok_or_else(|| {
                CaduceusError::Queue {
                    context: "migrate-to-sqlite",
                    stderr: "another tick holds daemon.lock; refusing to migrate".to_string(),
                }
            })?,
        )
    } else {
        None
    };

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

    if json_entries.is_empty() && json_meta.is_empty() {
        return Ok(SqliteMigrationReport {
            outcome: SqliteMigrationOutcome::AlreadyCurrent,
        });
    }

    if dry_run {
        return Ok(SqliteMigrationReport {
            outcome: SqliteMigrationOutcome::DryRun {
                would_migrate: json_entries.len() as u64,
            },
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

    if let Some(path) = cfg_path {
        write_state_backend_config(path, "sqlite", false)?;
    }

    Ok(SqliteMigrationReport {
        outcome: SqliteMigrationOutcome::Migrated {
            entries: json_entries.len() as u64,
        },
    })
}

/// Update `state_backend` in the operator's config file. The file may
/// be a standalone Caduceus YAML document or a Hermes-shaped file with
/// a top-level `caduceus:` section. The change writes through a temp
/// file and atomic rename in the same directory.
pub(crate) fn write_state_backend_config(
    cfg_path: &Path,
    backend: &str,
    dry_run: bool,
) -> CaduceusResult<()> {
    if dry_run {
        println!(
            "caduceus migrate-state: dry-run; would set state_backend: {} in {}",
            backend,
            cfg_path.display()
        );
        return Ok(());
    }

    let text = std::fs::read_to_string(cfg_path).map_err(|e| {
        CaduceusError::Config(format!("failed to read config {}: {e}", cfg_path.display()))
    })?;
    let mut outer: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| {
        CaduceusError::Config(format!(
            "failed to parse config {}: {e}",
            cfg_path.display()
        ))
    })?;
    let is_hermes_shape = outer
        .as_mapping()
        .map(|m| m.contains_key("caduceus"))
        .unwrap_or(false);

    fn set_state_backend(mapping: &mut serde_yaml::Mapping, backend: &str) {
        mapping.insert(
            serde_yaml::Value::String("state_backend".to_string()),
            serde_yaml::Value::String(backend.to_string()),
        );
    }

    if is_hermes_shape {
        if let Some(serde_yaml::Value::Mapping(caduceus)) = outer.get_mut("caduceus") {
            set_state_backend(caduceus, backend);
        }
    } else if let Some(mapping) = outer.as_mapping_mut() {
        if mapping.contains_key("caduceus") {
            return Err(CaduceusError::Config(format!(
                "config {} looks Hermes-shaped but caduceus section is malformed",
                cfg_path.display()
            )));
        }
        set_state_backend(mapping, backend);
    } else {
        return Err(CaduceusError::Config(format!(
            "config {} is not a YAML mapping",
            cfg_path.display()
        )));
    }

    let new_text = serde_yaml::to_string(&outer).map_err(|e| {
        CaduceusError::Config(format!(
            "failed to serialize config {}: {e}",
            cfg_path.display()
        ))
    })?;
    let tmp = cfg_path.with_extension("yaml.tmp");
    std::fs::write(&tmp, new_text).map_err(|e| {
        CaduceusError::Config(format!(
            "failed to write temp config {}: {e}",
            tmp.display()
        ))
    })?;
    std::fs::rename(&tmp, cfg_path).map_err(|e| {
        CaduceusError::Config(format!(
            "failed to replace config {}: {e}",
            cfg_path.display()
        ))
    })?;
    Ok(())
}
