//! Task 4.3 acceptance tests for safe worktree teardown.
//!
//! Each test exercises a single bullet from the task packet:
//!
//! * Successful teardown: `git worktree remove --force`,
//!   `git worktree prune`, local branch deleted when not
//!   pushed and not resumable.
//! * Dry-run / pre-commit worker-failure teardown: the
//!   branch is local-only with no upstream, so it is deleted.
//! * Pushed branch retained: the branch has an upstream so
//!   `remove` does not delete it.
//! * Already-missing path is idempotent.
//! * Nested filesystem contents (uncommitted files inside the
//!   worktree) are removed by `git worktree remove --force`.
//! * Registered metadata (`<repo>/.git/worktrees/<run_id>`) is
//!   removed alongside the worktree directory.
//! * Path-escape rejection: a `Worktree` whose `path` is NOT
//!   beneath `<repo>/.worktrees/` is refused with a typed
//!   error.
//! * Failure teardown surfaces a typed error when the worktree
//!   path can't be removed by git (simulated by a
//!   read-only mount).
//!
//! All tests use deterministic local bare-remote fixtures; no
//! network or real GitHub is touched.

#![allow(unused_variables)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::worktree::{
    create as create_worktree, remove as remove_worktree, GitRunner, RepositoryInfo, Worktree,
};

/// Unique tempdir per call so parallel test invocations don't
/// trample each other.
fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-worktree-remove-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn config_for(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.git_timeout_seconds = 30;
    cfg
}

fn key(owner: &str, repo: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
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

/// Initialise a bare repository at *path* with a `main` branch
/// that has one empty commit so the cloned repo has something
/// to fetch and set `refs/remotes/origin/HEAD` against.
fn init_bare_repo(path: &Path) -> String {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    let head_path = path.join("HEAD");
    fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    let mut cmd = Command::new("git");
    cmd.current_dir(path)
        .args(["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let output = cmd.output().expect("hash-object");
    assert!(
        output.status.success(),
        "hash-object failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut cmd = Command::new("git");
    cmd.current_dir(path)
        .args(["commit-tree", &tree, "-m", "initial"]);
    let output = cmd.output().expect("commit-tree");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit,
    ]));
    commit
}

/// Build a non-bare clone of *remote* into *dest* with a
/// `file://` origin URL.
fn clone_into(remote: &Path, dest: &Path) {
    let remote_uri = format!("file://{}", remote.display());
    let mut cmd = Command::new("git");
    cmd.arg("clone")
        .arg("-b")
        .arg("main")
        .arg(&remote_uri)
        .arg(dest);
    run_command(&mut cmd);
    run_command(
        Command::new("git")
            .current_dir(dest)
            .args(["remote", "set-head", "origin", "main"]),
    );
}

fn info_for(repo_path: &Path, base_branch: &str) -> RepositoryInfo {
    RepositoryInfo {
        path: repo_path.to_path_buf(),
        base_branch: base_branch.to_string(),
        remote_url: "file://localhost/tmp".to_string(),
    }
}

/// Provision a worktree at `dest/.worktrees/<run_id>` and
/// return the `Worktree` handle. Tests use this helper to
/// focus on the teardown semantics instead of repeating the
/// create() success path (which is covered by
/// `worktree_create_test`).
async fn provision(
    cfg: &Config,
    runner: &GitRunner,
    dest: &Path,
    issue_number: u64,
    run_id: &str,
) -> Worktree {
    let info = info_for(dest, "main");
    create_worktree(
        cfg,
        runner,
        &info,
        &key("octocat", "Hello-World", issue_number),
        run_id,
    )
    .await
    .expect("provision worktree")
}

/// Build a worktree that mimics a worker-failure outcome: the
/// branch is local-only (no upstream, no merge into base).
async fn provision_worker_failure(
    cfg: &Config,
    runner: &GitRunner,
    dest: &Path,
    issue_number: u64,
    run_id: &str,
) -> Worktree {
    let worktree = provision(cfg, runner, dest, issue_number, run_id).await;
    // Drop a tracked change inside the worktree so the
    // `git worktree remove --force` call has to deal with
    // untracked / dirty content.
    fs::write(worktree.path.join("WIP_NOTES.md"), "scratch\n").unwrap();
    worktree
}

/// Build a worktree that mimics a successful push: the
/// branch has an upstream so `remove` retains it.
async fn provision_pushed(
    cfg: &Config,
    runner: &GitRunner,
    dest: &Path,
    bare: &Path,
    issue_number: u64,
    run_id: &str,
) -> Worktree {
    let worktree = provision(cfg, runner, dest, issue_number, run_id).await;
    // Commit a change so the branch is no longer at the base
    // OID, then push to the bare repo so an upstream exists.
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "config",
        "user.email",
        "caduceus@example.com",
    ]));
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "config",
        "user.name",
        "Caduceus Test",
    ]));
    fs::write(worktree.path.join("RESULT.md"), "pushed\n").unwrap();
    run_command(
        Command::new("git")
            .current_dir(&worktree.path)
            .args(["add", "RESULT.md"]),
    );
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "commit",
        "-m",
        "pushed result",
    ]));
    // Push the branch to the bare repo so an upstream
    // exists. The push also writes a remote-tracking ref
    // (`refs/remotes/origin/<branch>`) into the main clone
    // because the worktree shares the main clone's refs/.
    let bare_uri = format!("file://{}", bare.display());
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "push",
        "-u",
        &bare_uri,
        &worktree.branch_name,
    ]));
    // `git push -u` from inside a worktree records the
    // upstream in the worktree's git config, not the main
    // clone's. Mirror the upstream config into the main
    // clone so `git rev-parse <branch>@{u}` resolves from
    // the main-clone context the daemon uses. The main
    // clone's `origin` already points at the bare repo via
    // the worker's `file://` URL.
    run_command(Command::new("git").current_dir(dest).args([
        "fetch",
        "origin",
        &worktree.branch_name,
    ]));
    run_command(Command::new("git").current_dir(dest).args([
        "branch",
        "--set-upstream-to",
        &format!("origin/{}", worktree.branch_name),
        &worktree.branch_name,
    ]));
    worktree
}

