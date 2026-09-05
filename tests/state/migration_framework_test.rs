//! Migration framework tests for both state backends (issue #293,
//! DAR §15).
//!
//! Acceptance coverage:
//!
//! - AC1 — the SQLite chain runs from any older version, is idempotent,
//!   side-effect-tracked, and structural-only (no drain-by-execution).
//! - AC2 — unknown store versions hard-fail at open with operator
//!   guidance for both backends.
//! - AC3 — per-type `schema_version` plumbing rejects unknown
//!   review-typed versions with a dedicated, matchable variant.
//! - AC4 — no live version flip: `SCHEMA_VERSION` stays 7 and
//!   `QUEUE_FILE_VERSION` stays 1; the registry ships no v7→v8 entry.
//!
//! The harness builders (`harness::sqlite_store_at_version`,
//! `harness::json_state_at_version`) take a path and are the shape
//! #295's activation tests and #327's frozen-fixture suite consume:
//! build a store at version N, run the chain, assert version +
//! structure.

use caduceus::error::{store_version_guidance, CaduceusError};
use caduceus::queue::{parse_queue_state, StateStore, QUEUE_FILE_VERSION};
use caduceus::review::{parse_review_result, review_schema_version_supported};
use caduceus::store::{
    assert_registry_wellformed, open, sqlite_migration_chain, SCHEMA_VERSION, STALE_SCHEMA_VERSION,
};
use rusqlite::{params, Connection};
use std::fs;
use std::path::{Path, PathBuf};

/// Harness builders shared by the matrix below (consumed by #295/#327).
mod harness {
    use super::*;

