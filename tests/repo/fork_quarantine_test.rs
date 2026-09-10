//! Fork quarantine primitive tests (issue #337, Phase 2).
//!
//! Proves the per-PR quarantine clone end-to-end against real local
//! remotes:
//!
//! - `create` clones the TRUSTED base repo URL into
//!   `<state_dir>/fork-quarantine/<owner>/<repo>/<pr>@<head_sha>/`
//!   and writes a `QUARANTINE_MARKER` (leaf, head_repo, head_sha,
//!   base_sha, timestamp);
//! - `fetch_fork_sha` fetches the fork's head SHA SHA-anchored from
//!   the fork URL (no tracking ref, no fork remote persisted);
//! - `merge_base` computes the base/head merge base INSIDE the
//!   quarantine clone;
//! - `remove` deletes the quarantine directory and writes a forensic
//!   removal marker under `<root>/.removed/`;
//! - the quarantine clone persists `core.hooksPath=/dev/null` and
//!   carries NO `credential.helper` (the runner's `GIT_ASKPASS`
//!   broker is the only credential surface; nothing persists in the
//!   clone).

use std::path::{Path, PathBuf};
use std::process::Command;

use caduceus::config::Config;
use caduceus::repo::fork_quarantine::{
    fork_quarantine_root, quarantine_leaf, quarantine_queue_key, QUARANTINE_MARKER_FILENAME,
    REMOVED_DIRNAME,
};
use caduceus::repo::ForkQuarantine;
use caduceus::worktree::GitRunner;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

/// Create a bare repo with a root commit on `main`; returns the root
/// commit SHA.
fn init_bare_with_root(path: &Path) -> String {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(Command::new("git").arg("init").arg("--bare").arg(path));
    run(Command::new("git")
        .current_dir(path)
        .args(["symbolic-ref", "HEAD", "refs/heads/main"]));
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["commit-tree", &tree, "-m", "root"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/main", &commit]));
    commit
}

/// Add a child commit (of `parent`) on `refs/heads/<branch>` and
/// return its SHA.
fn add_commit(path: &Path, branch: &str, parent: &str, message: &str) -> String {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["commit-tree", &tree, "-p", parent, "-m", message])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let ref_name = format!("refs/heads/{branch}");
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", &ref_name, &commit]));
    commit
}

fn git_config_get(repo: &Path, key: &str) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["config", "--get", key])
        .output()
        .expect("git config --get");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A base repo (trusted origin) and a fork repo (shares the base
/// history plus one fork commit), both local bare remotes.
struct ForkRemoteFixture {
    root: PathBuf,
    base_url: String,
    fork_url: String,
    base_sha: String,
    fork_head: String,
}

fn fork_remote_fixture(label: &str) -> ForkRemoteFixture {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let root = tempdir(label);
    let base_dir = root.join("base.git");
    let base_sha = init_bare_with_root(&base_dir);
    // The fork is a real fork: a bare clone of the base (same
    // history AND same objects), plus its own commit.
    let fork_dir = root.join("fork.git");
    run(Command::new("git").args([
        "clone",
        "--bare",
        &base_dir.to_string_lossy(),
        &fork_dir.to_string_lossy(),
    ]));
    let fork_head = add_commit(&fork_dir, "fork-branch", &base_sha, "fork work");
    ForkRemoteFixture {
        root,
        base_url: format!("file://{}", base_dir.display()),
        fork_url: format!("file://{}", fork_dir.display()),
        base_sha,
        fork_head,
    }
}

fn runner_cfg(root: &Path) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    cfg
}

#[tokio::test]
async fn create_clones_base_and_writes_marker() {
    let f = fork_remote_fixture("q-create");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    let q = ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create");

    let leaf = quarantine_leaf(7, &f.fork_head);
    let expected = fork_quarantine_root(&cfg.state_dir)
        .join("owner")
        .join("repo")
        .join(&leaf);
    assert_eq!(q.path, expected, "quarantine path shape");
    assert!(q.path.join("HEAD").exists(), "bare clone materialised");

    // The clone's origin is the TRUSTED base URL — no fork remote is
    // ever persisted on the quarantine (and never on the production
    // mirror).
    assert_eq!(
        git_config_get(&q.path, "remote.origin.url"),
        f.base_url,
        "origin must be the trusted base repo, not the fork"
    );

    // Provenance marker.
    let marker_raw =
        std::fs::read_to_string(q.path.join(QUARANTINE_MARKER_FILENAME)).expect("marker exists");
    let marker: serde_json::Value = serde_json::from_str(&marker_raw).expect("marker parses");
    assert_eq!(marker["head_repo"], "attacker/fork");
    assert_eq!(marker["head_sha"], f.fork_head);
    assert_eq!(marker["base_sha"], f.base_sha);
    assert_eq!(marker["schema_version"], 1);

    // Credential + hooks posture: no credential.helper persisted,
    // hooks neutralised in the clone's own config.
    assert!(
        git_config_get(&q.path, "credential.helper").is_empty(),
        "no credential.helper may persist in the quarantine clone"
    );
    assert_eq!(
        git_config_get(&q.path, "core.hooksPath"),
        "/dev/null",
        "hooks neutralised in the clone config"
    );
}

