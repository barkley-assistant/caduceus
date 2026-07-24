//! * SSH/HTTPS origin normalization (`parse_origin`).
//! * Official `github.com` host validation.
//! * Enterprise host validation (origin must equal the
//!   configured api_base host verbatim).
//! * SSH host-alias / host-mismatch rejection.
//! * Missing repository directory.
//! * Non-git directory.
//! * Slug mismatch (origin `owner/repo` differs from the issue
//!   slug).
//! * Detached HEAD without `refs/remotes/origin/HEAD`.
//! * Dirty main checkout.
//! * Paths containing spaces.
//! * Prompt suppression (`GIT_TERMINAL_PROMPT=0` on the child).
//! * Timeout with SSH-like grandchild (the runner kills the
//!   process group on `git_timeout_seconds`).
//! * Cancellation via the shared `GitRunner::cancel`.
//! * Stderr redaction (token-shaped substrings stripped) and
//!   truncation (cap marker present when the body exceeds the
//!   limit).
//!
//! All tests use deterministic local fixtures; no network or
//! real GitHub is touched.

#![allow(unused_variables, unused_imports)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::worktree::{
    find_main_clone, parse_origin, validate_origin_host, GitOutput, GitRunner, RepositoryInfo,
};

/// Build a temporary directory with a unique name so parallel
/// test invocations don't collide.
fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-repo-discovery-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// Default config rooted at *root*; the daemon's pre-flight
/// checks aren't exercised here so we skip the worker command.
fn config_for(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.git_timeout_seconds = 2;
    cfg
}