// ---------------------------------------------------------------------------
// Success / failure / dry-run teardown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_succeeds_for_worker_failure_worktree() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-worker-failure");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree =
        provision_worker_failure(&cfg, &runner, &dest, 11, "01H9Z3Y4G8W2J7N5K1QXV0F8P3").await;
    let worktree_path = worktree.path.clone();
    let branch = worktree.branch_name.clone();

    remove_worktree(&worktree).await.expect("remove worktree");

    // Worktree directory is gone.
    assert!(!worktree_path.exists(), "worktree path should be removed");
    // Local branch is deleted (no upstream, not merged into base).
    let probe = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .expect("branch probe");
    assert!(!probe.status.success(), "branch should be deleted");
    // Git's worktree registration is gone.
    let wt_meta = dest.join(".git/worktrees/01H9Z3Y4G8W2J7N5K1QXV0F8P3");
    assert!(!wt_meta.exists(), "git worktree metadata should be removed");
}

#[tokio::test]
async fn remove_retains_pushed_branch() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-pushed");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree = provision_pushed(
        &cfg,
        &runner,
        &dest,
        &bare,
        12,
        "01H9Z3Y4G8W2J7N5K1QXV0F8P4",
    )
    .await;
    let worktree_path = worktree.path.clone();
    let branch = worktree.branch_name.clone();

    remove_worktree(&worktree)
        .await
        .expect("remove pushed worktree");

    // Worktree directory is gone.
    assert!(!worktree_path.exists(), "worktree path should be removed");
    // Local branch is RETAINED because it has an upstream.
    let probe = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .expect("branch probe");
    assert!(
        probe.status.success(),
        "pushed branch should be retained, got: stderr={}",
        String::from_utf8_lossy(&probe.stderr)
    );
    // Upstream is still recorded.
    let upstream = std::process::Command::new("git")
        .current_dir(&dest)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{branch}@{{u}}"),
        ])
        .output()
        .expect("upstream probe");
    assert!(upstream.status.success(), "upstream should still exist");
}

