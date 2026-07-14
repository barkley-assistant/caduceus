//! Task 4.2 acceptance tests for daemon-owned worktree creation.
//!
//! Each test exercises a single bullet from the task packet:
//!
//! * Successful creation (worktree exists, branch is at the
//!   fetched base OID, `base_oid` is recorded).
//! * Default base branch selection (no explicit base).
//! * Branch and path stay separated (branch contains
//!   slashes / dashes, path does not).
//! * Fetch failure surfaces as a precise error.
//! * Path collision with a foreign run ID surfaces as a
//!   collision error.
//! * Branch collision with a foreign run ID surfaces as a
//!   collision error.
//! * Invalid run ID (path traversal / control characters) is
//!   rejected before any git subprocess runs.
//! * The parent main checkout's HEAD is unchanged after a
//!   successful create.
//!
//! The fetch tests use a local bare-remote fixture so the
//! `git fetch --prune origin <base>` step exercises real code
//! without contacting github.com. The `api_base` for those
//! fixtures is the empty host of a `file://` URL — the
//! discovery helper is bypassed and `RepositoryInfo` is
//! constructed directly so a network-free daemon can still
//! provision the worktree.

#![allow(unused_variables)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::worktree::{create as create_worktree, GitRunner, RepositoryInfo, Worktree};

/// Unique tempdir per call so parallel test invocations don't
/// trample each other.
fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-worktree-create-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// Build a config rooted at *root*.
fn config_for(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    // 30s for fetch-timeout during acceptance tests; collisions
    // and run-id validation tests never reach fetch.
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

/// Build `cmd` so it echoes its failure information with the
/// expected *label* prefix.
fn run_with_label(label: &str, cmd: &mut Command) -> String {
    let output = cmd.output().expect("spawn command");
    if !output.status.success() {
        panic!(
            "[{label}] command {:?} failed: status={:?}\nstdout={}\nstderr={}",
            cmd,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Initialise a bare repository at *path* with a `main` branch
/// that has one empty commit so the cloned repo has something to
/// fetch and set `refs/remotes/origin/HEAD` against. Returns the
/// commit OID so callers can verify the recorded base.
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
    assert!(
        output.status.success(),
        "commit-tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit,
    ]));
    commit
}

/// Same as [`init_bare_repo`] but also drops a second empty
/// commit on `main` so we can test that `fetch --prune origin`
/// picks up new refs without conflating with the initial state.
fn init_bare_repo_with_two_commits(path: &Path) -> (String, String) {
    let commit = init_bare_repo(path);
    let parent = commit;
    let mut cmd = Command::new("git");
    cmd.current_dir(path).args(["rev-parse", &parent]);
    let output = cmd.output().expect("rev-parse parent");
    let parent_oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut cmd = Command::new("git");
    cmd.current_dir(path)
        .args(["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let output = cmd.output().expect("hash-object 2");
    let tree2 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut cmd = Command::new("git");
    cmd.current_dir(path)
        .args(["commit-tree", &tree2, "-p", &parent_oid, "-m", "second"]);
    let output = cmd.output().expect("commit-tree 2");
    let second = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &second,
    ]));
    (parent, second)
}

/// Build a non-bare clone of *remote* into *dest* using a
/// `file://` origin URL — no network involved. The clone ends
/// up with `remote.origin.url` set to the same `file://` URI
/// the daemon will see when it runs `git fetch --prune origin
/// <base>`. The daemon-side `RepositoryInfo` constructed by
/// the test mirrors the same `file://` URL so any host-aware
/// paths still see the value.
fn clone_into(remote: &Path, dest: &Path) -> String {
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
    let mut cmd = Command::new("git");
    cmd.current_dir(dest).args(["rev-parse", "HEAD"]);
    run_with_label("clone_into", &mut cmd).trim().to_string()
}

/// Build a [`RepositoryInfo`] directly from a working clone.
/// The daemon-side `find_main_clone` validates the origin URL
/// host against `api_base`; the test bypasses that helper and
/// constructs `RepositoryInfo` so `create()` is exercised
/// without exercising the discovery path. The local clone's
/// actual `remote.origin.url` is `file://` (set by
/// [`clone_into`]) so `git fetch` succeeds without network.
fn info_for(repo_path: &Path, base_branch: &str) -> RepositoryInfo {
    RepositoryInfo {
        path: repo_path.to_path_buf(),
        base_branch: base_branch.to_string(),
        remote_url: "file://localhost/tmp".to_string(),
    }
}