fn key(owner: &str, repo: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

/// Run a shell command via `std::process::Command` and panic on
/// failure with the captured stderr inlined.
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

/// Build a fresh non-bare clone of *remote* into *dest*. The
/// caller-supplied *remote* must already exist on disk (use
/// [`init_bare_repo`] first).
fn clone_into(remote: &Path, dest: &Path) {
    run_command(Command::new("git").arg("clone").arg(remote).arg(dest));
}

/// Initialise an empty bare repository at *path* with a `main`
/// branch that has one empty commit so the cloned repo has
/// something to set `refs/remotes/origin/HEAD` against.
fn init_bare_repo(path: &Path) {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    // Point the bare repo's HEAD at `refs/heads/main` so
    // `git remote set-head origin main` succeeds on the
    // resulting clone.
    let head_path = path.join("HEAD");
    fs::write(&head_path, "ref: refs/heads/main\n").expect("write HEAD");
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    // `git commit` cannot run in a bare repo; create an empty
    // tree object and point refs/heads/main at it so the
    // ref exists before the clone.
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
}

/// Initialise a working clone with a single empty commit so
/// `git status --porcelain` is clean. The remote is set to
/// *remote_url* (the literal string git stores in
/// `remote.origin.url`).
fn init_working_repo(path: &Path, remote_url: &str) {
    fs::create_dir_all(path).expect("create dir");
    run_command(
        Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(path),
    );
    run_command(Command::new("git").current_dir(path).args([
        "config",
        "user.email",
        "caduceus@example.com",
    ]));
    run_command(Command::new("git").current_dir(path).args([
        "config",
        "user.name",
        "Caduceus Test",
    ]));
    fs::write(path.join("README.md"), "test repo\n").expect("write readme");
    run_command(
        Command::new("git")
            .current_dir(path)
            .arg("add")
            .arg("README.md"),
    );
    run_command(
        Command::new("git")
            .current_dir(path)
            .args(["commit", "-m", "initial"]),
    );
    run_command(
        Command::new("git")
            .current_dir(path)
            .args(["remote", "add", "origin", remote_url]),
    );
}

// Origin URL normalization

#[test]
fn parse_origin_handles_ssh_form() {
    let (owner, repo) = parse_origin("git@github.com:octocat/Hello-World.git").unwrap();
    assert_eq!(owner, "octocat");
    assert_eq!(repo, "Hello-World");
}

#[test]
fn parse_origin_handles_https_form() {
    let (owner, repo) = parse_origin("https://github.com/octocat/Hello-World.git").unwrap();
    assert_eq!(owner, "octocat");
    assert_eq!(repo, "Hello-World");
}

#[test]
fn parse_origin_handles_git_protocol_form() {
    let (owner, repo) = parse_origin("git://github.com/octocat/Hello-World.git").unwrap();
    assert_eq!(owner, "octocat");
    assert_eq!(repo, "Hello-World");
}

#[test]
fn parse_origin_handles_https_without_dot_git_suffix() {
    let (owner, repo) = parse_origin("https://github.com/octocat/Hello-World").unwrap();
    assert_eq!(owner, "octocat");
    assert_eq!(repo, "Hello-World");
}

#[test]
fn parse_origin_handles_ssh_with_bare_user_at() {
    let (owner, repo) = parse_origin("octocat@github.com:owner/Hello-World.git").unwrap();
    assert_eq!(owner, "owner");
    assert_eq!(repo, "Hello-World");
}

// Host validation

#[test]
fn validate_origin_host_accepts_official_github_com_with_https() {
    validate_origin_host(
        "https://github.com/octocat/Hello-World.git",
        "https://api.github.com",
    )
    .unwrap();
}

#[test]
fn validate_origin_host_accepts_official_github_com_with_ssh() {
    validate_origin_host(
        "git@github.com:octocat/Hello-World.git",
        "https://api.github.com",
    )
    .unwrap();
}

#[test]
fn validate_origin_host_rejects_enterprise_host_when_api_base_is_public() {
    let err = validate_origin_host(
        "https://ghe.example.com/octocat/Hello-World.git",
        "https://api.github.com",
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("origin host"), "got: {text}");
    assert!(text.contains("github.com"), "got: {text}");
}

#[test]
fn validate_origin_host_accepts_enterprise_origin_matching_enterprise_api_base() {
    validate_origin_host(
        "https://ghe.example.com/octocat/Hello-World.git",
        "https://ghe.example.com",
    )
    .unwrap();
}

#[test]
fn validate_origin_host_rejects_mismatched_enterprise_origin() {
    let err = validate_origin_host(
        "https://github.com/octocat/Hello-World.git",
        "https://ghe.example.com",
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("origin host"), "got: {text}");
}

#[test]
fn validate_origin_host_rejects_ssh_alias_like_github_com_attacker() {
    let err = validate_origin_host(
        "git@github.com-attacker:octocat/Hello-World.git",
        "https://api.github.com",
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("alias"), "got: {text}");
}

#[test]
fn validate_origin_host_rejects_ssh_alias_within_enterprise_host() {
    let err = validate_origin_host(
        "git@ghe.example.com-attacker:octocat/Hello-World.git",
        "https://ghe.example.com",
    )
    .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("alias"), "got: {text}");
}

// End-to-end find_main_clone paths (filesystem)

#[tokio::test]
async fn find_main_clone_succeeds_for_clean_working_repo_with_https_origin() {
    let root = tempdir("clean-https");
    let bare = root.join("remote.git");
    init_bare_repo(&bare);
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let owner = "octocat";
    let repo = "Hello-World";
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    clone_into(&bare, &dest);

    // Update the origin URL to the canonical HTTPS form so the
    // host-validation branch matches the configured api_base,
    // and explicitly set `refs/remotes/origin/HEAD` so the
    // test exercises the documented success path (rather than
    // the fallback).
    run_command(Command::new("git").current_dir(&dest).args([
        "remote",
        "set-url",
        "origin",
        "https://github.com/octocat/Hello-World.git",
    ]));
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["remote", "set-head", "origin", "main"]),
    );

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = find_main_clone(&cfg, &runner, &key(owner, repo, 1))
        .await
        .expect("discovery");
    assert_eq!(info.path, dest);
    assert_eq!(info.base_branch, "main");
    assert_eq!(
        info.remote_url, "https://github.com/octocat/Hello-World.git",
        "remote_url should be canonicalized"
    );
}

