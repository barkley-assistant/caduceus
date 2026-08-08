//! Unit tests for SQLite corruption recovery.

use std::fs;
use std::path::PathBuf;

use caduceus::migrate::recover_sqlite_state;
use caduceus::store;

fn state_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("migrate-recover-test-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn recover_sqlite_healthy_db_returns_noop() {
    let dir = state_dir();
    // Create a healthy database.
    let conn = store::open_in(&dir).expect("open fresh db");
    drop(conn);

    let report = recover_sqlite_state(&dir, None, false).expect("recover healthy");
    assert!(
        report.archived_corrupt.is_none(),
        "healthy db must not archive"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_sqlite_corrupt_db_archives_and_creates_fresh() {
    let dir = state_dir();
    // Write corrupt bytes to the database path.
    let db_path = dir.join(store::DB_FILENAME);
    fs::write(&db_path, b"not a valid sqlite database").unwrap();

    let report = recover_sqlite_state(&dir, None, false).expect("recover corrupt");
    assert!(
        report.archived_corrupt.is_some(),
        "corrupt db must be archived"
    );
    assert!(
        report.archived_corrupt.as_ref().unwrap().is_file(),
        "archive must exist"
    );

    // Fresh database must be created and be valid.
    let conn = store::open(&db_path).expect("fresh db must be valid");
    drop(conn);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_sqlite_restores_from_backup() {
    let dir = state_dir();
    // Create a healthy database.
    let conn = store::open_in(&dir).expect("open fresh");
    drop(conn);

    // Create a backup copy.
    let db_path = dir.join(store::DB_FILENAME);
    let backup_path = dir.join("state.db.backup");
    fs::copy(&db_path, &backup_path).unwrap();

    // Corrupt the original.
    fs::write(&db_path, b"garbage").unwrap();

    // Recover from backup.
    let report =
        recover_sqlite_state(&dir, Some(&backup_path), false).expect("recover from backup");
    assert!(
        report.archived_corrupt.is_some(),
        "corrupt db must be archived"
    );

    // Restored database must be valid.
    let conn = store::open(&db_path).expect("restored db must be valid");
    drop(conn);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_sqlite_missing_backup_creates_fresh() {
    let dir = state_dir();
    let db_path = dir.join(store::DB_FILENAME);
    fs::write(&db_path, b"garbage").unwrap();

    // No backup provided — should create fresh.
    let report = recover_sqlite_state(&dir, None, false).expect("recover without backup");
    assert!(
        report.archived_corrupt.is_some(),
        "corrupt db must be archived"
    );

    let conn = store::open(&db_path).expect("fresh db must be valid");
    drop(conn);
    let _ = fs::remove_dir_all(&dir);
}
