//! Integration tests for `repo::mirror::BareMirror`.
//!
//! Tests cover: creation, fetch, idempotency, mode 0700.

use std::path::Path;
use std::process::Command;

use caduceus::config::Config;
use caduceus::repo::BareMirror;
use caduceus::worktree::GitRunner;

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-mirror-test-{label}-{nonce}"));
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