#[tokio::test]
async fn find_main_clone_succeeds_for_ssh_origin() {
    let root = tempdir("clean-ssh");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let owner = "octocat";
    let repo = "Hello-World";
    let dest = workdir.join(owner).join(repo);
    fs::create_dir_all(dest.parent().unwrap()).unwrap();

    init_working_repo(&dest, "git@github.com:octocat/Hello-World.git");
    // `init_working_repo` already created the remote; verify
    // the SSH URL passes through host validation as well.
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = find_main_clone(&cfg, &runner, &key(owner, repo, 1))
        .await
        .expect("discovery");
    assert_eq!(info.remote_url, "git@github.com:octocat/Hello-World.git");
}

#[tokio::test]
async fn find_main_clone_rejects_missing_repo() {
    let root = tempdir("missing-repo");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let err = find_main_clone(&cfg, &runner, &key("octocat", "missing", 1))
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("clone missing"), "got: {text}");
}

#[tokio::test]
async fn find_main_clone_rejects_non_git_directory() {
    let root = tempdir("non-git");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("plain");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("not-a-repo.txt"), "not a git repo\n").unwrap();

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let err = find_main_clone(&cfg, &runner, &key("octocat", "plain", 1))
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(
        text.contains("not a git repository") || text.contains("fatal"),
        "got: {text}"
    );
}

#[tokio::test]
async fn find_main_clone_rejects_slug_mismatch() {
    let root = tempdir("slug-mismatch");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("Hello-World");
    fs::create_dir_all(&dest).unwrap();
    // Set up a repo whose origin points at *owner-typo/repo*.
    init_working_repo(&dest, "https://github.com/octocat-typo/Hello-World.git");

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let err = find_main_clone(&cfg, &runner, &key("octocat", "Hello-World", 1))
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("does not match issue slug"), "got: {text}");
}

#[tokio::test]
async fn find_main_clone_rejects_dirty_main_checkout() {
    let root = tempdir("dirty");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("Hello-World");
    fs::create_dir_all(&dest).unwrap();
    init_working_repo(&dest, "https://github.com/octocat/Hello-World.git");
    // Dirty the working tree.
    fs::write(dest.join("README.md"), "modified\n").unwrap();

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let err = find_main_clone(&cfg, &runner, &key("octocat", "Hello-World", 1))
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("dirty"), "got: {text}");
}

#[tokio::test]
async fn find_main_clone_succeeds_when_workdir_base_path_contains_spaces() {
    let root = tempdir("path-with-spaces");
    let workdir = root.join("path with spaces");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("Hello-World");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    init_working_repo(&dest, "https://github.com/octocat/Hello-World.git");

    let mut cfg = config_for(&root, "https://api.github.com");
    // The test creates the clone under `path with spaces`, so
    // point the daemon's workdir_base there explicitly.
    cfg.workdir_base = workdir.clone();
    let runner = GitRunner::new(&cfg);
    let info = find_main_clone(&cfg, &runner, &key("octocat", "Hello-World", 1))
        .await
        .expect("discovery");
    assert_eq!(info.path, dest);
    assert!(info.path.to_string_lossy().contains("path with spaces"));
}

#[tokio::test]
async fn find_main_clone_handles_detached_head_without_origin_head() {
    // A freshly-`init`'d repo has no commits and no
    // refs/remotes/origin/HEAD. We expect `find_main_clone` to
    // surface a precise error rather than crash.
    let root = tempdir("detached");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("Hello-World");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    run_command(Command::new("git").arg("init").arg(&dest));

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let err = find_main_clone(&cfg, &runner, &key("octocat", "Hello-World", 1))
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    // The repo has no remote and no commits; the daemon must
    // surface either a "no remote.origin.url" error (from
    // `git config --get remote.origin.url`) or the eventual
    // "detached HEAD" error from the HEAD-resolution branch.
    assert!(
        text.contains("remote.origin.url")
            || text.contains("detached HEAD")
            || text.contains("not a git repository"),
        "got: {text}"
    );
}

