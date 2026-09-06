//! Review-era activation tests (issue #295, AC2): the envelope bump
//! is atomic with the review structures, pre-#295 binaries are
//! structurally locked out, and v8 == exactly the schema this change
//! implements.
//!
//! The rejection contract cannot ship an actual pre-#295 binary in a
//! test, so it is pinned structurally: the same code path a pre-#295
//! binary takes on #295-era state (newer-version gate) is exercised
//! with a further-future version (JSON v3, SQLite v99). A pre-#295
//! binary has `QUEUE_FILE_VERSION = 1` / `SCHEMA_VERSION = 7`, so
//! #295-era state (2 / 8) hits exactly those branches.

use caduceus::queue::{parse_queue_state, StateStore, QUEUE_FILE_VERSION};
use caduceus::store::{open, sqlite_migration_chain, SCHEMA_VERSION};
use rusqlite::Connection;
use std::fs;
use std::path::PathBuf;

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "review-activation-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The real pre-#295 v7-era DDL (v6 tables + `oci_runs`, no review
/// tables) — the era shape `migration_framework_test`'s harness also
/// encodes.
const V7_ERA_DDL: &str = "
CREATE TABLE queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, blocked_source TEXT, blocked_recovery_hint TEXT);
CREATE TABLE state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, worker_pid INTEGER, token TEXT NOT NULL, claimed_at TEXT NOT NULL, expires_at TEXT NOT NULL);
CREATE TABLE checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, operation_id TEXT, remote_marker TEXT, PRIMARY KEY (run_id, stage));
CREATE TABLE circuit_state (scope TEXT NOT NULL, scope_id TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'closed', consecutive_failures INTEGER NOT NULL DEFAULT 0, last_failure_at INTEGER, opened_at INTEGER, last_probe_at INTEGER, PRIMARY KEY (scope, scope_id));
CREATE TABLE leases (issue_key TEXT PRIMARY KEY, owner_id TEXT NOT NULL, fencing_token INTEGER NOT NULL, expires_at INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired')));
CREATE TABLE oci_runs (run_id TEXT PRIMARY KEY, container_id TEXT, state TEXT NOT NULL, engine TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, daemon_id TEXT NOT NULL, issue_id TEXT NOT NULL, worker_command_sha256 TEXT NOT NULL);
";

#[test]
fn pre_295_json_state_parses_as_v2_in_place() {
    let dir = temp_dir("json-v1");
    let state_path = dir.join("state.json");
    fs::write(
        &state_path,
        r#"{"version":1,"entries":{"owner/repo#1":{"key":{"owner":"owner","repo":"repo","number":1},"phase":"queued","ticket_type":"code","attempts":0,"last_error":null,"last_run_id":null,"next_attempt_at":null,"finalization":null,"queued_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","generation":1}}}"#,
    )
    .unwrap();

    // The parse seam upgrades in place.
    let parsed = parse_queue_state(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(parsed.version, QUEUE_FILE_VERSION);
    assert_eq!(parsed.entries.len(), 1);

    // The real store-open path also accepts the pre-#295 file.
    let store = StateStore::open(&dir).expect("open store on v1 file");
    let snap = store.snapshot().unwrap();
    assert_eq!(snap.version, QUEUE_FILE_VERSION);
    assert!(snap.entries.contains_key("owner/repo#1"));

    // Parse is pure: the file on disk is NOT rewritten by the load.
    let on_disk = fs::read_to_string(&state_path).unwrap();
    assert!(
        on_disk.contains(r#""version":1"#),
        "parse must not rewrite the file; got: {on_disk}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pre_295_sqlite_v7_opens_and_migrates_to_v8() {
    let dir = temp_dir("sqlite-v7");
    let db_path = dir.join("state.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
             INSERT INTO schema_version (version, migrated_at) VALUES (7, '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        conn.execute_batch(V7_ERA_DDL).unwrap();
        conn.execute(
            "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at, generation)
             VALUES ('owner/repo#1', 'queued', 'code', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        drop(conn);
    }

    // A plain `open` runs the chain: v7 → v8 → v9 with the review tables.
    let conn = open(&db_path).expect("open v7 store");
    drop(conn);

    let conn = Connection::open(&db_path).unwrap();
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    for table in ["review_queue_entries", "review_state", "review_history"] {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "review table {table} exists after migration");
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM queue_entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "issue data preserved through migration");
    drop(conn);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pre_295_binary_rejection_is_structural() {
    // JSON: the newer-version branch keys on state.version >
    // QUEUE_FILE_VERSION. A pre-#295 binary (QUEUE_FILE_VERSION = 1)
    // hits exactly this branch on a #295-era v2 state.json; we pin the
    // branch with version 3.
    let bad = r#"{"version":3,"entries":{}}"#;
    let err = parse_queue_state(bad).expect_err("v3 rejected");
    match err {
        caduceus::error::CaduceusError::StoreVersionUnsupported {
            backend,
            found,
            supported,
            ref guidance,
            ..
        } => {
            assert_eq!(backend, "json");
            assert_eq!(found, 3);
            assert_eq!(supported, 2);
            assert!(guidance.contains("NEWER"));
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }

    // SQLite: same shape — a version-99 db hits the same gate a
    // pre-#295 binary (SCHEMA_VERSION = 7) would hit on a current db.
    let dir = temp_dir("sqlite-future");
    let db_path = dir.join("state.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
             INSERT INTO schema_version (version, migrated_at) VALUES (99, '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        drop(conn);
    }
    let err = open(&db_path).expect_err("v99 rejected");
    match err {
        caduceus::error::CaduceusError::StoreVersionUnsupported {
            backend,
            found,
            supported,
            ..
        } => {
            assert_eq!(backend, "sqlite");
            assert_eq!(found, 99);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn v8_is_exactly_the_review_schema() {
    // "v8 == implemented schema": a fresh current store contains
    // exactly the pre-#295 table set PLUS the three review tables (and
    // no other additions). #306 (v9) only adds COLUMNS to
    // review_queue_entries, so the table set is unchanged.
    let dir = temp_dir("v8-exact");
    let db_path = dir.join("state.db");
    let conn = open(&db_path).expect("open fresh review-schema store");
    drop(conn);

    let conn = Connection::open(&db_path).unwrap();
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    tables.sort();
    drop(conn);

    let mut expected: Vec<String> = [
        // Pre-change (v7) tables, verbatim.
        "schema_version",
        "queue_entries",
        "state_meta",
        "claims",
        "checkpoints",
        "circuit_state",
        "leases",
        "oci_runs",
        // The v8 additions (#295).
        "review_queue_entries",
        "review_state",
        "review_history",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    expected.sort();
    assert_eq!(
        tables, expected,
        "the review schema adds exactly the three review tables"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn registry_last_step_reaches_current_and_review_activation_exists() {
    let chain = sqlite_migration_chain();
    // The v7→v8 review activation step stays in the registry.
    assert!(
        chain
            .iter()
            .any(|&(from, to, label)| from == 7 && to == 8 && label == "review-era structures"),
        "the v7→v8 review-era step (#295) must exist in the registry"
    );
    // The chain's last step reaches SCHEMA_VERSION (#306: 8→9).
    let (last_from, last_to, last_label) = *chain.last().unwrap();
    assert_eq!(last_from, 8);
    assert_eq!(last_to, 9);
    assert_eq!(last_label, "review_queue_entries blocked columns");
    assert_eq!(last_to, SCHEMA_VERSION);
}
