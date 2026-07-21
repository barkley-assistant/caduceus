//! Integration tests for `repo::Worktree` (disposable worktrees).
//!
//! Tests cover: lifecycle, failure reuse refused, cleanup.

use std::path::Path;
use std::process::Command;

use caduceus::config::Config;
use caduceus::repo::BareMirror;
use caduceus::worktree::GitRunner;
use caduceus::RepoWorktree;

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-worktree-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn run_command(cmd: &mut Command) {
    let output = cmd.output().expect("spawn command");
    if !output.status.success() {
        panic!(
            "command {:?} failed: status={:?}\nstdout={}\nstderr={}",
            cmd,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn init_bare_remote(path: &Path) -> String {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    let output = Command::new("git")
        .current_dir(path)
        .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
        .output()
        .expect("hash-object");
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-m", "initial"])
        .output()
        .expect("commit-tree");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit,
    ]));
    commit
}

#[tokio::test]
async fn worktree_create_and_remove() {
    let root = tempdir("lifecycle");
    let remote_dir = root.join("remote.git");
    let base_oid = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "wowner", "wrepo", &remote_url, "main")
        .await
        .expect("ensure mirror");

    let run_id = "test-run-001";
    let wt = RepoWorktree::create(&runner, &mirror, run_id, &base_oid)
        .await
        .expect("create worktree");

    assert!(wt.path.exists(), "worktree path should exist");
    assert!(wt.path.to_string_lossy().contains("worktrees/test-run-001"));
    assert_eq!(wt.run_id, "test-run-001");
    assert_eq!(wt.base_oid, base_oid);

    // Verify the worktree has the correct commit checked out
    let output = Command::new("git")
        .current_dir(&wt.path)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let head_oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(head_oid, base_oid, "worktree HEAD should match base OID");

    // Remove
    RepoWorktree::remove(&runner, &wt)
        .await
        .expect("remove worktree");
    assert!(!wt.path.exists(), "worktree path should be removed");
}

#[tokio::test]
async fn worktree_reuse_refused() {
    let root = tempdir("reuse");
    let remote_dir = root.join("remote.git");
    let base_oid = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "rowner", "rrepo", &remote_url, "main")
        .await
        .expect("ensure mirror");

    let run_id = "reuse-test-run";
    let _wt = RepoWorktree::create(&runner, &mirror, run_id, &base_oid)
        .await
        .expect("first create");

    let err = RepoWorktree::create(&runner, &mirror, run_id, &base_oid)
        .await
        .expect_err("reuse should fail");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("reuse of failed worktree"),
        "error should mention reuse, got: {rendered}"
    );
}

#[tokio::test]
async fn worktree_cleanup_removes_directory() {
    let root = tempdir("cleanup");
    let remote_dir = root.join("remote.git");
    let base_oid = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "cowner", "crepo", &remote_url, "main")
        .await
        .expect("ensure mirror");

    let run_id = "cleanup-test-run";
    let wt = RepoWorktree::create(&runner, &mirror, run_id, &base_oid)
        .await
        .expect("create worktree");

    // Remove should clean up both the git worktree registration and the directory
    RepoWorktree::remove(&runner, &wt)
        .await
        .expect("remove worktree");
    assert!(
        !wt.path.exists(),
        "worktree directory should be removed after cleanup"
    );

    // Verify git worktree list no longer references it
    let output = Command::new("git")
        .current_dir(&mirror.path)
        .args(["worktree", "list"])
        .output()
        .expect("worktree list");
    let list_output = String::from_utf8_lossy(&output.stdout);
    assert!(
        !list_output.contains(&wt.path.to_string_lossy().to_string()),
        "worktree should not appear in git worktree list: {list_output}"
    );
}
