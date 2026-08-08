//! Unit tests for the SQLite state store.

use std::fs;
use std::path::PathBuf;

use caduceus::store::{open, open_in, DB_FILENAME, SCHEMA_VERSION};
use rusqlite::{params, Connection};

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
    assert!(tables.contains(&"circuit_state".to_string()));
    assert!(tables.contains(&"leases".to_string()));
    assert!(tables.contains(&"oci_runs".to_string()));
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
        params![
            "owner/repo#1",
            "queued",
            "code",
            chrono::Utc::now().to_rfc3339()
        ],
    )
    .expect("insert");

    // Start a transaction, insert, then roll back.
    let tx = conn.unchecked_transaction().expect("tx");
    tx.execute(
        "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)",
        params![
            "owner/repo#2",
            "queued",
            "code",
            chrono::Utc::now().to_rfc3339()
        ],
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
    // Re-open with current SCHEMA_VERSION (4) — the migration
    // should add the circuit_state table and drop circuit_breakers.
    let conn = open(&path).expect("open v4");

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();

    assert!(
        tables.contains(&"circuit_state".to_string()),
        "circuit_state table must exist after v2→v4 migration; tables: {tables:?}"
    );
    assert!(
        !tables.contains(&"circuit_breakers".to_string()),
        "circuit_breakers table must be dropped after v2→v4 migration; tables: {tables:?}"
    );
    assert!(
        tables.contains(&"leases".to_string()),
        "leases table must exist after v2→v4 migration; tables: {tables:?}"
    );
    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}

#[test]
fn migrate_v3_to_v4_adds_circuit_state_and_drops_circuit_breakers() {
    // Create a v3 database with both circuit_breakers but no circuit_state,
    // then verify that a v4 open adds circuit_state and drops circuit_breakers.
    let path = db_path();
    {
        let conn = Connection::open(&path).expect("open raw");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
             INSERT INTO schema_version (version, migrated_at) VALUES (3, '2026-01-01T00:00:00Z');",
        )
        .expect("init v3 schema");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1);
             CREATE TABLE IF NOT EXISTS state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, worker_pid INTEGER, token TEXT NOT NULL, claimed_at TEXT NOT NULL, expires_at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, operation_id TEXT, remote_marker TEXT, PRIMARY KEY (run_id, stage));
             CREATE TABLE IF NOT EXISTS circuit_breakers (issue_key TEXT PRIMARY KEY, failure_count INTEGER NOT NULL DEFAULT 0, last_failure_at TEXT, opened_at TEXT);
             CREATE TABLE IF NOT EXISTS leases (issue_key TEXT PRIMARY KEY, owner_id TEXT NOT NULL, fencing_token INTEGER NOT NULL, expires_at INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired')));",
        )
        .expect("create v3 tables");
        conn.close().expect("close");
    }
    // Re-open with current SCHEMA_VERSION (4) — migration should add circuit_state
    // and drop circuit_breakers.
    let conn = open(&path).expect("open v4");

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();

    assert!(
        tables.contains(&"circuit_state".to_string()),
        "circuit_state table must exist after v3→v4 migration; tables: {tables:?}"
    );
    assert!(
        !tables.contains(&"circuit_breakers".to_string()),
        "circuit_breakers table must be dropped after v3→v4 migration; tables: {tables:?}"
    );
    conn.close().expect("close");
    let _ = fs::remove_file(&path);
}
