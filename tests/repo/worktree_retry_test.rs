//! Retry behavior for failed-run worktrees (issue #177).
//!
//! * A prior attempt of the same issue is detected by its branch
//!   prefix (`automation/issue-<N>-*`) and is archived/removed/recreated
//!   instead of surfacing a "foreign run id" collision.
//! * A prior attempt of a different issue is still refused as a
//!   genuine collision.
//! * The attic archives a working tree when `archive_on_retry` is
//!   enabled, and the daemon sweep prunes archives by age.

#![allow(unused_variables)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::worktree::{create as create_worktree, GitRunner, RepositoryInfo, Worktree};
use filetime::{set_file_mtime, FileTime};

const HAPPY_RUN_ID: &str = "01H9Z3Y4G8W2J7N5K1QXV0F8P3";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-worktree-retry-{label}-{nonce}"));
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

fn init_bare_repo(path: &Path) -> String {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    fs::write(path.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
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
    assert!(output.status.success(), "hash-object failed");
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-m", "initial"])
        .output()
        .expect("commit-tree");
    assert!(output.status.success(), "commit-tree failed");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit,
    ]));
    commit
}

fn clone_into(remote: &Path, dest: &Path) {
    let remote_uri = format!("file://{}", remote.display());
    run_command(
        Command::new("git")
            .arg("clone")
            .arg("-b")
            .arg("main")
            .arg(&remote_uri)
            .arg(dest),
    );
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

fn branch_ref_exists(repo_path: &Path, branch_name: &str) -> bool {
    Command::new("git")
        .current_dir(repo_path)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch_name}"),
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn run_git(runner: &GitRunner, cwd: &Path, args: &[&str]) -> caduceus::worktree::GitOutput {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    let temp_root = tempdir("run-git-shim");
    let cfg = config_for(&temp_root, "https://api.github.com");
    runner
        .run_in(&cfg, "fixture", &borrowed, Some(cwd))
        .await
        .expect("git fixture")
}

#[tokio::test]
async fn retry_different_run_id_preserves_old_branch_without_archive() {
    // A realistic retry uses a fresh run id. The old worktree path
    // is removed, but the old branch ref stays in the object store
    // so an operator can still inspect the prior attempt's commits.
    let owner = "octocat";
    let repo = "repo";
    let root = tempdir("retry-diff-run");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdirs = root.join("workdirs");
    fs::create_dir_all(&workdirs).unwrap();
    let dest = workdirs.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let mut cfg = config_for(&root, "https://api.github.com");
    cfg.archive_on_retry = false;
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let issue_key = key(owner, repo, 7);

    let first: Worktree = create_worktree(&cfg, &runner, &info, &issue_key, HAPPY_RUN_ID)
        .await
        .expect("first create");
    assert!(first.path.is_dir());

    let new_run_id = "01H9Z3Y4G8W2J7N5K1QXV0F8NEW";
    let second: Worktree = create_worktree(&cfg, &runner, &info, &issue_key, new_run_id)
        .await
        .expect("retry create with new run id");
    assert!(second.path.is_dir());
    assert_ne!(second.path, first.path);

    let branch_out = run_git(&runner, &dest, &["branch", "-a"]).await;
    let branches: Vec<String> = branch_out
        .stdout
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("* ")
                .trim_start_matches("+ ")
                .to_string()
        })
        .collect();
    assert!(
        branches.iter().any(|b| b == &second.branch_name),
        "new branch ref must exist"
    );
    assert!(
        branches.iter().any(|b| b == &first.branch_name),
        "old branch ref from prior attempt must survive retry (AC #2)"
    );

    // No archive should have been written.
    let attic = cfg.state_dir.join("attic");
    assert!(!attic.exists() || fs::read_dir(&attic).unwrap().count() == 0);
}

#[tokio::test]
async fn retry_same_run_id_is_idempotent_when_worktree_present() {
    // The daemon resume path re-enters create_worktree with the same
    // run_id. It must return the existing handle without archiving,
    // removing, or recreating the worktree.
    let owner = "octocat";
    let repo = "repo";
    let root = tempdir("retry-same-run-idempotent");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdirs = root.join("workdirs");
    fs::create_dir_all(&workdirs).unwrap();
    let dest = workdirs.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let mut cfg = config_for(&root, "https://api.github.com");
    cfg.archive_on_retry = true; // even when enabled, must not archive
    cfg.attic_retention_days = 30;
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let issue_key = key(owner, repo, 10);

    let first: Worktree = create_worktree(&cfg, &runner, &info, &issue_key, HAPPY_RUN_ID)
        .await
        .expect("first create");
    assert!(first.fresh, "initial create must report fresh");
    assert!(first.path.is_dir());

    let second: Worktree = create_worktree(&cfg, &runner, &info, &issue_key, HAPPY_RUN_ID)
        .await
        .expect("same-run-id re-entry must be idempotent");
    assert!(!second.fresh, "same-run-id re-entry must not report fresh");
    assert_eq!(second.path, first.path);
    assert_eq!(second.branch_name, first.branch_name);
    assert!(
        branch_ref_exists(&dest, &first.branch_name),
        "branch ref must survive idempotent re-entry"
    );

    // Same-run-id re-entry must never archive.
    let attic = cfg.state_dir.join("attic");
    assert!(!attic.exists() || fs::read_dir(&attic).unwrap().count() == 0);
}

