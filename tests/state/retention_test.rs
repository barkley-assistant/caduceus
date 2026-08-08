//! Unit tests for backup retention and state compaction.

use std::fs;
use std::path::PathBuf;

use caduceus::retention::prune_backups;

fn dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("retention-test-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn prune_removes_old_backups() {
    let d = dir();

    // Create a backup file with an old timestamp.
    let old_backup = d.join("state.json.bak-1000000");
    fs::write(&old_backup, b"old").unwrap();
    // Set its modified time to 30 days ago.
    let old_time = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
    );
    filetime::set_file_mtime(&old_backup, old_time).unwrap();

    // Create a recent backup (within retention window).
    let recent_backup = d.join("state.json.bak-9999999999");
    fs::write(&recent_backup, b"recent").unwrap();

    let count = prune_backups(&d, 7).expect("prune");
    assert_eq!(count, 1, "only old backup should be pruned");

    assert!(!old_backup.exists(), "old backup must be removed");
    assert!(recent_backup.exists(), "recent backup must be kept");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn prune_preserves_active_state() {
    let d = dir();

    // Active state files must never be pruned.
    fs::write(d.join("state.json"), b"active").unwrap();
    fs::write(d.join("state.db"), b"active").unwrap();
    fs::write(d.join("state_meta.json"), b"active").unwrap();

    let count = prune_backups(&d, 7).expect("prune");
    assert_eq!(count, 0, "no backup files to prune");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn prune_preserves_untimed_corrupt_marker() {
    let d = dir();

    // An untimed corruption marker (no timestamp) must be preserved.
    fs::write(d.join("state.json.corrupt"), b"marker").unwrap();
    // But a timed one can be pruned.
    let old = d.join("state.db.corrupt-1000000");
    fs::write(&old, b"old").unwrap();
    let old_time = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
    );
    filetime::set_file_mtime(&old, old_time).unwrap();

    let count = prune_backups(&d, 7).expect("prune");
    assert_eq!(count, 1, "only timed corrupt archive should be pruned");

    assert!(
        d.join("state.json.corrupt").exists(),
        "untimed marker must be kept"
    );

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn prune_empty_dir_returns_zero() {
    let d = dir();
    let count = prune_backups(&d, 7).expect("prune empty");
    assert_eq!(count, 0);
    let _ = fs::remove_dir_all(&d);
}
