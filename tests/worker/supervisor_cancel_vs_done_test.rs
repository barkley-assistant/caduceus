//! Regression test for the supervisor cancel/DONE race (#130, part of
//! #94).
//!
//! The daemon-side protocol loop used a `biased` `tokio::select!` with a
//! cancel arm and a done-frame arm. After the supervisor child exited,
//! `dispatch::run_supervisor` fired `cancellation.cancel()` *before*
//! awaiting the protocol task, so the cancel arm won over a `DONE` frame
//! already buffered on stdout. A worker that exited 0 was reported as
//! `status: 130, cancelled: true`.
//!
//! The fix awaits the protocol task (which drains the `DONE` frame or
//! EOF from the now-dead supervisor) before firing the cleanup cancel.
//! These tests drive the real `caduceus` binary end-to-end through
//! `supervise`, the same path the daemon's `TrustedHostExecutor` uses.

#![cfg(target_os = "linux")]

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::worker_supervisor::supervise;
use tokio_util::sync::CancellationToken;

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-cancel-vs-done-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn find_self_exe() -> PathBuf {
    fixtures::ReleaseBinary::locate()
}

fn issue_key() -> IssueKey {
    IssueKey::parse("test-owner/test-repo#130").expect("valid key")
}

fn cfg_for(root: &std::path::Path) -> Config {
    Config::test_defaults(root)
}

fn worker_command(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// A worker that exits 0 cleanly must be reported `cancelled: false`,
/// `status: 0`. Before the fix, the cleanup cancel fired before the
/// protocol task drained the buffered `DONE` frame, and the `biased`
/// select! let the cancel arm report `status: 130, cancelled: true`.
#[tokio::test]
async fn clean_exit_reports_status_not_cancelled() {
    let dir = tempdir("clean");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let outcome = supervise(
        &find_self_exe(),
        &cfg_for(&dir),
        &issue_key(),
        &worktree,
        "RUN_CLEAN_130",
        r#"{}"#,
        &worker_command(&["sh", "-c", "exit 0"]),
        CancellationToken::new(),
        "title",
        "body",
        &[],
        "automation/issue-130",
    )
    .await
    .expect("supervise ok");

    assert_eq!(outcome.status, 0, "clean exit must report status 0");
    assert!(!outcome.cancelled, "clean exit must not report cancelled");
    assert!(!outcome.timed_out, "clean exit must not report timed out");
    assert!(!outcome.signaled, "clean exit must not report signaled");
}

/// A genuinely cancelled worker (the token is fired externally while the
/// worker is alive) must still report `cancelled: true`, `status: 130`.
/// Guards that the fix did not break the cancel path.
#[tokio::test]
async fn genuine_cancel_still_reports_cancelled() {
    let dir = tempdir("cancel");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let cancellation = CancellationToken::new();
    let cancel_fire = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_fire.cancel();
    });

    let outcome = supervise(
        &find_self_exe(),
        &cfg_for(&dir),
        &issue_key(),
        &worktree,
        "RUN_CANCEL_130",
        r#"{}"#,
        &worker_command(&["sh", "-c", "sleep 30"]),
        cancellation,
        "title",
        "body",
        &[],
        "automation/issue-130",
    )
    .await
    .expect("supervise ok");

    assert!(outcome.cancelled, "external cancel must report cancelled");
    assert_eq!(outcome.status, 130, "cancelled worker must report 130");
}