#[tokio::test]
async fn fetch_fork_sha_and_merge_base_compute_inside_quarantine() {
    let f = fork_remote_fixture("q-fetch");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    let q = ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create");

    q.fetch_fork_sha(&runner, &f.fork_url, &f.fork_head)
        .await
        .expect("fork head fetch");

    // The fork HEAD is now present in the quarantine object store.
    let present = Command::new("git")
        .current_dir(&q.path)
        .args(["cat-file", "-e", &format!("{}^{{commit}}", f.fork_head)])
        .status()
        .expect("cat-file");
    assert!(present.success(), "fork head SHA must be present");

    // Merge base computed inside the quarantine clone: the shared
    // root commit.
    let merge_base = q
        .merge_base(&runner, &f.base_sha, &f.fork_head)
        .await
        .expect("merge-base");
    assert_eq!(merge_base, f.base_sha, "fork shares the base root");

    // No tracking ref was created by the SHA-anchored fetch: the
    // fork commit is reachable only via its SHA.
    let refs = Command::new("git")
        .current_dir(&q.path)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .expect("for-each-ref");
    let refs = String::from_utf8_lossy(&refs.stdout);
    assert!(
        !refs.contains("refs/remotes/"),
        "SHA-anchored fetch must not create tracking refs: {refs}"
    );
}

#[tokio::test]
async fn unavailable_fork_sha_surfaces_head_sha_unavailable() {
    let f = fork_remote_fixture("q-unavailable");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    let q = ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create");

    let ghost = "1111111111111111111111111111111111111111";
    let err = q
        .fetch_fork_sha(&runner, &f.fork_url, ghost)
        .await
        .expect_err("ghost SHA must not fetch");
    assert!(
        matches!(
            err,
            caduceus::error::CaduceusError::HeadShaUnavailable { .. }
        ),
        "expected HeadShaUnavailable, got: {err:?}"
    );
}

#[tokio::test]
async fn find_for_target_locates_the_same_quarantine() {
    let f = fork_remote_fixture("q-find");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create");

    let target = caduceus::review::ReviewTarget {
        repository: caduceus::review::RepositoryId {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        },
        pull_request: 7,
        head_sha: f.fork_head.clone(),
        base_sha: f.base_sha.clone(),
        base_ref: "main".to_string(),
        merge_base: f.base_sha.clone(),
    };
    let found = ForkQuarantine::find_for_target(&cfg.state_dir, &target)
        .expect("quarantine found for the target");
    assert!(found.path.join("HEAD").exists());
    assert_eq!(
        found.marker.as_ref().map(|m| m.head_repo.as_str()),
        Some("attacker/fork")
    );

    // A same-repo target (no quarantine) is None.
    let same_repo = caduceus::review::ReviewTarget {
        repository: caduceus::review::RepositoryId {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        },
        pull_request: 8,
        head_sha: f.fork_head.clone(),
        base_sha: f.base_sha.clone(),
        base_ref: "main".to_string(),
        merge_base: f.base_sha.clone(),
    };
    assert!(
        ForkQuarantine::find_for_target(&cfg.state_dir, &same_repo).is_none(),
        "no quarantine for a target that never created one"
    );
}

#[tokio::test]
async fn remove_deletes_clone_and_writes_forensic_marker() {
    let f = fork_remote_fixture("q-remove");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    let q = ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create");
    assert!(q.path.exists());

    q.remove(&runner).await.expect("remove");

    assert!(!q.path.exists(), "quarantine directory removed");
    let removed_dir = fork_quarantine_root(&cfg.state_dir).join(REMOVED_DIRNAME);
    assert!(removed_dir.is_dir(), "removal-marker dir exists");
    let markers: Vec<_> = std::fs::read_dir(&removed_dir)
        .expect("read removed dir")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(markers.len(), 1, "exactly one removal marker");
    let body = std::fs::read_to_string(markers[0].path()).expect("marker body");
    assert!(body.contains("attacker/fork"), "marker records head_repo");
    assert!(body.contains(&f.fork_head), "marker records head_sha");

    // Idempotent: removing an already-removed clone is a no-op.
    q.remove(&runner).await.expect("second remove is a no-op");
}

#[tokio::test]
async fn sweep_removes_orphans_and_keeps_active() {
    let f = fork_remote_fixture("q-sweep");
    let cfg = runner_cfg(&f.root);
    let runner = GitRunner::new(&cfg);

    // Two quarantine clones: one whose queue key is active, one
    // orphaned.
    ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        7,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/fork",
    )
    .await
    .expect("create active");
    ForkQuarantine::create(
        &runner,
        &cfg.state_dir,
        "owner",
        "repo",
        99,
        &f.fork_head,
        &f.base_sha,
        &f.base_url,
        "attacker/orphan",
    )
    .await
    .expect("create orphan");

    let active_key = quarantine_queue_key("owner", "repo", &quarantine_leaf(7, &f.fork_head));
    let removed = ForkQuarantine::sweep(&cfg.state_dir, &runner, &[active_key])
        .await
        .expect("sweep");

    assert_eq!(removed, 1, "orphan removed, active kept");
    assert!(
        fork_quarantine_root(&cfg.state_dir)
            .join("owner")
            .join("repo")
            .join(quarantine_leaf(7, &f.fork_head))
            .exists(),
        "active clone kept"
    );
    assert!(
        !fork_quarantine_root(&cfg.state_dir)
            .join("owner")
            .join("repo")
            .join(quarantine_leaf(99, &f.fork_head))
            .exists(),
        "orphan clone removed"
    );

    // Empty active set removes everything.
    let removed_all = ForkQuarantine::sweep(&cfg.state_dir, &runner, &[])
        .await
        .expect("sweep all");
    assert_eq!(removed_all, 1, "the remaining clone swept");
}

#[tokio::test]
async fn sweep_on_absent_root_is_noop() {
    let root = tempdir("q-sweep-absent");
    let cfg = runner_cfg(&root);
    let runner = GitRunner::new(&cfg);
    let removed = ForkQuarantine::sweep(&cfg.state_dir, &runner, &[])
        .await
        .expect("sweep");
    assert_eq!(removed, 0, "no quarantine root yet");
}
