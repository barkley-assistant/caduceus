//! Regression test for the dual-writer transcript race (#134, part of
//! #94).
//!
//! Both the supervisor (`main.rs` `run_supervisor_mode`) and the daemon
//! (`dispatch::run_supervisor`) opened the same run transcript with
//! `truncate(true)`. Whichever opened second truncated the first's
//! bytes. In production the supervisor opens first, so the daemon's
//! later open silently discarded worker output written before it.
//!
//! The fix makes the supervisor the sole owner of the transcript; the
//! daemon no longer opens it. These tests drive the real `caduceus`
//! binary end-to-end through `supervise`, the same path the daemon's
//! `TrustedHostExecutor` uses, and assert that both worker streams
//! survive the full daemon -> supervisor -> worker handshake.

#![cfg(target_os = "linux")]

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::path::PathBuf;

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
    dir.push(format!("caduceus-transcript-owner-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn find_self_exe() -> PathBuf {
    fixtures::ReleaseBinary::locate()
}

fn issue_key() -> IssueKey {
    IssueKey::parse("test-owner/test-repo#134").expect("valid key")
}

fn cfg_for(root: &std::path::Path) -> Config {
    Config::test_defaults(root)
}

fn worker_command(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// A worker that writes distinct markers to stdout and stderr must have
/// BOTH markers in the transcript after the full daemon -> supervisor
/// -> worker handshake. Before the fix, the daemon's second
/// `truncate(true)` open of the same path could discard the worker's
/// stdout bytes. Regression for #134.
#[tokio::test]
async fn transcript_keeps_worker_output_after_daemon_handshake() {
    let dir = tempdir("owner");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let cfg = cfg_for(&dir);
    let run_id = "RUN_OWNER_134";

    let outcome = supervise(
        &find_self_exe(),
        &cfg,
        &issue_key(),
        &worktree,
        run_id,
        r#"{}"#,
        &worker_command(&[
            "sh",
            "-c",
            "echo STDOUT_MARKER_134; echo STDERR_MARKER_134 1>&2; exit 0",
        ]),
        CancellationToken::new(),
        "title",
        "body",
        &[],
        "automation/issue-134",
    )
    .await
    .expect("supervise ok");

    assert_eq!(outcome.status, 0, "worker must exit 0");
    let transcript = cfg.state_dir.join("runs").join(format!("{run_id}.log"));
    let body = fs::read_to_string(&transcript).expect("read transcript");
    assert!(body.contains("STDOUT_MARKER_134"), "stdout lost: {body:?}");
    assert!(body.contains("STDERR_MARKER_134"), "stderr lost: {body:?}");
}
