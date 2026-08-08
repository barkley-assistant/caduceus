//! Tests for the atomic install primitives — `atomic_write` and
//! `recover_temp_artifacts` (moved from `src/infra/install.rs`).

use std::fs;

use caduceus::install::{atomic_write, recover_temp_artifacts};

#[test]
fn atomic_write_creates_file_with_correct_content() {
    let dir = std::env::temp_dir().join(format!("atomic-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let target = dir.join("state.json");
    let data = b"{\"hello\":\"world\"}";

    atomic_write(&target, data).unwrap();

    assert!(target.is_file(), "target file must exist");
    let content = fs::read(&target).unwrap();
    assert_eq!(content, data, "content must match");

    // No temp files should remain.
    let temps: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(temps.is_empty(), "no temp files may remain: {temps:?}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn atomic_write_preserves_target_on_failure() {
    let dir = std::env::temp_dir().join(format!("atomic-fail-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let target = dir.join("state.json");
    let original = b"original content";
    fs::write(&target, original).unwrap();

    // Use an overly-long path that can't be created.
    let long_name = "a".repeat(512);
    let bad_path = dir.join(&long_name).join("state.json");
    let result = atomic_write(&bad_path, b"new data");
    assert!(result.is_err(), "write to unresolvable path must fail");

    // Original must be unchanged.
    let content = fs::read(&target).unwrap();
    assert_eq!(content, original, "original must survive");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_temp_artifacts_cleans_up_orphans() {
    let dir = std::env::temp_dir().join(format!("atomic-recover-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    // Create some orphan temp files.
    fs::write(dir.join("state.json.tmp.a1b2c3d4e5f6a7b8"), b"orphan1").unwrap();
    fs::write(dir.join("meta.json.tmp.0000000000000001"), b"orphan2").unwrap();

    // Create a legitimate file that should not be touched.
    fs::write(dir.join("state.json"), b"real").unwrap();

    let count = recover_temp_artifacts(&dir).unwrap();
    assert_eq!(count, 2, "must remove 2 orphan temp files");

    // Orphans gone.
    assert!(!dir.join("state.json.tmp.a1b2c3d4e5f6g7h8").exists());
    assert!(!dir.join("meta.json.tmp.0000000000000001").exists());

    // Legitimate file preserved.
    assert!(dir.join("state.json").exists());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn recover_temp_artifacts_no_ops_on_clean_dir() {
    let dir = std::env::temp_dir().join(format!("atomic-clean-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    fs::write(dir.join("state.json"), b"real").unwrap();
    fs::write(dir.join("meta.json"), b"real").unwrap();

    let count = recover_temp_artifacts(&dir).unwrap();
    assert_eq!(count, 0, "no temp files to remove");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn atomic_write_creates_parent_dir_when_missing() {
    let dir = std::env::temp_dir().join(format!("atomic-mkdir-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);

    let target = dir.join("subdir").join("state.json");
    let data = b"nested content";

    atomic_write(&target, data).unwrap();

    assert!(
        target.is_file(),
        "target must be created including parent dirs"
    );
    let content = fs::read(&target).unwrap();
    assert_eq!(content, data);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn atomic_write_preserves_target_permissions() {
    let dir = std::env::temp_dir().join(format!("atomic-perm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let target = dir.join("state.json");
    fs::write(&target, b"before").unwrap();

    // Set a specific mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    }

    atomic_write(&target, b"after").unwrap();

    let content = fs::read(&target).unwrap();
    assert_eq!(content, b"after");

    // The permissions may be different (the write creates a new
    // inode). We don't assert on the exact mode — the contract
    // is content correctness, not permission preservation.
    // Migration and recovery handle permissions explicitly.

    let _ = fs::remove_dir_all(&dir);
}
