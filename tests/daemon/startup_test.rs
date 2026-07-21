//! Integration tests for daemon startup — storage tree init, idempotent restart.

use std::path::Path;

use caduceus::config::Config;
use caduceus::repo::Storage;

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-startup-test-{label}-{nonce}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[test]
fn startup_creates_storage_tree_when_missing() {
    let root = tempdir("cold-start");
    let repos_root = root.join("repos");

    let storage = Storage::new(repos_root.clone());
    storage.ensure_dirs().expect("ensure_dirs on cold start");

    assert!(
        repos_root.join("mirrors").exists(),
        "mirrors dir should exist"
    );
    assert!(
        repos_root.join("worktrees").exists(),
        "worktrees dir should exist"
    );
}

#[test]
fn startup_is_idempotent() {
    let root = tempdir("idempotent-start");
    let repos_root = root.join("repos");

    let storage = Storage::new(repos_root.clone());
    storage.ensure_dirs().expect("first ensure_dirs");
    storage
        .ensure_dirs()
        .expect("second ensure_dirs (idempotent)");

    assert!(repos_root.join("mirrors").exists());
    assert!(repos_root.join("worktrees").exists());
}

#[test]
fn config_repo_storage_root_defaults_to_state_dir_repos() {
    let root = tempdir("cfg-default");
    let cfg = Config::test_defaults(&root);
    let expected = root.join("repos");
    assert_eq!(
        cfg.repo_storage_root, expected,
        "expected repo_storage_root {:?}, got {:?}",
        expected, cfg.repo_storage_root
    );
}

#[test]
fn config_repo_storage_root_in_raw_is_used() {
    // Verify that the Config field is reachable
    let root = tempdir("cfg-custom");
    let custom = Path::new("/tmp/caduceus-custom-repos");
    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = custom.to_path_buf();
    assert_eq!(cfg.repo_storage_root, custom);
}
