//! Integration tests for `repo::Storage` — symlink rejection, mode validation.

use caduceus::repo::Storage;

#[test]
fn symlink_storage_root_rejected() {
    let tmp = std::env::temp_dir().join(format!(
        "caduceus-storage-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let real_dir = tmp.join("real");
    std::fs::create_dir_all(&real_dir).unwrap();
    let link = tmp.join("linked");
    std::os::unix::fs::symlink(&real_dir, &link).unwrap();

    let storage = Storage::new(link);
    let err = storage
        .validate_root()
        .expect_err("symlink should be rejected");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("symlink"),
        "error should mention symlink, got: {rendered}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn valid_directory_accepted() {
    let tmp = std::env::temp_dir().join(format!(
        "caduceus-storage-accept-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let storage = Storage::new(tmp.clone());
    // Should not error — the root is a real directory
    storage
        .validate_root()
        .expect("real directory should pass validation");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn ensure_dirs_creates_subdirectories() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = std::env::temp_dir().join(format!(
        "caduceus-ensure-dirs-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);

    let storage = Storage::new(tmp.clone());
    storage.ensure_dirs().expect("ensure_dirs should succeed");

    assert!(tmp.join("mirrors").exists(), "mirrors dir should exist");
    assert!(tmp.join("worktrees").exists(), "worktrees dir should exist");

    let mirrors_mode = std::fs::metadata(tmp.join("mirrors"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mirrors_mode, 0o700,
        "mirrors dir should be 0700, got {:03o}",
        mirrors_mode
    );

    let worktrees_mode = std::fs::metadata(tmp.join("worktrees"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        worktrees_mode, 0o700,
        "worktrees dir should be 0700, got {:03o}",
        worktrees_mode
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn ensure_dirs_is_idempotent() {
    let tmp = std::env::temp_dir().join(format!(
        "caduceus-idempotent-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&tmp);

    let storage = Storage::new(tmp.clone());
    storage.ensure_dirs().expect("first ensure_dirs");
    storage
        .ensure_dirs()
        .expect("second ensure_dirs (idempotent)");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn storage_new_sets_correct_subdirs() {
    let storage = Storage::new(std::path::PathBuf::from("/tmp/caduceus-repos"));
    assert_eq!(
        storage.mirrors_dir,
        std::path::PathBuf::from("/tmp/caduceus-repos/mirrors")
    );
    assert_eq!(
        storage.worktrees_dir,
        std::path::PathBuf::from("/tmp/caduceus-repos/worktrees")
    );
}
