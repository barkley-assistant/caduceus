//! Unit-level tests for the `migrate-state --to-sqlite` path.

use caduceus::migrate_to_sqlite::{migrate_to_sqlite, LockPolicy, SqliteMigrationOutcome};
use caduceus::queue::DaemonLock;
use caduceus::queue::STATE_FILENAME;
use caduceus::store;
use rusqlite::params;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::fs;
use std::path::Path;
use std::thread;

#[test]
fn migrate_with_acquire_rejects_concurrent_daemon_lock() {
    let dir = tempdir("lock-race");
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.json"),
        r#"{"version":1,"entries":{"owner/repo#1":{"phase":"queued","ticket_type":"code","attempts":0,"queued_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}}"#,
    )
    .unwrap();

    // Hold the daemon lock from the test thread.
    let _lock = DaemonLock::try_acquire(&state_dir)
        .expect("acquire")
        .expect("no one else holds lock");

    // Migration in another thread must be rejected at the lock.
    let state_dir_for_thread = state_dir.clone();
    let handle = thread::spawn(move || {
        migrate_to_sqlite(&state_dir_for_thread, false, LockPolicy::Acquire, None)
    });

    // The migration should return quickly with a lock-contention error.
    let result = handle
        .join()
        .expect("thread did not panic")
        .expect_err("migration must fail when lock is held");
    let msg = format!("{result:?}");
    assert!(msg.contains("another tick holds daemon.lock"), "got: {msg}");
}

#[test]
fn migrate_skip_does_not_need_lock_and_succeeds() {
    let dir = tempdir("lock-skip");
    let state_dir = dir.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.json"),
        r#"{"version":1,"entries":{"owner/repo#1":{"phase":"queued","ticket_type":"code","attempts":0,"queued_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}}"#,
    )
    .unwrap();

    let report =
        migrate_to_sqlite(&state_dir, false, LockPolicy::Skip, None).expect("migrate with skip");
    assert!(
        matches!(
            report.outcome,
            SqliteMigrationOutcome::Migrated { entries: 1 }
        ),
        "expected migrated one entry, got {report:?}"
    );
}

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
    fs::write(dir.join(STATE_FILENAME), body).expect("write state.json");
}

#[test]
fn migrate_empty_state_is_already_current() {
    let dir = state_dir();
    let report =
        migrate_to_sqlite(&dir, false, LockPolicy::Skip, None).expect("migrate empty state");
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

    let report = migrate_to_sqlite(&dir, true, LockPolicy::Skip, None).expect("dry run");
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

    let report = migrate_to_sqlite(&dir, false, LockPolicy::Skip, None).expect("migrate");
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

    migrate_to_sqlite(&dir, false, LockPolicy::Skip, None).expect("migrate");
    assert!(
        dir.join(STATE_FILENAME).is_file(),
        "JSON state must be preserved as backup"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn migrate_when_already_current_is_noop() {
    let dir = state_dir();
    let report = migrate_to_sqlite(&dir, false, LockPolicy::Skip, None).expect("migrate empty");
    assert_eq!(report.outcome, SqliteMigrationOutcome::AlreadyCurrent);
    let _ = fs::remove_dir_all(&dir);
}
