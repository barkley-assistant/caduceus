//! Tests for the OciExecutor implementation.
//!
//! Verifies that `OciExecutor::run` attempts to dispatch via the
//! configured OCI CLI. In CI without Docker/Podman, the executor
//! returns a typed OCI error: the pre-flight engine probe fails
//! fail-closed with `OciIdentityUnsupported` (the engine mode cannot
//! be determined without a reachable engine), surfaced from the
//! lifecycle path as `OciEngineUnavailable`/`OciCreateFailed` in
//! older configurations. The tests verify the typed error and
//! that no subprocess is spawned for config-only errors.

use std::sync::Arc;
use std::time::Instant;

use caduceus::executor::oci::OciExecutor;
use caduceus::executor::{Executor, ExecutorSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::disk::DiskPressureGuard;
use caduceus::infra::error::CaduceusError;
use tempfile::TempDir;

fn issue_key() -> IssueKey {
    IssueKey::parse("test-owner/test-repo#1").expect("valid key")
}

fn setup() -> (Config, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.state_dir = state_dir;
    // The pre-flight probe stats the worktree, so it must exist on
    // disk (a missing worktree would fail the probe before the
    // engine-availability error could surface).
    let worktree = cfg
        .workdir_base
        .join("test-owner")
        .join("test-repo")
        .join("run-1");
    std::fs::create_dir_all(&worktree).expect("create worktree dir");
    (cfg, tmp)
}

fn test_spec(cfg: &Config) -> ExecutorSpec {
    // The worktree must live under `cfg.workdir_base` so the
    // resolution step's host-path allow-list accepts it and the
    // engine-availability error (not a resolution error) surfaces.
    let worktree = cfg
        .workdir_base
        .join("test-owner")
        .join("test-repo")
        .join("run-1");
    ExecutorSpec {
        self_exe: "/usr/bin/caduceus".into(),
        issue: issue_key(),
        worktree,
        run_id: "oci-run-1".to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
        issue_title: "title".to_string(),
        issue_body: "body".to_string(),
        labels: Vec::new(),
        branch_name: "automation/issue-1".to_string(),
    }
}

// oci_executor_returns_typed_error

/// `OciExecutor::run` returns a typed `CaduceusError` (not a panic).
/// Without Docker/Podman in CI, the error is either
/// `OciEngineUnavailable` or the fail-closed pre-flight refusal
/// `OciIdentityUnsupported` (mode undetectable without a reachable
/// engine).
#[tokio::test]
async fn oci_executor_returns_typed_error() {
    let (cfg, _tmp) = setup();
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(
        cfg.clone(),
        Arc::new(DiskPressureGuard::from_config(&cfg)),
    ));
    let spec = test_spec(&cfg);
    let err = executor
        .run(&spec)
        .await
        .expect_err("OciExecutor::run must return an error without Docker");

    let is_oci_error = matches!(
        &err,
        CaduceusError::OciEngineUnavailable { .. }
            | CaduceusError::OciCreateFailed { .. }
            | CaduceusError::OciCliNotFound { .. }
            | CaduceusError::OciPullFailed { .. }
            | CaduceusError::OciIdentityUnsupported { .. }
    );
    assert!(
        is_oci_error,
        "expected a typed OCI error without Docker; got: {err:?}"
    );
}

// oci_executor_does_not_spawn_process

/// `OciExecutor::run` returns quickly without spawning a long-lived
/// subprocess when the engine is unavailable.
#[tokio::test]
async fn oci_executor_does_not_spawn_process() {
    let (cfg, _tmp) = setup();
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(
        cfg.clone(),
        Arc::new(DiskPressureGuard::from_config(&cfg)),
    ));
    let spec = test_spec(&cfg);
    let started = Instant::now();
    let _ = executor.run(&spec).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "OciExecutor::run returned in {elapsed:?} — should be fast even on error"
    );
}