#[tokio::test]
async fn find_main_clone_falls_back_to_local_head_with_warning() {
    // A repo whose origin URL matches the slug but whose
    // `refs/remotes/origin/HEAD` is missing must fall back to
    // the local HEAD (with a tracing warning) instead of
    // failing.
    let root = tempdir("origin-head-missing");
    let workdir = root.join("workdirs");
    fs::create_dir_all(&workdir).unwrap();
    let dest = workdir.join("octocat").join("Hello-World");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    init_working_repo(&dest, "https://github.com/octocat/Hello-World.git");
    // Set, then delete, `refs/remotes/origin/HEAD` so the
    // daemon's fallback path is exercised. `git remote
    // set-head origin --delete` is the documented way to
    // remove the remote HEAD symref.
    run_command(
        Command::new("git")
            .current_dir(&dest)
            .args(["remote", "set-head", "origin", "--delete"]),
    );

    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let info = find_main_clone(&cfg, &runner, &key("octocat", "Hello-World", 1))
        .await
        .expect("discovery falls back");
    assert_eq!(info.base_branch, "main");
}

// GitRunner contract — prompt suppression, timeout, cancellation,
// stderr redaction/truncation.

#[tokio::test]
async fn git_runner_sets_git_terminal_prompt_zero_on_child() {
    let root = tempdir("prompt-suppression");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    // `git -c alias.X='!...' X` runs the `...` body in a shell
    // that inherits the parent process's environment, so the
    // assertion below sees exactly what the runner exposed.
    let output = run_alias(
        &runner,
        "prompt-suppression",
        "show-prompt",
        "!printf 'GIT_TERMINAL_PROMPT=%s\\n' \"$GIT_TERMINAL_PROMPT\"",
    )
    .await;
    assert!(
        output.stdout.contains("GIT_TERMINAL_PROMPT=0"),
        "stdout: {:?}",
        output.stdout
    );
}

#[tokio::test]
async fn git_runner_does_not_inherit_github_token() {
    let root = tempdir("token-scrub");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    // Set hostile parent env BEFORE building the runner so the
    // runner's env-clear + allowlist loop scrubs it from the
    // child. We restore the original values at the end so
    // parallel tests aren't affected.
    let saved_gh = std::env::var("GITHUB_TOKEN").ok();
    let saved_cgh = std::env::var("CADUCEUS_GITHUB_TOKEN").ok();
    let saved_gh2 = std::env::var("GH_TOKEN").ok();
    std::env::set_var("GITHUB_TOKEN", "secret");
    std::env::set_var("CADUCEUS_GITHUB_TOKEN", "secret");
    std::env::set_var("GH_TOKEN", "secret");
    let output = run_alias(&runner, "token-scrub", "show-env", "!env").await;
    match saved_gh {
        Some(v) => std::env::set_var("GITHUB_TOKEN", v),
        None => std::env::remove_var("GITHUB_TOKEN"),
    }
    match saved_cgh {
        Some(v) => std::env::set_var("CADUCEUS_GITHUB_TOKEN", v),
        None => std::env::remove_var("CADUCEUS_GITHUB_TOKEN"),
    }
    match saved_gh2 {
        Some(v) => std::env::set_var("GH_TOKEN", v),
        None => std::env::remove_var("GH_TOKEN"),
    }
    assert!(
        !output.stdout.contains("GITHUB_TOKEN=secret"),
        "GITHUB_TOKEN leaked: {:?}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("GH_TOKEN=secret"),
        "GH_TOKEN leaked: {:?}",
        output.stdout
    );
    assert!(
        !output.stdout.contains("CADUCEUS_GITHUB_TOKEN=secret"),
        "CADUCEUS_GITHUB_TOKEN leaked: {:?}",
        output.stdout
    );
}