    /// Unique temp dir per call, tagged for failure readability.
    pub fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "migration-framework-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Build a real SQLite store whose `schema_version` MAX equals
    /// `version` and whose tables match that era closely enough for the
    /// migration chain to run (each chain step must find its
    /// pre-migration shape). Returns the database path.
    pub fn sqlite_store_at_version(dir: &Path, version: i64) -> PathBuf {
        let path = dir.join("state.db");
        if version == SCHEMA_VERSION {
            // The current era is created by the daemon itself.
            let conn = open(&path).expect("open fresh current-era store");
            drop(conn);
            return path;
        }
        let conn = Connection::open(&path).expect("open raw db");
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);",
        )
        .expect("create schema_version");
        conn.execute(
            "INSERT INTO schema_version (version, migrated_at) VALUES (?1, ?2)",
            params![version, "2026-01-01T00:00:00Z"],
        )
        .expect("seed schema version");
        conn.execute_batch(&era_ddl(version))
            .expect("create era tables");
        seed_queue_entry(&conn, version);
        drop(conn);
        path
    }

    /// Write a `state.json` whose envelope carries the given version.
    /// For version ≠ 1 the body is the same and only the version field
    /// differs — sufficient to exercise the version guard.
    pub fn json_state_at_version(dir: &Path, version: u32) -> PathBuf {
        let path = dir.join("state.json");
        fs::write(&path, format!(r#"{{"version":{version},"entries":{{}}}}"#))
            .expect("write state.json");
        path
    }

    /// Assert the store's `schema_version` MAX equals `expected`.
    pub fn assert_sqlite_version(path: &Path, expected: i64) {
        let conn = Connection::open(path).expect("open raw for version check");
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("read schema_version");
        assert_eq!(version, expected, "schema version at {}", path.display());
        drop(conn);
    }

    /// Per-era DDL for versions 1-6. Hand-written per era so each chain
    /// step finds its pre-migration shape; `apply_schema` supplies the
    /// post-migration tables idempotently.
    fn era_ddl(version: i64) -> String {
        let mut sql = String::new();
        // `blocked_source` / `blocked_recovery_hint` land at v6; the
        // pre-v6 shape must NOT carry them or the v5→v6 ALTER would
        // fail with a duplicate-column error.
        sql.push_str(if version >= 6 {
            "CREATE TABLE queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, blocked_source TEXT, blocked_recovery_hint TEXT);"
        } else {
            "CREATE TABLE queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1);"
        });
        sql.push_str("CREATE TABLE state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);");
        sql.push_str("CREATE TABLE claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, worker_pid INTEGER, token TEXT NOT NULL, claimed_at TEXT NOT NULL, expires_at TEXT NOT NULL);");
        // `operation_id` / `remote_marker` land at v2; the v1 shape must
        // NOT carry them or the v1→v2 ALTER would fail.
        sql.push_str(if version >= 2 {
            "CREATE TABLE checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, operation_id TEXT, remote_marker TEXT, PRIMARY KEY (run_id, stage));"
        } else {
            "CREATE TABLE checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, PRIMARY KEY (run_id, stage));"
        });
        // The dead `circuit_breakers` table existed from v1 until
        // v3→v4 dropped it; `circuit_state` replaces it at v4.
        if version <= 3 {
            sql.push_str("CREATE TABLE circuit_breakers (issue_key TEXT PRIMARY KEY, failure_count INTEGER NOT NULL DEFAULT 0, last_failure_at TEXT, opened_at TEXT);");
        } else {
            sql.push_str("CREATE TABLE circuit_state (scope TEXT NOT NULL, scope_id TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'closed', consecutive_failures INTEGER NOT NULL DEFAULT 0, last_failure_at INTEGER, opened_at INTEGER, last_probe_at INTEGER, PRIMARY KEY (scope, scope_id));");
        }
        // `leases` lands at v3; `oci_runs` at v5.
        if version >= 3 {
            sql.push_str("CREATE TABLE leases (issue_key TEXT PRIMARY KEY, owner_id TEXT NOT NULL, fencing_token INTEGER NOT NULL, expires_at INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired')));");
        }
        if version >= 5 {
            sql.push_str("CREATE TABLE oci_runs (run_id TEXT PRIMARY KEY, container_id TEXT, state TEXT NOT NULL, engine TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, daemon_id TEXT NOT NULL, issue_id TEXT NOT NULL, worker_command_sha256 TEXT NOT NULL);");
        }
        sql
    }

    /// Seed one queue row so the matrix can assert data survives
    /// structural steps.
    fn seed_queue_entry(conn: &Connection, version: i64) {
        if version >= 6 {
            conn.execute(
                "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at, generation, blocked_source, blocked_recovery_hint)
                 VALUES (?1, 'queued', 'code', 0, ?2, ?2, 1, NULL, NULL)",
                params!["owner/repo#1", "2026-01-01T00:00:00Z"],
            )
            .expect("seed v6+ queue row");
        } else {
            conn.execute(
                "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at, generation)
                 VALUES (?1, 'queued', 'code', 0, ?2, ?2, 1)",
                params!["owner/repo#1", "2026-01-01T00:00:00Z"],
            )
            .expect("seed pre-v6 queue row");
        }
    }
}

/// Assert the store carries every current table and no dead
/// `circuit_breakers` table.
fn assert_current_tables(path: &Path) {
    let conn = Connection::open(path).expect("open raw for table check");
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .filter_map(|r| r.ok())
        .collect();
    for table in [
        "schema_version",
        "queue_entries",
        "state_meta",
        "claims",
        "checkpoints",
        "circuit_state",
        "leases",
        "oci_runs",
    ] {
        assert!(
            tables.contains(&table.to_string()),
            "missing table {table} after migration; tables: {tables:?}"
        );
    }
    assert!(
        !tables.contains(&"circuit_breakers".to_string()),
        "dead circuit_breakers must be dropped; tables: {tables:?}"
    );
    drop(conn);
}

/// Assert the seeded row survives structural migration steps.
fn assert_seeded_row_preserved(path: &Path) {
    let conn = Connection::open(path).expect("open raw");
    let (phase, source, hint): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT phase, blocked_source, blocked_recovery_hint
             FROM queue_entries WHERE issue_key = 'owner/repo#1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("seeded row readable after migration");
    assert_eq!(phase, "queued", "phase preserved");
    assert!(source.is_none(), "pre-v6 row gets NULL blocked_source");
    assert!(hint.is_none(), "pre-v6 row gets NULL blocked_recovery_hint");
    drop(conn);
}

/// Row count of `schema_version` (append-only history; MAX is truth).
fn schema_version_row_count(path: &Path) -> i64 {
    let conn = Connection::open(path).expect("open raw");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .expect("count schema_version rows");
    drop(conn);
    count
}

// -----------------------------------------------------------------------
// Matrix area 1 — chain-from-anywhere (SQLite)
// -----------------------------------------------------------------------

#[test]
fn chain_runs_from_each_older_era_to_current() {
    for version in [1i64, 2, 3, 4, 5] {
        let dir = harness::temp_dir(&format!("chain-v{version}"));
        let path = harness::sqlite_store_at_version(&dir, version);
        let conn = open(&path).unwrap_or_else(|e| panic!("open v{version} store: {e}"));
        drop(conn);
        harness::assert_sqlite_version(&path, SCHEMA_VERSION);
        assert_current_tables(&path);
        assert_seeded_row_preserved(&path);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn current_era_store_opens_without_chain() {
    let dir = harness::temp_dir("current-era");
    let path = harness::sqlite_store_at_version(&dir, SCHEMA_VERSION);
    let conn = open(&path).expect("open v7 store");
    drop(conn);
    harness::assert_sqlite_version(&path, SCHEMA_VERSION);
    assert_current_tables(&path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stale_v6_is_rejected_not_migrated() {
    let dir = harness::temp_dir("stale-v6");
    let path = harness::sqlite_store_at_version(&dir, STALE_SCHEMA_VERSION);
    let err = open(&path).expect_err("v6 must be rejected by policy");
    let text = err.to_string();
    // The existing substring pin (sqlite_store_test.rs) and the
    // structured operator-instruction block added by D3/D5.
    assert!(text.contains("stale schema v6"), "got: {text}");
    assert!(
        text.contains("https://github.com/barkley-assistant/caduceus/wiki/State-Recovery"),
        "got: {text}"
    );
    harness::assert_sqlite_version(&path, STALE_SCHEMA_VERSION);
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------------
// Matrix area 2 — idempotency (SQLite)
// -----------------------------------------------------------------------

#[test]
fn second_open_performs_no_migration_steps() {
    let dir = harness::temp_dir("idempotent");
    let path = harness::sqlite_store_at_version(&dir, 1);
    open(&path).expect("first open migrates to current");
    harness::assert_sqlite_version(&path, SCHEMA_VERSION);
    let rows_after_first = schema_version_row_count(&path);
    assert!(
        rows_after_first > 1,
        "chain steps recorded: {rows_after_first}"
    );

    open(&path).expect("second open is a no-op");
    harness::assert_sqlite_version(&path, SCHEMA_VERSION);
    let rows_after_second = schema_version_row_count(&path);
    assert_eq!(
        rows_after_first, rows_after_second,
        "second open must not record additional version rows"
    );
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------------
// Matrix area 3 — unknown-version rejection (SQLite)
// -----------------------------------------------------------------------

#[test]
fn unknown_sqlite_version_is_rejected_with_guidance() {
    let dir = harness::temp_dir("future-sqlite");
    let path = harness::sqlite_store_at_version(&dir, SCHEMA_VERSION);
    // Bump the store to a version this daemon does not know.
    let conn = Connection::open(&path).expect("open raw");
    conn.execute(
        "INSERT INTO schema_version (version, migrated_at) VALUES (99, '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("insert future version");
    drop(conn);

    let err = open(&path).expect_err("v99 must be rejected");
    match err {
        CaduceusError::StoreVersionUnsupported {
            backend,
            found,
            supported,
            ref guidance,
            ..
        } => {
            assert_eq!(backend, "sqlite", "got: {err:?}");
            assert_eq!(found, 99, "got: {err:?}");
            assert_eq!(supported, SCHEMA_VERSION, "got: {err:?}");
            assert!(guidance.contains("NEWER"), "got: {guidance}");
            assert!(guidance.contains("upgrade the daemon"), "got: {guidance}");
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

// Area 3b (below-floor version 0) does not exist by design: the floor
// is v1 — a fresh store is always initialised at SCHEMA_VERSION and a
// v0 file that already carries a schema_version table is not a
// constructible state, so there is no codepath to exercise.

// -----------------------------------------------------------------------
// Matrix areas 4/5 — unknown-version rejection (JSON)
// -----------------------------------------------------------------------

#[test]
fn json_future_version_is_rejected_with_guidance() {
    let dir = harness::temp_dir("json-future");
    let path = harness::json_state_at_version(&dir, 2);
    let text = fs::read_to_string(&path).expect("read state.json");
    let err = parse_queue_state(&text).expect_err("v2 rejected");
    match err {
        CaduceusError::StoreVersionUnsupported {
            backend,
            found,
            supported,
            ref guidance,
            ..
        } => {
            assert_eq!(backend, "json", "got: {err:?}");
            assert_eq!(found, 2, "got: {err:?}");
            assert_eq!(supported, QUEUE_FILE_VERSION as i64, "got: {err:?}");
            assert!(guidance.contains("NEWER"), "got: {guidance}");
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_older_version_is_rejected_with_guidance() {
    let dir = harness::temp_dir("json-older");
    let path = harness::json_state_at_version(&dir, 0);
    let text = fs::read_to_string(&path).expect("read state.json");
    let err = parse_queue_state(&text).expect_err("v0 rejected");
    match err {
        CaduceusError::StoreVersionUnsupported {
            backend,
            found,
            supported,
            ref guidance,
            ..
        } => {
            assert_eq!(backend, "json", "got: {err:?}");
            assert_eq!(found, 0, "got: {err:?}");
            assert_eq!(supported, QUEUE_FILE_VERSION as i64, "got: {err:?}");
            assert!(guidance.contains("OLDER"), "got: {guidance}");
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_store_open_hard_fails_on_future_envelope() {
    // The real disk-load path hard-fails the whole store open (startup
    // hard-fail), not best-effort parse.
    let dir = harness::temp_dir("json-v2-file");
    let path = harness::json_state_at_version(&dir, 2);
    let err = StateStore::open(&dir).expect_err("open of a v2 store must hard-fail");
    match err {
        CaduceusError::StoreVersionUnsupported { backend, .. } => {
            assert_eq!(backend, "json", "got: {err:?}");
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = path;
}

#[test]
fn json_v1_parses_and_missing_file_yields_empty_v1() {
    let parsed = parse_queue_state(r#"{"version":1,"entries":{}}"#).expect("v1 parses");
    assert_eq!(parsed.version, QUEUE_FILE_VERSION);
    assert!(parsed.entries.is_empty());

    // Missing state.json → empty v1 envelope (existing behaviour).
    let dir = harness::temp_dir("json-missing");
    let store = StateStore::open(&dir).expect("open store");
    let snap = store.snapshot().expect("snapshot empty dir");
    assert_eq!(snap.version, QUEUE_FILE_VERSION);
    assert!(snap.entries.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------------
// Matrix area 6 — per-type schema_version plumbing (review)
// -----------------------------------------------------------------------

#[test]
fn review_schema_version_plumbing_rejects_unknown_versions() {
    assert!(
        review_schema_version_supported(1),
        "v1 is the supported version"
    );
    for bad in [0u32, 2, u32::MAX] {
        assert!(
            !review_schema_version_supported(bad),
            "v{bad} must be unsupported"
        );
    }

    // The parse seam classifies wrong versions as a dedicated, matchable
    // variant carrying found/supported; the message keeps the
    // "schema_version" substring pinned by review_domain_test.
    let doc = r#"{"schema_version":2,"status":"success","review":null}"#;
    let err = parse_review_result(doc).expect_err("v2 doc rejected");
    match err {
        CaduceusError::ReviewSchemaVersion { found, supported } => {
            assert_eq!(found, 2, "got: {err:?}");
            assert_eq!(supported, 1, "got: {err:?}");
            assert!(err.to_string().contains("schema_version"), "got: {err}");
        }
        other => panic!("expected ReviewSchemaVersion; got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Matrix area 7 — registry well-formedness + AC4 no-flip pin
// -----------------------------------------------------------------------

#[test]
fn registry_is_wellformed_and_ceiling_is_one_below_current() {
    assert_registry_wellformed().expect("registry invariants hold");

    let chain = sqlite_migration_chain();
    assert!(!chain.is_empty(), "registry must not be empty");
    let (first_from, _, _) = chain[0];
    assert_eq!(first_from, 1, "chain starts at v1");

    for i in 1..chain.len() {
        let (from, _, _) = chain[i];
        let (prev_from, prev_to, _) = chain[i - 1];
        assert_eq!(from, prev_to, "chain must be contiguous at step {i}");
        assert!(
            from > prev_from,
            "chain must be strictly increasing at step {i}"
        );
    }
    for (from, _, _) in &chain {
        assert_ne!(
            *from, STALE_SCHEMA_VERSION,
            "the stale v6 boundary is never migratable"
        );
    }
    let (_, last_to, _) = chain.last().expect("non-empty chain");
    assert_eq!(
        *last_to,
        SCHEMA_VERSION - 1,
        "no v7→v8 entry ships in this change (AC4's structural assertion)"
    );
    assert!(*last_to < SCHEMA_VERSION);
}

#[test]
fn no_live_version_flip() {
    // AC4: the store still reports v7 and the pre-review JSON envelope
    // version; #295 bumps both atomically with the review structures.
    assert_eq!(SCHEMA_VERSION, 7);
    assert_eq!(QUEUE_FILE_VERSION, 1);
}

// -----------------------------------------------------------------------
// Matrix area 8 — guidance wording source
// -----------------------------------------------------------------------

#[test]
fn guidance_wording_carries_wiki_and_direction() {
    const WIKI: &str = "https://github.com/barkley-assistant/caduceus/wiki/State-Recovery";
    let newer = store_version_guidance(true);
    assert!(newer.contains(WIKI), "got: {newer}");
    assert!(newer.contains("NEWER"), "got: {newer}");
    let older = store_version_guidance(false);
    assert!(older.contains(WIKI), "got: {older}");
    assert!(older.contains("OLDER"), "got: {older}");
}
