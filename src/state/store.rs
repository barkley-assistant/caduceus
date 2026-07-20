//! Versioned SQLite state store — the v1 runtime backend.
//!
//! Every stateful operation (queue, metadata, claims, checkpoints,
//! circuit breakers) goes through this store. The schema is versioned
//! so the daemon can refuse an unknown schema before mutating data.
//!
//! ## Schema (v1)
//!
//! - `schema_version` — single-row version table (envelope check).
//! - `queue_entries` — one row per tracked issue (replaces
//!   `state.json`).
//! - `state_meta` — arbitrary key-value metadata (replaces
//!   `state_meta.json`).
//! - `claims` — per-issue lease tokens (replaces files under
//!   `<state_dir>/claims/`).
//! - `checkpoints` — durable finalization checkpoints per run.
//! - `circuit_breakers` — per-issue failure tracking.

use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use rusqlite::{params, Connection, Transaction};

use crate::infra::error::{CaduceusError, CaduceusResult};

/// Current schema version. Bumping it is a breaking change — the
/// store refuses to open a database with a *higher* version.
///
/// ## v2 (Task 4.2)
///
/// - `checkpoints` table gains `operation_id TEXT` and
///   `remote_marker TEXT` columns for durable operation IDs and
///   remote reconciliation markers. Existing rows get NULL.
///
/// ## v3 (Task 5.1)
///
/// - `leases` table for per-issue fenced leases with fencing
///   tokens, owner tracking, and expiry.
pub const SCHEMA_VERSION: i64 = 3;

/// Name of the SQLite database file inside the state directory.
pub const DB_FILENAME: &str = "state.db";

// ---------------------------------------------------------------------------
// Schema DDL (applied atomically at open time)
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER NOT NULL,
    migrated_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS queue_entries (
    issue_key     TEXT PRIMARY KEY,
    phase         TEXT NOT NULL,
    ticket_type   TEXT NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    last_run_id   TEXT,
    next_attempt_at TEXT,
    finalization  TEXT,
    queued_at     TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    generation    INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS state_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS claims (
    claim_id   TEXT PRIMARY KEY,
    issue_key  TEXT NOT NULL,
    worker_pid INTEGER,
    token      TEXT NOT NULL,
    claimed_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    FOREIGN KEY (issue_key) REFERENCES queue_entries(issue_key)
);

CREATE TABLE IF NOT EXISTS checkpoints (
    run_id          TEXT NOT NULL,
    stage           TEXT NOT NULL,
    checkpoint_data TEXT,
    created_at      TEXT NOT NULL,
    operation_id    TEXT,
    remote_marker   TEXT,
    PRIMARY KEY (run_id, stage)
);

CREATE TABLE IF NOT EXISTS circuit_breakers (
    issue_key      TEXT PRIMARY KEY,
    failure_count  INTEGER NOT NULL DEFAULT 0,
    last_failure_at TEXT,
    opened_at      TEXT,
    FOREIGN KEY (issue_key) REFERENCES queue_entries(issue_key)
);

CREATE TABLE IF NOT EXISTS leases (
    issue_key     TEXT PRIMARY KEY,
    owner_id      TEXT NOT NULL,
    fencing_token INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    state         TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired'))
);
";

// ---------------------------------------------------------------------------
// Open / initialise
// ---------------------------------------------------------------------------

/// Open or create a versioned SQLite database at `path`. If the
/// database is new it is initialised with the current schema. If
/// it already exists the schema version is checked:
///
/// - Equal to [`SCHEMA_VERSION`] → open, apply any missing tables.
/// - Higher than [`SCHEMA_VERSION`] → reject with
///   [`CaduceusError::StateCorrupt`] (future schema, must upgrade).
/// - Lower → migration is performed (future task).
///
/// The connection uses WAL mode for concurrent reads and is created
/// with `PRAGMA journal_mode=WAL`.
pub fn open(path: &Path) -> CaduceusResult<Connection> {
    let db_path = path.to_path_buf();
    let conn = Connection::open(path).map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.clone(),
        message: format!("cannot open SQLite store at {}: {e}", path.display()),
    })?;

    // Enable WAL mode for read concurrency.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot set pragmas: {e}"),
        })?;

    // Check / initialise schema version.
    let table_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.clone(),
            message: format!("cannot probe schema_version table: {e}"),
        })?;

    if table_count == 0 {
        // Fresh database — initialise schema.
        init_schema(&conn, &db_path)?;
    } else {
        // Existing database — check version.
        let existing_version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .map_err(|e| CaduceusError::StateCorrupt {
                path: db_path.clone(),
                message: format!("cannot read schema_version: {e}"),
            })?;

        if existing_version > SCHEMA_VERSION {
            return Err(CaduceusError::StateCorrupt {
                path: db_path,
                message: format!(
                    "SQLite store has schema v{existing_version} but this daemon only supports v{SCHEMA_VERSION} — upgrade required"
                ),
            });
        }

        if existing_version < SCHEMA_VERSION {
            // Run migration from existing_version to SCHEMA_VERSION.
            migrate_v1_to_v2(&conn, &db_path, existing_version)?;
            migrate_v2_to_v3(&conn, &db_path, existing_version)?;
            apply_schema(&conn, &db_path)?;
            record_version(&conn, &db_path)?;
        }

        // Ensure missing tables are created (idempotent).
        apply_schema(&conn, &db_path)?;
    }

    Ok(conn)
}