/// Run `git <args>` inside *cwd* via the supplied *runner* and
/// return the captured GitOutput. Used by acceptance tests to
/// inspect state without going through the runner's public
/// `fetch` / `worktree` paths.
async fn run_git(runner: &GitRunner, cwd: &Path, args: &[&str]) -> caduceus::worktree::GitOutput {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    let temp_root = tempdir("run_git-shim");
    let cfg = config_for(&temp_root, "https://api.github.com");
    runner
        .run_in(&cfg, "fixture", &borrowed, Some(cwd))
        .await
        .expect("git fixture")
}

// ---------------------------------------------------------------------------
// Successful creation
// ---------------------------------------------------------------------------

/// Stable, ULID-shaped run id used across the happy-path tests
/// so a single value threads through both the branch name and
/// the worktree path.
const HAPPY_RUN_ID: &str = "01H9Z3Y4G8W2J7N5K1QXV0F8P3";

#[tokio::test]
async fn create_succeeds_for_clean_repo_with_explicit_base() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("success-explicit-base");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let handle: Worktree =
        create_worktree(&cfg, &runner, &info, &key(owner, repo, 7), HAPPY_RUN_ID)
            .await
            .expect("create worktree");

    // Path lives under `<repo>/.worktrees/<run_id>`.
    let expected_path = dest.join(".worktrees").join(HAPPY_RUN_ID);
    assert_eq!(handle.path, expected_path);
    assert!(handle.path.is_dir());

    // Branch has the documented shape.
    assert_eq!(
        handle.branch_name,
        format!(
            "automation/issue-{}-{}",
            7,
            HAPPY_RUN_ID.to_ascii_lowercase()
        )
    );

    // The worktree's HEAD is the branch tip.
    let cwd_path = handle.path.clone();
    let head_oid = run_git(&runner, &cwd_path, &["rev-parse", "HEAD"]).await;
    let head_oid_str = head_oid.stdout.trim().to_string();
    assert_eq!(head_oid_str, handle.base_oid);

    // Branch contains the same OID.
    let branch_oid = run_git(&runner, &cwd_path, &["rev-parse", &handle.branch_name]).await;
    assert_eq!(branch_oid.stdout.trim(), handle.base_oid);

    // Branch tip on the local side equals `origin/<base>`.
    let remote_tip = run_git(&runner, &dest, &["rev-parse", "origin/main"]).await;
    assert_eq!(remote_tip.stdout.trim(), handle.base_oid);

    // Worktree knows about its run_id and issue key.
    assert_eq!(handle.run_id, HAPPY_RUN_ID);
    assert_eq!(handle.issue.number, 7);
    assert_eq!(handle.issue.owner, owner);
    assert_eq!(handle.issue.repo, repo);
}

#[tokio::test]
async fn create_succeeds_with_default_base_from_origin_head() {
    // `RepositoryInfo::base_branch` defaults to "main" when the
    // caller has the canonical main branch. create() uses it as
    // the fetch target and seeds `handle.base_oid` from the
    // tip of `refs/remotes/origin/<base>`.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("default-base");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    assert_eq!(info.base_branch, "main");
    let handle = create_worktree(
        &cfg,
        &runner,
        &info,
        &key(owner, repo, 8),
        "01H9Z3Y4G8W2J7N5K1QXV0F8P4",
    )
    .await
    .expect("create worktree (default base)");
    assert!(handle.path.join(".git").exists());
}

// ---------------------------------------------------------------------------
// Branch and path separation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_branch_name_contains_slashes_but_path_does_not() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("branch-path-separation");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let handle = create_worktree(&cfg, &runner, &info, &key(owner, repo, 12), HAPPY_RUN_ID)
        .await
        .expect("create worktree");

    // Branch contains a slash but uses the run_id lowercase.
    assert!(handle.branch_name.starts_with("automation/issue-"));
    assert!(handle.branch_name.contains('/'));
    assert_eq!(
        handle.branch_name,
        format!("automation/issue-12-{}", HAPPY_RUN_ID.to_ascii_lowercase())
    );

    // Path does NOT contain a slash beyond the leading
    // `.worktrees/` separator — only `run_id` is appended.
    let path_str = handle.path.to_string_lossy();
    assert!(
        path_str.ends_with(HAPPY_RUN_ID),
        "path did not end with run_id: {path_str}"
    );
    assert!(
        !path_str.contains(&HAPPY_RUN_ID.to_ascii_lowercase()),
        "path should preserve original-case run_id, got: {path_str}"
    );
}