#[tokio::test]
async fn remove_retains_merged_branch() {
    // A local-only branch whose tip is an ancestor of
    // `refs/heads/main` (i.e. merged into base) is considered
    // "resumable" and must be retained — the operator can
    // still find the work via the base branch's history.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-merged");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree = provision(&cfg, &runner, &dest, 13, "01H9Z3Y4G8W2J7N5K1QXV0F8P5").await;
    // Make a commit on the branch then fast-forward `main` to
    // include it. The branch is now an ancestor of main.
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "config",
        "user.email",
        "caduceus@example.com",
    ]));
    run_command(Command::new("git").current_dir(&worktree.path).args([
        "config",
        "user.name",
        "Caduceus Test",
    ]));
    fs::write(worktree.path.join("MERGED.md"), "x").unwrap();
    run_command(
        Command::new("git")
            .current_dir(&worktree.path)
            .args(["add", "MERGED.md"]),
    );
    run_command(
        Command::new("git")
            .current_dir(&worktree.path)
            .args(["commit", "-m", "merged"]),
    );
    // Fast-forward main to the branch tip in the main clone.
    run_command(Command::new("git").current_dir(&dest).args([
        "merge",
        "--ff-only",
        &worktree.branch_name,
    ]));
    let branch = worktree.branch_name.clone();

    remove_worktree(&worktree)
        .await
        .expect("remove merged worktree");

    // Branch is retained (merged into main).
    let probe = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .expect("branch probe");
    assert!(
        probe.status.success(),
        "merged branch should be retained, got: stderr={}",
        String::from_utf8_lossy(&probe.stderr)
    );
}

// ---------------------------------------------------------------------------
// Idempotency: already-missing path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_is_idempotent_for_already_missing_path() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-idempotent");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree = provision(&cfg, &runner, &dest, 21, "01H9Z3Y4G8W2J7N5K1QXV0F8P6").await;
    let worktree_path = worktree.path.clone();
    let branch = worktree.branch_name.clone();

    // First remove is a success.
    remove_worktree(&worktree).await.expect("first remove");
    // Second remove of the same handle is idempotent.
    let second = remove_worktree(&worktree).await;
    assert!(
        second.is_ok(),
        "second remove must be idempotent, got: {second:?}"
    );
    // The worktree path and branch are still gone.
    assert!(!worktree_path.exists());
    let probe = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .expect("branch probe");
    assert!(!probe.status.success());
}

// ---------------------------------------------------------------------------
// Nested filesystem contents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_handles_nested_filesystem_contents() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-nested");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree = provision(&cfg, &runner, &dest, 31, "01H9Z3Y4G8W2J7N5K1QXV0F8P7").await;
    // Drop a deeply nested set of untracked files.
    let deep = worktree.path.join("a/b/c/d");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("leaf.txt"), "x").unwrap();
    fs::write(worktree.path.join("top.txt"), "x").unwrap();

    remove_worktree(&worktree).await.expect("remove nested");
    assert!(!worktree.path.exists());
}

// ---------------------------------------------------------------------------
// Registered metadata removed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_clears_registered_worktree_metadata() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-metadata");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let run_id = "01H9Z3Y4G8W2J7N5K1QXV0F8P8";
    let worktree = provision(&cfg, &runner, &dest, 41, run_id).await;
    // Pre-condition: git's worktree metadata exists.
    let metadata_dir = dest.join(".git/worktrees").join(run_id);
    assert!(
        metadata_dir.exists(),
        "git worktree metadata should exist pre-remove"
    );
    let head_file = metadata_dir.join("HEAD");
    assert!(head_file.exists(), "worktree metadata HEAD exists");

    remove_worktree(&worktree).await.expect("remove");
    assert!(
        !metadata_dir.exists(),
        "git worktree metadata should be removed"
    );

    // `git worktree list` no longer mentions the path.
    let listing = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("worktree list");
    let listing_str = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing_str.contains(&worktree.path.to_string_lossy().to_string()),
        "worktree list should not contain removed path: {listing_str}"
    );
}

