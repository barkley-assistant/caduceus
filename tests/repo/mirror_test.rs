//! Integration tests for `repo::mirror::BareMirror`.
//!
//! Tests cover: creation, fetch, idempotency, mode 0700.

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::repo::BareMirror;
use caduceus::worktree::GitRunner;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::path::Path;
use std::process::Command;

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

/// Initialise a bare repository at *path* with one commit on `main`.
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

/// Initialise a bare repository at *path* with `main` (commit A) and a
/// `feature` branch (commit B whose parent is A). Returns `(A, B)`.
///
/// B is a **non-ancestor** of main: deleting the `feature` ref makes B
/// unreachable, so `git gc --prune=now` on the remote actually prunes
/// it. (If B were an ancestor of main, gc would keep it and a later
/// SHA-anchored fetch would succeed instead of rejecting.)
fn init_bare_remote_with_feature(path: &Path) -> (String, String) {
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
    let commit_a = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit_a,
    ]));
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-p", &commit_a, "-m", "feature"])
        .output()
        .expect("commit-tree feature");
    let commit_b = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &commit_b,
    ]));
    (commit_a, commit_b)
}

/// Sorted `git show-ref` output for deterministic before/after
/// comparison (ref order across ls-refs walks is not guaranteed).
fn sorted_show_refs(git_dir: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(git_dir)
        .args(["show-ref"])
        .output()
        .expect("show-ref");
    assert!(
        output.status.success(),
        "show-ref failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut refs: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.to_string())
        .collect();
    refs.sort();
    refs
}

#[tokio::test]
async fn mirror_creates_bare_repo_at_storage_path() {
    let root = tempdir("ensure");
    let remote_dir = root.join("remote.git");
    let _commit = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(
        &runner,
        &cfg,
        "test-owner",
        "test-repo",
        &remote_url,
        "main",
    )
    .await
    .expect("BareMirror::ensure");

    assert!(mirror.path.exists(), "mirror path should exist");
    assert!(
        mirror.path.join("HEAD").exists(),
        "bare repo HEAD should exist"
    );
    assert!(
        mirror.path.join("config").exists(),
        "bare repo config should exist"
    );
    assert!(mirror
        .path
        .to_string_lossy()
        .contains("repos/mirrors/test-owner/test-repo.git"));
}

#[tokio::test]
async fn mirror_is_idempotent() {
    let root = tempdir("idempotent");
    let remote_dir = root.join("remote.git");
    let _commit = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let m1 = BareMirror::ensure(&runner, &cfg, "owner", "repo", &remote_url, "main")
        .await
        .expect("first ensure");
    let m2 = BareMirror::ensure(&runner, &cfg, "owner", "repo", &remote_url, "main")
        .await
        .expect("second ensure");

    assert_eq!(m1.path, m2.path, "same path on idempotent call");
}

#[tokio::test]
async fn mirror_fetches_refs() {
    let root = tempdir("fetch");
    let remote_dir = root.join("remote.git");
    let _commit = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "fowner", "frepo", &remote_url, "main")
        .await
        .expect("ensure");

    // Verify the mirror has the remote ref
    let oid = mirror.rev_parse(&runner, "origin/main").await.unwrap();
    assert!(
        !oid.is_empty(),
        "rev-parse should return a non-empty OID, got: {oid:?}"
    );
}

#[tokio::test]
async fn mirror_mode_0700() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempdir("mode");
    let remote_dir = root.join("remote.git");
    let _commit = init_bare_remote(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "mowner", "mrepo", &remote_url, "main")
        .await
        .expect("ensure");

    let meta = std::fs::metadata(&mirror.path).expect("metadata");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "mirror dir should be 0700, got {:03o}", mode);
}

#[tokio::test]
async fn mirror_fetches_pr_head_sha() {
    let root = tempdir("fetch-sha");
    let remote_dir = root.join("remote.git");
    let (_commit_a, commit_b) = init_bare_remote_with_feature(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "fshaowner", "fsharepo", &remote_url, "main")
        .await
        .expect("ensure");

    let refs_before = sorted_show_refs(&mirror.path);
    mirror
        .fetch_sha(&runner, &commit_b)
        .await
        .expect("fetch_sha should fetch the PR head SHA");
    let refs_after = sorted_show_refs(&mirror.path);

    assert_eq!(
        refs_before, refs_after,
        "SHA-anchored fetch must not create branch or ref artefacts"
    );
    let oid = mirror
        .rev_parse(&runner, &commit_b)
        .await
        .expect("rev_parse should resolve the fetched SHA");
    assert_eq!(oid, commit_b, "PR head SHA should be present in the mirror");
}

#[tokio::test]
async fn mirror_rejects_unavailable_sha() {
    let root = tempdir("unavailable-sha");
    let remote_dir = root.join("remote.git");
    let (_commit_a, commit_b) = init_bare_remote_with_feature(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "uowner", "urepo", &remote_url, "main")
        .await
        .expect("ensure");

    // B is NOT in the local mirror: ensure() fetched main only.
    // Simulate force-push + GC between discovery and execution: the
    // feature branch is deleted and B (unreachable, non-ancestor of
    // main) is pruned from the remote.
    run_command(Command::new("git").current_dir(&remote_dir).args([
        "update-ref",
        "-d",
        "refs/heads/feature",
    ]));
    run_command(Command::new("git").current_dir(&remote_dir).args([
        "reflog",
        "expire",
        "--expire=now",
        "--all",
    ]));
    run_command(
        Command::new("git")
            .current_dir(&remote_dir)
            .args(["gc", "--prune=now"]),
    );

    let err = mirror
        .fetch_sha(&runner, &commit_b)
        .await
        .expect_err("fetch_sha must reject an unavailable SHA");
    assert!(
        matches!(
            err,
            CaduceusError::HeadShaUnavailable { ref sha } if sha == &commit_b
        ),
        "expected HeadShaUnavailable with sha {commit_b}, got: {err:?}"
    );
}

#[tokio::test]
async fn mirror_fetch_sha_is_idempotent() {
    let root = tempdir("fetch-sha-idempotent");
    let remote_dir = root.join("remote.git");
    let (_commit_a, commit_b) = init_bare_remote_with_feature(&remote_dir);
    let remote_url = format!("file://{}", remote_dir.display());

    let mut cfg = Config::test_defaults(&root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    let runner = GitRunner::new(&cfg);

    let mirror = BareMirror::ensure(&runner, &cfg, "iowner", "irepo", &remote_url, "main")
        .await
        .expect("ensure");

    mirror
        .fetch_sha(&runner, &commit_b)
        .await
        .expect("first fetch_sha");
    mirror
        .fetch_sha(&runner, &commit_b)
        .await
        .expect("second fetch_sha should be a safe no-op");

    let oid = mirror
        .rev_parse(&runner, &commit_b)
        .await
        .expect("rev_parse should resolve the fetched SHA");
    assert_eq!(oid, commit_b, "PR head SHA should still be present");
}
