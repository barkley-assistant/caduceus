//! Unit tests for backup retention and state compaction.

use std::fs;
use std::path::PathBuf;

use caduceus::retention::{prune_backups, prune_run_artifacts};

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

#[test]
fn prune_removes_meta_corrupt_archive() {
    let d = dir();

    // The meta quarantine writer (src/state/meta.rs:759) produces
    // `state_meta.json.corrupt-<ts>`; the old prefix list never
    // matched it (issue #402).
    let old = d.join("state_meta.json.corrupt-1000000");
    fs::write(&old, b"old").unwrap();
    let old_time = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
    );
    filetime::set_file_mtime(&old, old_time).unwrap();

    let count = prune_backups(&d, 7).expect("prune");
    assert_eq!(count, 1, "meta corrupt archive must be pruned");
    assert!(!old.exists(), "meta corrupt archive must be removed");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn prune_ignores_state_db_bak_prefix() {
    let d = dir();

    // `state.db.bak-<ts>` has no writer in repo history; operators'
    // manual backups follow the wiki's `.backup-*` convention, which
    // the daemon must never match. The phantom prefix is removed.
    let old = d.join("state.db.bak-1000000");
    fs::write(&old, b"old").unwrap();
    let old_time = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
    );
    filetime::set_file_mtime(&old, old_time).unwrap();

    let count = prune_backups(&d, 7).expect("prune");
    assert_eq!(count, 0, "state.db.bak- must no longer be matched");
    assert!(old.exists(), "state.db.bak- must survive the sweep");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn prune_huge_window_does_not_overflow() {
    let d = dir();

    // A huge retention window must never crash the sweep (the old
    // `retention_days * 86400` overflowed the multiply, and the
    // plain `SystemTime` subtraction panicked). With the fix, a
    // window larger than history prunes nothing.
    let fresh = d.join("state.json.bak-1000000");
    fs::write(&fresh, b"fresh").unwrap();
    let old = d.join("state.json.bak-999999");
    fs::write(&old, b"old").unwrap();
    let old_time = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86400),
    );
    filetime::set_file_mtime(&old, old_time).unwrap();

    let count = prune_backups(&d, u64::MAX).expect("prune");
    assert_eq!(count, 0, "huge window must prune nothing");
    assert!(
        fresh.exists() && old.exists(),
        "nothing removed on u64::MAX window"
    );

    let _ = fs::remove_dir_all(&d);
}

fn runs_dir(d: &std::path::Path) -> std::path::PathBuf {
    let r = d.join("runs");
    fs::create_dir_all(&r).unwrap();
    r
}

fn backdate(path: &std::path::Path, days: u64) {
    let t = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86400),
    );
    filetime::set_file_mtime(path, t).unwrap();
}

