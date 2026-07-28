//! Unit-level tests for the `migrate-state --to-sqlite` path.

use std::fs;
use std::path::PathBuf;
use std::thread;

use caduceus::migrate_to_sqlite::{migrate_to_sqlite, LockPolicy, SqliteMigrationOutcome};
use caduceus::queue::DaemonLock;

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-migrate-test-{label}-{nonce}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

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