#[tokio::test]
async fn git_runner_kills_process_group_on_timeout() {
    let root = tempdir("timeout-grandchild");
    let mut cfg = config_for(&root, "https://api.github.com");
    // One-second ceiling keeps the test fast.
    cfg.git_timeout_seconds = 1;
    let runner = GitRunner::new(&cfg);
    // Spawn a long-running grandchild via the git alias. The
    // runner's process-group kill must reach the shell's
    // children too.
    let start = std::time::Instant::now();
    let output = run_alias(
        &runner,
        "timeout-grandchild",
        "long-grandchild",
        "!sleep 60",
    )
    .await;
    let elapsed = start.elapsed();
    assert!(
        output.timed_out,
        "expected timed_out flag, got: {:?}",
        output
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "group kill took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn git_runner_cancellation_takes_effect() {
    let root = tempdir("cancel");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    let runner_cancel = runner.clone();
    // Schedule the cancellation 200 ms after launch — long
    // enough for the child to have started.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        runner_cancel.cancel();
    });
    let start = std::time::Instant::now();
    let output = run_alias(&runner, "cancel", "long-cancel", "!sleep 30").await;
    let elapsed = start.elapsed();
    assert!(
        output.cancelled,
        "expected cancelled flag, got: {:?}",
        output
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "cancel took too long: {elapsed:?}"
    );
}

#[tokio::test]
async fn git_runner_redacts_token_shaped_substrings_from_stderr() {
    let root = tempdir("stderr-redact");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    // `!cmd 1>&2` makes the alias emit a token-shaped line on
    // stderr. The runner's `redact_and_cap` must replace it
    // with `<redacted>` before returning.
    let output = run_alias(
        &runner,
        "stderr-redact",
        "leak",
        "!echo 'GITHUB_TOKEN=ghp_should_never_leak' 1>&2; exit 1",
    )
    .await;
    assert!(
        output.stderr.contains("<redacted>"),
        "stderr missing redaction marker: {:?}",
        output.stderr
    );
    assert!(
        !output.stderr.contains("ghp_should_never_leak"),
        "token leaked: {:?}",
        output.stderr
    );
}

#[tokio::test]
async fn git_runner_truncates_stderr_above_cap() {
    let root = tempdir("stderr-truncate");
    let cfg = config_for(&root, "https://api.github.com");
    let runner = GitRunner::new(&cfg);
    // Emit ~40 KiB on stderr — over the runner's 32 KiB cap.
    let output = run_alias(
        &runner,
        "stderr-truncate",
        "flood",
        "!python3 -c 'import sys; sys.stderr.write(\"x\"*40000); sys.exit(1)'",
    )
    .await;
    assert!(
        output.stderr.contains("truncated"),
        "stderr missing truncation marker: len={}",
        output.stderr.len()
    );
    assert!(
        output.stderr.len() <= caduceus::worktree::GIT_OUTPUT_BYTE_CAP + 64,
        "stderr unexpectedly large: len={}",
        output.stderr.len()
    );
}

// Helpers

/// Run a `git` invocation through the runner that proxies a
/// shell command via a `!`-prefixed git alias. This lets the
/// test suite exercise properties of the runner (timeout,
/// cancellation, environment scrubbing, redaction) without
/// needing real git semantics for the inner command.
async fn run_alias(runner: &GitRunner, op: &'static str, alias: &str, body: &str) -> GitOutput {
    let config = format!("alias.{alias}={body}");
    runner
        .run(
            op,
            &[OsStr::new("-c"), OsStr::new(&config), OsStr::new(alias)],
        )
        .await
        .expect("alias runs")
}