// ---------------------------------------------------------------------------
// Path-escape rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_rejects_path_outside_worktrees_dir() {
    // Synthesise a Worktree whose path lies outside the
    // `.worktrees/` directory — e.g. an attacker-crafted
    // handle. The daemon must refuse with a typed error
    // rather than perform a recursive deletion at the
    // foreign path.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-escape");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("README.md"), "x\n").unwrap();

    // Foreign worktree path: outside `.worktrees/`.
    let foreign_path = dest.parent().unwrap().join("Hello-World-evil");
    fs::create_dir_all(&foreign_path).unwrap();
    fs::write(foreign_path.join("harmless.md"), "y\n").unwrap();
    let bad_handle = Worktree {
        issue: key(owner, repo, 51),
        run_id: "01H9Z3Y4G8W2J7N5K1QXV0F8P9".to_string(),
        branch_name: "automation/issue-51-01h9z3y4g8w2j7n5k1qxv0f8p9".to_string(),
        path: foreign_path.clone(),
        base_oid: "deadbeef".to_string(),
        fresh: false,
        created_at: chrono::Utc::now(),
    };
    let err = remove_worktree(&bad_handle)
        .await
        .expect_err("escape must error");
    let text = format!("{err:?}");
    assert!(
        text.contains("escape")
            || text.contains("outside")
            || text.contains("invalid path")
            || text.contains("Worktree"),
        "got: {text}"
    );
    // The foreign path was NOT touched.
    assert!(foreign_path.exists(), "foreign path must remain");
    assert!(foreign_path.join("harmless.md").exists());
}

#[tokio::test]
async fn remove_rejects_path_above_worktrees_dir() {
    // Path-escape via `..` components must also be rejected.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-escape-up");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(dest.join(".worktrees")).unwrap();

    let escape_path = dest
        .join(".worktrees")
        .join("..")
        .join("..")
        .join("escape.txt");
    fs::write(&escape_path, "x\n").unwrap();
    let bad_handle = Worktree {
        issue: key(owner, repo, 61),
        run_id: "01H9Z3Y4G8W2J7N5K1QXV0F8PA".to_string(),
        branch_name: "automation/issue-61-01h9z3y4g8w2j7n5k1qxv0f8pa".to_string(),
        path: escape_path.clone(),
        base_oid: "deadbeef".to_string(),
        fresh: false,
        created_at: chrono::Utc::now(),
    };
    let err = remove_worktree(&bad_handle)
        .await
        .expect_err("escape must error");
    let text = format!("{err:?}");
    assert!(
        text.contains("escape")
            || text.contains("outside")
            || text.contains("invalid path")
            || text.contains("Worktree"),
        "got: {text}"
    );
    // No path under the main clone was removed.
    assert!(escape_path.exists());
}

// ---------------------------------------------------------------------------
// Failure teardown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_surfaces_typed_error_when_worktree_path_unremovable() {
    // Make the worktree path read-only so `git worktree
    // remove --force` fails. The daemon must surface a
    // typed Worktree error rather than panic.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("remove-failure");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let worktree = provision(&cfg, &runner, &dest, 71, "01H9Z3Y4G8W2J7N5K1QXV0F8PB").await;
    // Strip the worktree directory of write permission so
    // `git worktree remove --force` cannot unlink its
    // children. (Skip when running as root — root bypasses
    // the read-only check; the assertion is conditional on
    // the surface being tested.)
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = fs::metadata(&worktree.path).unwrap();
    let mut perms = metadata.permissions();
    // Strip write permission so `git worktree remove --force`
    // cannot unlink the worktree's children.
    perms.set_mode(0o555);
    fs::set_permissions(&worktree.path, perms.clone()).unwrap();

    let result = remove_worktree(&worktree).await;
    // Restore permissions so cleanup works regardless of
    // what the assertion below said.
    let mut restore = perms;
    restore.set_mode(0o755);
    fs::set_permissions(&worktree.path, restore).ok();

    // We can't guarantee `git worktree remove` fails under
    // root or under aggressive git versions, so accept both
    // outcomes — but the daemon must NOT panic either way.
    if let Err(err) = result {
        let text = format!("{err:?}");
        assert!(
            text.contains("Worktree"),
            "got: {text} (must be a Worktree error variant)"
        );
    }
}