#[test]
fn run_sweep_prunes_old_artifacts_of_all_classes() {
    let d = dir();
    let runs = runs_dir(&d);

    let log = runs.join("old-run.log");
    fs::write(&log, b"old transcript").unwrap();
    backdate(&log, 31);
    let result = runs.join("old-run.result.json");
    fs::write(&result, b"{}").unwrap();
    backdate(&result, 31);
    let preview = runs.join("old-run.preview.json");
    fs::write(&preview, b"{}").unwrap();
    backdate(&preview, 31);
    let dry = runs.join("old-run.dry-run.md");
    fs::write(&dry, b"# old").unwrap();
    backdate(&dry, 31);
    let fresh_log = runs.join("new-run.log");
    fs::write(&fresh_log, b"fresh").unwrap();

    let count = prune_run_artifacts(&d, 7).expect("sweep");
    assert_eq!(count, 4, "one pruned file per artifact class");
    assert!(!log.exists() && !result.exists() && !preview.exists() && !dry.exists());
    assert!(fresh_log.exists(), "fresh transcript must survive");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn run_sweep_never_prunes_fresh_heartbeat_of_run_with_old_files() {
    let d = dir();
    let runs = runs_dir(&d);

    // The issue's safety pin: the run's OTHER files are old, but
    // the heartbeat is fresh (mtime now) — the in-flight signal
    // gc.rs:319 reads. The heartbeat must survive even though the
    // old log and result are pruned.
    let log = runs.join("live-run.log");
    fs::write(&log, b"old-looking transcript").unwrap();
    backdate(&log, 31);
    let result = runs.join("live-run.result.json");
    fs::write(&result, b"{}").unwrap();
    backdate(&result, 31);
    let heartbeat = runs.join("live-run.heartbeat");
    fs::write(&heartbeat, b"{\"version\":2}").unwrap();

    let count = prune_run_artifacts(&d, 7).expect("sweep");
    assert_eq!(count, 2, "only the old log and result go");
    assert!(heartbeat.exists(), "fresh heartbeat must NEVER be pruned");

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn run_sweep_prunes_stale_heartbeat() {
    let d = dir();
    let runs = runs_dir(&d);

    // A heartbeat older than the GC freshness cutoff is inert for
    // gc.rs (it skips stale heartbeats), so the sweep may retire it.
    let heartbeat = runs.join("dead-run.heartbeat");
    fs::write(&heartbeat, b"{\"version\":2}").unwrap();
    backdate(&heartbeat, 31);

    let count = prune_run_artifacts(&d, 7).expect("sweep");
    assert_eq!(count, 1, "stale heartbeat is pruned");
    assert!(!heartbeat.exists());

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn run_sweep_preserves_foreign_files_dirs_symlinks_and_tmps() {
    let d = dir();
    let runs = runs_dir(&d);

    let notes = runs.join("notes.txt");
    fs::write(&notes, b"operator notes").unwrap();
    backdate(&notes, 31);
    let dir_named_log = runs.join("weird.log");
    fs::create_dir_all(&dir_named_log).unwrap();
    let link = runs.join("link.log");
    std::os::unix::fs::symlink("does-not-exist", &link).unwrap();
    let tmp = runs.join(".crashed-run.log.tmp.123.999");
    fs::write(&tmp, b"partial").unwrap();
    backdate(&tmp, 31);
    let old_log = runs.join("plain-run.log");
    fs::write(&old_log, b"old").unwrap();
    backdate(&old_log, 31);

    let count = prune_run_artifacts(&d, 7).expect("sweep");
    assert_eq!(count, 1, "only the real transcript goes");
    assert!(notes.exists(), "foreign files are never touched");
    assert!(dir_named_log.is_dir(), "directories are never touched");
    assert!(
        link.symlink_metadata().is_ok(),
        "symlinks are never removed"
    );
    assert!(tmp.exists(), "atomic-write temporaries are left alone");
    assert!(!old_log.exists());

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn run_sweep_huge_window_prunes_nothing() {
    let d = dir();
    let runs = runs_dir(&d);

    let old = runs.join("ancient-run.log");
    fs::write(&old, b"old").unwrap();
    backdate(&old, 31);

    let count = prune_run_artifacts(&d, u64::MAX).expect("sweep");
    assert_eq!(count, 0, "window past the epoch prunes nothing");
    assert!(old.exists());

    let _ = fs::remove_dir_all(&d);
}

#[test]
fn run_sweep_zero_window_still_spares_fresh_files() {
    let d = dir();
    let runs = runs_dir(&d);

    // The guard pin, independent of the window: config validation
    // forbids run_retention_days == 0, but the function must be
    // safe anyway. A zero window makes cutoff == now, which would
    // prune EVERYTHING older than the call instant — only the
    // 1-hour freshness floor stands between a just-written
    // heartbeat and deletion. This test fails if the guard is
    // removed (mirrors the #177 bug class the issue calls out).
    let heartbeat = runs.join("guard-run.heartbeat");
    fs::write(&heartbeat, b"{\"version\":2}").unwrap();
    let old_log = runs.join("guard-run.log");
    fs::write(&old_log, b"old").unwrap();
    backdate(&old_log, 31);

    let count = prune_run_artifacts(&d, 0).expect("sweep");
    assert_eq!(count, 1, "only the old transcript goes");
    assert!(heartbeat.exists(), "fresh heartbeat survives a zero window");

    let _ = fs::remove_dir_all(&d);
}