fn init_schema(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot start init transaction: {e}"),
        })?;

    apply_schema_in_tx(&tx, db_path)?;
    record_version_in_tx(&tx, db_path)?;

    tx.commit().map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot commit init transaction: {e}"),
    })?;

    Ok(())
}

fn apply_schema(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot apply schema: {e}"),
        })
}

fn apply_schema_in_tx(tx: &Transaction, db_path: &Path) -> CaduceusResult<()> {
    tx.execute_batch(SCHEMA_SQL)
        .map_err(|e| CaduceusError::StateCorrupt {
            path: db_path.to_path_buf(),
            message: format!("cannot apply schema in tx: {e}"),
        })
}

fn record_version(conn: &Connection, db_path: &Path) -> CaduceusResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
        params![SCHEMA_VERSION, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot record schema version: {e}"),
    })?;
    Ok(())
}

fn record_version_in_tx(tx: &Transaction, db_path: &Path) -> CaduceusResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
        params![SCHEMA_VERSION, now],
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("cannot record schema version in tx: {e}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Migration: v1 → v2
// ---------------------------------------------------------------------------

/// Migrate from schema v1 to v2 by adding the `operation_id` and
/// `remote_marker` columns to the `checkpoints` table. Both columns
/// are nullable so existing rows get NULL defaults.
fn migrate_v1_to_v2(conn: &Connection, db_path: &Path, from_version: i64) -> CaduceusResult<()> {
    if from_version >= 2 {
        return Ok(());
    }
    conn.execute_batch(
        "ALTER TABLE checkpoints ADD COLUMN operation_id TEXT;
         ALTER TABLE checkpoints ADD COLUMN remote_marker TEXT;",
    )
    .map_err(|e| CaduceusError::StateCorrupt {
        path: db_path.to_path_buf(),
        message: format!("v1→v2 migration failed: {e}"),
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Migration: v2 → v3
// ---------------------------------------------------------------------------

/// Migrate from schema v2 to v3. The `leases` table is created by
/// `apply_schema`, so this is a no-op migration that exists for
/// the migration wiring convention.
fn migrate_v2_to_v3(conn: &Connection, db_path: &Path, from_version: i64) -> CaduceusResult<()> {
    if from_version >= 3 {
        return Ok(());
    }
    // The `leases` table is created by `apply_schema` via `SCHEMA_SQL`.
    // No ALTER TABLE statements are needed for v2→v3.
    let _ = conn;
    let _ = db_path;
    Ok(())
}

// ---------------------------------------------------------------------------
// Convenience accessors
// ---------------------------------------------------------------------------

/// Open or create the database under `state_dir`.
pub fn open_in(state_dir: &Path) -> CaduceusResult<Connection> {
    let path = state_dir.join(DB_FILENAME);
    open(&path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn db_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("sqlite-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("test.db")
    }

    #[test]
    fn open_creates_fresh_database_with_current_version() {
        let path = db_path();
        let conn = open(&path).expect("open fresh db");
        conn.close().expect("close");

        // Re-open and check version.
        let conn = open(&path).expect("re-open");
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "schema version must match");
        conn.close().expect("close");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_rejects_future_schema() {
        let path = db_path();
        let conn = open(&path).expect("open fresh db");
        // Manually bump the version.
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
            params![SCHEMA_VERSION + 1, now],
        )
        .expect("insert future version");
        drop(conn);

        let result = open(&path);
        assert!(result.is_err(), "must reject future schema version");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn schema_tables_are_created() {
        let path = db_path();
        let conn = open(&path).expect("open fresh db");

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"queue_entries".to_string()));
        assert!(tables.contains(&"state_meta".to_string()));
        assert!(tables.contains(&"claims".to_string()));
        assert!(tables.contains(&"checkpoints".to_string()));
        assert!(tables.contains(&"circuit_breakers".to_string()));
        assert!(tables.contains(&"leases".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));

        conn.close().expect("close");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn transactional_rollback_preserves_prior_state() {
        let path = db_path();
        let conn = open(&path).expect("open fresh db");

        // Write a queue entry.
        conn.execute(
            "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?4)",
            params!["owner/repo#1", "queued", "code", chrono::Utc::now().to_rfc3339()],
        )
        .expect("insert");

        // Start a transaction, insert, then roll back.
        let tx = conn.unchecked_transaction().expect("tx");
        tx.execute(
            "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?4)",
            params!["owner/repo#2", "queued", "code", chrono::Utc::now().to_rfc3339()],
        )
        .expect("insert in tx");
        tx.rollback().expect("rollback");

        // Only the first entry should survive.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue_entries", [], |row| row.get(0))
            .expect("count");
        assert_eq!(count, 1, "only one entry after rollback");

        conn.close().expect("close");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_mode_is_enabled() {
        let path = db_path();
        let conn = open(&path).expect("open fresh db");

        let journal: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("read journal mode");
        assert_eq!(journal.to_lowercase(), "wal", "WAL mode must be enabled");

        conn.close().expect("close");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_in_creates_db_in_state_dir() {
        let dir = std::env::temp_dir().join(format!("sqlite-state-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let conn = open_in(&dir).expect("open in state dir");
        conn.close().expect("close");

        let db_path = dir.join(DB_FILENAME);
        assert!(db_path.is_file(), "database file must exist in state dir");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconnect_after_close_works() {
        let path = db_path();
        {
            let conn = open(&path).expect("open");
            conn.close().expect("close");
        }
        {
            let conn = open(&path).expect("re-open");
            let version: i64 = conn
                .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                    row.get(0)
                })
                .expect("read version");
            assert_eq!(version, SCHEMA_VERSION);
            conn.close().expect("close");
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn migrate_v2_to_v3_adds_leases_table() {
        // Create a v2 database by opening with SCHEMA_VERSION=2,
        // then verify that a v3 open adds the leases table.
        let path = db_path();
        {
            // Force SCHEMA_VERSION to 2 by creating the database
            // without the leases table.
            let conn = Connection::open(&path).expect("open raw");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
                 INSERT INTO schema_version (version, migrated_at) VALUES (2, '2026-01-01T00:00:00Z');",
            )
            .expect("init v2 schema");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1);
                 CREATE TABLE IF NOT EXISTS state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, worker_pid INTEGER, token TEXT NOT NULL, claimed_at TEXT NOT NULL, expires_at TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, operation_id TEXT, remote_marker TEXT, PRIMARY KEY (run_id, stage));
                 CREATE TABLE IF NOT EXISTS circuit_breakers (issue_key TEXT PRIMARY KEY, failure_count INTEGER NOT NULL DEFAULT 0, last_failure_at TEXT, opened_at TEXT);",
            )
            .expect("create v2 tables");
            conn.close().expect("close");
        }
        // Re-open with current SCHEMA_VERSION (3) — the migration
        // should add the leases table.
        let conn = open(&path).expect("open v3");

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            tables.contains(&"leases".to_string()),
            "leases table must exist after v2→v3 migration; tables: {tables:?}"
        );
        conn.close().expect("close");
        let _ = fs::remove_file(&path);
    }
}
