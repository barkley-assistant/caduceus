//! Tests for the OciExecutor stub.
//!
//! Verifies the runtime rejection contract: the OciExecutor parses in
//! config (config loads cleanly) but returns
//! `CaduceusError::OciNotImplementedYet` from `run` with an error
//! message that names Task 6.2 as the unblocking work unit. The
//! dispatch never spawns a subprocess.

use std::sync::Arc;
use std::time::Instant;

use caduceus::executor::oci::OciExecutor;
use caduceus::executor::{Executor, ExecutorSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;
use tempfile::TempDir;

fn issue_key() -> IssueKey {
    IssueKey::parse("test-owner/test-repo#1").expect("valid key")
}

fn test_cfg() -> Config {
    let tmp = TempDir::new().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn test_spec() -> ExecutorSpec {
    ExecutorSpec {
        self_exe: "/usr/bin/caduceus".into(),
        issue: issue_key(),
        worktree: "/tmp/test-worktree".into(),
        run_id: "oci-run-1".to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

// ---------------------------------------------------------------------------
// SCN-03.3: oci_executor_returns_not_implemented
// ---------------------------------------------------------------------------

/// `OciExecutor::run` returns `CaduceusError::OciNotImplementedYet`
/// whose `Display` contains "Task 6.2".
#[tokio::test]
async fn oci_executor_returns_not_implemented() {
    let cfg = test_cfg();
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(cfg));
    let spec = test_spec();
    let err = executor
        .run(&spec)
        .await
        .expect_err("OciExecutor::run must return an error");
    match err {
        CaduceusError::OciNotImplementedYet => {}
        other => panic!("expected OciNotImplementedYet; got: {other:?}"),
    }
    let display = format!("{}", CaduceusError::OciNotImplementedYet);
    assert!(
        display.contains("Task 6.2"),
        "error message must name Task 6.2; got: {display}"
    );
}

// ---------------------------------------------------------------------------
// oci_executor_does_not_spawn
// ---------------------------------------------------------------------------

/// `OciExecutor::run` returns immediately without spawning a subprocess.
/// We assert this by measuring elapsed time — the stub returns in
/// well under 100ms. A real spawn would take longer (process creation,
/// tokio scheduling, etc.).
#[tokio::test]
async fn oci_executor_does_not_spawn() {
    let cfg = test_cfg();
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(cfg));
    let spec = test_spec();
    let started = Instant::now();
    let _ = executor.run(&spec).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_millis() < 100,
        "OciExecutor::run returned in {elapsed:?} — must be immediate (no spawn)"
    );
}