#[tokio::test]
async fn retry_different_run_id_archives_working_tree_when_enabled() {
    // Archiving is only meaningful when the daemon is replacing a
    // prior attempt's worktree with a *new* run id. Same-run-id
    // re-entry must be idempotent and never archive.
    let owner = "octocat";
    let repo = "repo";
    let root = tempdir("retry-archive");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdirs = root.join("workdirs");
    fs::create_dir_all(&workdirs).unwrap();
    let dest = workdirs.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let mut cfg = config_for(&root, "https://api.github.com");
    cfg.archive_on_retry = true;
    cfg.attic_retention_days = 30;
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let issue_key = key(owner, repo, 8);

    let first = create_worktree(&cfg, &runner, &info, &issue_key, HAPPY_RUN_ID)
        .await
        .expect("first create");
    let marker = first.path.join("retry-marker.txt");
    fs::write(&marker, "preserve me").unwrap();

    let new_run_id = "01H9Z3Y4G8W2J7N5K1QXV0F8NEW";
    let second = create_worktree(&cfg, &runner, &info, &issue_key, new_run_id)
        .await
        .expect("retry create with archive");
    assert!(second.path.is_dir());
    assert_ne!(second.path, first.path);
    assert!(
        !second.path.join("retry-marker.txt").exists(),
        "recreated worktree must start clean"
    );

    let attic = cfg.state_dir.join("attic");
    assert!(attic.is_dir(), "attic dir should be created");
    let archives: Vec<PathBuf> = fs::read_dir(&attic)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(archives.len(), 1, "exactly one archive expected");
    let archive = &archives[0];
    assert!(
        archive.extension().is_some_and(|e| e == "tar"),
        "archive must be a plain .tar"
    );

    let listing = run_with_label("tar-list", Command::new("tar").arg("-tf").arg(archive));
    assert!(
        listing.contains("retry-marker.txt"),
        "archive should contain the marker file; got: {listing}"
    );
}

#[tokio::test]
async fn retry_refuses_foreign_issue_worktree() {
    let owner = "octocat";
    let repo = "repo";
    let root = tempdir("foreign-issue");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdirs = root.join("workdirs");
    fs::create_dir_all(&workdirs).unwrap();
    let dest = workdirs.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");

    // Pre-create a worktree for a different issue at the same path
    // the target create() would use.
    let foreign_branch = format!("automation/issue-99-{}", HAPPY_RUN_ID.to_ascii_lowercase());
    let target_path = cfg
        .state_dir
        .join("worktrees")
        .join(owner)
        .join(repo)
        .join(HAPPY_RUN_ID);
    run_command(Command::new("git").current_dir(&dest).args([
        "worktree",
        "add",
        "-b",
        &foreign_branch,
        target_path.to_str().unwrap(),
        "origin/main",
    ]));

    let err = create_worktree(&cfg, &runner, &info, &key(owner, repo, 7), HAPPY_RUN_ID)
        .await
        .expect_err("foreign issue worktree must collide");
    let text = format!("{err:?}");
    assert!(
        text.contains("collision") || text.contains("already exists"),
        "got: {text}"
    );

    let attic = cfg.state_dir.join("attic");
    assert!(!attic.exists() || fs::read_dir(&attic).unwrap().count() == 0);
}

#[tokio::test]
async fn retry_cleans_prior_same_issue_run_id_under_repo_state_dir() {
    // Simulates a real retry after a failed attempt: a new run_id is
    // issued, but an old worktree for the same issue is still sitting
    // under the per-repo state directory.
    let owner = "octocat";
    let repo = "repo";
    let root = tempdir("retry-different-run");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdirs = root.join("workdirs");
    fs::create_dir_all(&workdirs).unwrap();
    let dest = workdirs.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = info_for(&dest, "main");
    let issue_key = key(owner, repo, 9);

    let old_run_id = "01H9Z3Y4G8W2J7N5K1QXV0F8OLD";
    let old_branch = format!(
        "automation/issue-{}-{}",
        issue_key.number,
        old_run_id.to_ascii_lowercase()
    );
    let old_path = cfg
        .state_dir
        .join("worktrees")
        .join(owner)
        .join(repo)
        .join(old_run_id);
    run_command(Command::new("git").current_dir(&dest).args([
        "worktree",
        "add",
        "-b",
        &old_branch,
        old_path.to_str().unwrap(),
        "origin/main",
    ]));

    let new_run_id = "01H9Z3Y4G8W2J7N5K1QXV0F8NEW";
    let handle = create_worktree(&cfg, &runner, &info, &issue_key, new_run_id)
        .await
        .expect("retry with new run id must clean old same-issue worktree");
    assert_eq!(handle.run_id, new_run_id);
    assert!(
        !old_path.exists(),
        "old same-issue worktree path should be removed"
    );
    assert!(handle.path.exists(), "new worktree path should exist");
    assert!(
        branch_ref_exists(&dest, &old_branch),
        "old branch ref must survive removal of prior-attempt worktree (AC #2)"
    );
}

#[test]
fn attic_sweep_removes_expired_archives_only() {
    let root = tempdir("attic-sweep");
    let mut cfg = config_for(&root, "https://api.github.com");
    cfg.attic_retention_days = 30;

    let attic = cfg.state_dir.join("attic");
    fs::create_dir_all(&attic).unwrap();

    let old = attic.join("owner-repo-8-old-100.tar");
    let fresh = attic.join("owner-repo-8-fresh-200.tar");
    fs::write(&old, b"old").unwrap();
    fs::write(&fresh, b"fresh").unwrap();

    let long_ago = chrono::Utc::now() - chrono::Duration::days(60);
    set_file_mtime(&old, FileTime::from_unix_time(long_ago.timestamp(), 0)).unwrap();

    let removed = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(caduceus::worktree::sweep(&cfg))
        .expect("sweep");
    assert_eq!(removed, 1);
    assert!(!old.exists());
    assert!(fresh.exists());
}