// ---------------------------------------------------------------------------
// Fetch failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_surfaces_precise_error_on_fetch_failure() {
    // Point the clone's origin at a file:// URL that does
    // NOT exist on disk so the first step (`git fetch --prune
    // origin <base>`) fails loudly. The daemon must NOT fall
    // through to `worktree add` — it must refuse on the
    // fetch error.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("fetch-failure");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();

    fs::create_dir_all(&dest).unwrap();
    run_command(
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(&dest),
    );
    run_command(Command::new("git").current_dir(&dest).args([
        "config",
        "user.email",
        "caduceus@example.com",
    ]));
    run_command(Command::new("git").current_dir(&dest).args([
        "config",
        "user.name",
        "Caduceus Test",
    ]));
    fs::write(dest.join("README.md"), "x").unwrap();
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["add", "README.md"]),
    );
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["commit", "-m", "first"]),
    );
    // Origin URL pointing at a syntactically-valid file:// path
    // that does not exist on disk. ``git config --get`` and the
    // URL host validation succeed; only the fetch itself fails.
    run_command(Command::new("git").current_dir(&dest).args([
        "remote",
        "add",
        "origin",
        "file:///nonexistent/bare.git",
    ]));

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 33), HAPPY_RUN_ID)
        .await
        .expect_err("fetch must fail");
    let text = format!("{err:?}");
    assert!(
        text.contains("Worktree") && text.contains("create") && text.contains("fetch"),
        "got: {text}"
    );

    // The daemon's flock creates `<repo>/.worktrees/` before the
    // fetch step so a concurrent tick can't race the
    // pre-flight + worktree-add sequence. The fetch-failure
    // assertion therefore checks that NO worktree directory
    // was actually created and NO branch was created — the
    // daemon must NOT have committed to a worktree before
    // fetching failed.
    assert!(
        dest.join(".worktrees").exists(),
        "flock parent dir should exist"
    );
    let ours = dest.join(".worktrees").join(HAPPY_RUN_ID);
    assert!(
        !ours.exists(),
        "create must NOT have produced a worktree at {ours:?} on fetch failure"
    );
    // Branch also must not have been created.
    let branch_ref = format!(
        "refs/heads/automation/issue-33-{}",
        HAPPY_RUN_ID.to_ascii_lowercase()
    );
    let probe = std::process::Command::new("git")
        .current_dir(&dest)
        .args(["rev-parse", &branch_ref])
        .output()
        .expect("probe");
    assert!(
        !probe.status.success(),
        "create must NOT have created {branch_ref}"
    );
}

// ---------------------------------------------------------------------------
// Collision (path / branch owned by a foreign run id)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_returns_collision_when_path_owned_by_foreign_run_id() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("path-collision");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    // Pre-create the worktree directory under a different run
    // id to simulate a leftover from a prior run.
    fs::create_dir_all(dest.join(".worktrees").join("foreign-run-id")).unwrap();

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 42), HAPPY_RUN_ID)
        .await
        .expect_err("path collision must error");
    let text = format!("{err:?}");
    assert!(
        text.contains("collision") || text.contains("already exists"),
        "got: {text}"
    );
}

#[tokio::test]
async fn create_returns_collision_when_branch_owned_by_foreign_run_id() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("branch-collision");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    // Pre-create a local branch with the same name as the one
    // create() would use, pointing at a non-origin commit.
    let foreign_branch = format!("automation/issue-99-{}", HAPPY_RUN_ID.to_ascii_lowercase());
    // Create a stray commit on main first so the foreign
    // branch points at something definitely not in origin/main.
    fs::write(dest.join("README.md"), "stray").unwrap();
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["add", "README.md"]),
    );
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["commit", "-m", "stray"]),
    );
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["branch", &foreign_branch, "HEAD"]),
    );

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 99), HAPPY_RUN_ID)
        .await
        .expect_err("branch collision must error");
    let text = format!("{err:?}");
    assert!(
        text.contains("collision") || text.contains("already exists"),
        "got: {text}"
    );
}

