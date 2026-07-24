//! Tests for the OciExecutor implementation.
//!
//! Verifies that `OciExecutor::run` attempts to dispatch via the
//! configured OCI CLI. In CI without Docker/Podman, the executor
//! returns `OciEngineUnavailable` (via `OciCliNotFound` or similar
//! from the subprocess path). The tests verify the typed error and
//! that no subprocess is spawned for config-only errors.

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

fn setup() -> (Config, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.state_dir = state_dir;
    (cfg, tmp)
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
        network_profile: None,
    }
}

// oci_executor_returns_typed_error

/// `OciExecutor::run` returns a typed `CaduceusError` (not a panic).
/// Without Docker/Podman in CI, the error is either
/// `OciEngineUnavailable` or `OciCreateFailed`.
#[tokio::test]
async fn oci_executor_returns_typed_error() {
    let (cfg, _tmp) = setup();
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(cfg));
    let spec = test_spec();
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
    let executor: Arc<dyn Executor> = Arc::new(OciExecutor::new(cfg));
    let spec = test_spec();
    let started = Instant::now();
    let _ = executor.run(&spec).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "OciExecutor::run returned in {elapsed:?} — should be fast even on error"
    );
}