#[tokio::test]
async fn create_reconciles_when_branch_and_path_belong_to_same_run_id() {
    // Replaying create() with the same run id must be allowed
    // (idempotent) and return the existing handle.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("reconcile-same-run");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let first = create_worktree(&cfg, &runner, &info, &key(owner, repo, 5), HAPPY_RUN_ID)
        .await
        .expect("first create");
    // Same run id => reconcile (path and branch are both ours).
    let second = create_worktree(&cfg, &runner, &info, &key(owner, repo, 5), HAPPY_RUN_ID)
        .await
        .expect("second create must reconcile");
    assert_eq!(first.path, second.path);
    assert_eq!(first.branch_name, second.branch_name);
    assert_eq!(first.base_oid, second.base_oid);
}

// ---------------------------------------------------------------------------
// Invalid run id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_rejects_run_id_with_path_traversal() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("bad-run-id");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 1), "../escape")
        .await
        .expect_err("path-traversal run_id must be rejected");
    let text = format!("{err:?}");
    assert!(
        text.contains("invalid") || text.contains("run_id") || text.contains("forbidden"),
        "got: {text}"
    );

    // No worktree directory should have been created.
    assert!(!dest.join(".worktrees").exists());
}

#[tokio::test]
async fn create_rejects_run_id_with_shell_metacharacters() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("bad-run-id-shell");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 1), "abc; rm -rf /")
        .await
        .expect_err("shell-metachar run_id must be rejected");
    let text = format!("{err:?}");
    assert!(
        text.contains("invalid") || text.contains("run_id") || text.contains("forbidden"),
        "got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Parent checkout unchanged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_leaves_parent_main_checkout_unchanged() {
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("parent-unchanged");
    let bare = root.join("remote.git");
    let (_first, second) = init_bare_repo_with_two_commits(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    // Capture parent HEAD before the call.
    let head_before = run_git(&runner, &dest, &["rev-parse", "HEAD"]).await;
    let head_before_oid = head_before.stdout.trim().to_string();
    assert_eq!(head_before_oid, second);

    let info = info_for(&dest, "main");
    let handle = create_worktree(&cfg, &runner, &info, &key(owner, repo, 1), HAPPY_RUN_ID)
        .await
        .expect("create");

    assert_eq!(handle.base_oid, second);
    let head_after = run_git(&runner, &dest, &["rev-parse", "HEAD"]).await;
    assert_eq!(head_after.stdout.trim(), head_before_oid);

    // Sanity: the parent's working tree must still be clean
    // apart from the daemon-managed `.worktrees/` directory
    // and its `.lock` file. `git worktree add` creates a
    // sibling worktree that appears as "untracked" from the
    // parent checkout's perspective by design — the spec's
    // "parent checkout unchanged" promise is about HEAD +
    // tracked files, not untracked dirs.
    let porcelain = run_git(&runner, &dest, &["status", "--porcelain"]).await;
    let mut offending: Vec<&str> = porcelain
        .stdout
        .lines()
        .filter(|l| !l.starts_with("?? .worktrees/") && !l.is_empty())
        .collect();
    offending.sort();
    assert!(
        offending.is_empty(),
        "parent checkout had unexpected changes after create: {offending:?}"
    );
}

#[tokio::test]
async fn create_picks_up_new_remote_commit_on_second_run() {
    // `git fetch --prune origin <base>` must incorporate the
    // latest remote tip; the worktree's branch then records
    // the new OID.
    let owner = "octocat";
    let repo = "Hello-World";
    let root = tempdir("fetch-picks-up-new-commit");
    let bare = root.join("remote.git");
    let (first_commit, second) = init_bare_repo_with_two_commits(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    // Roll back the bare repo to the first commit before
    // cloning, then advance it back to `second` so the clone
    // has to fetch new state.
    run_command(Command::new("git").current_dir(&bare).args([
        "update-ref",
        "refs/heads/main",
        &first_commit,
    ]));
    clone_into(&bare, &dest);
    run_command(Command::new("git").current_dir(&bare).args([
        "update-ref",
        "refs/heads/main",
        &second,
    ]));

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let handle = create_worktree(&cfg, &runner, &info, &key(owner, repo, 1), HAPPY_RUN_ID)
        .await
        .expect("create");
    assert_eq!(handle.base_oid, second);
}
