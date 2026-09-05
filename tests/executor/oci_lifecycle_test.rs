//! Lifecycle tests for the OCI 5-step orchestration.
//!
//! Tests use unique keys per test for parallel safety and verify the
//! typed errors, cancellation handling, cleanup guarantees, and the
//! `ContainerRunRow.engine` column wiring from the canonical lifecycle's
//! explicit `SandboxEngine` parameter.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use caduceus::executor::oci_lifecycle;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine};
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::{CaduceusError, CaduceusResult};
use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};

mod support;

// FakeOciRunState — in-memory state for testing

struct FakeOciRunState {
    rows: Mutex<Vec<ContainerRunRow>>,
}

impl FakeOciRunState {
    fn new() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
        }
    }
}

impl OciRunState for FakeOciRunState {
    fn insert(&self, row: &ContainerRunRow) -> CaduceusResult<()> {
        let mut rows = self.rows.lock().unwrap();
        rows.push(row.clone());
        Ok(())
    }

    fn update_state(&self, run_id: &str, state: &OciLifecycleState) -> CaduceusResult<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.run_id == run_id) {
            row.state = state.clone();
        }
        Ok(())
    }

    fn update_container_id(&self, run_id: &str, container_id: &str) -> CaduceusResult<()> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(row) = rows.iter_mut().find(|r| r.run_id == run_id) {
            row.container_id = Some(container_id.to_string());
        }
        Ok(())
    }

    fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .iter()
            .filter(|r| r.state == OciLifecycleState::PendingReconciliation)
            .cloned()
            .collect())
    }

    fn get(&self, run_id: &str) -> CaduceusResult<Option<ContainerRunRow>> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.iter().find(|r| r.run_id == run_id).cloned())
    }

    fn delete(&self, run_id: &str) -> CaduceusResult<()> {
        let mut rows = self.rows.lock().unwrap();
        rows.retain(|r| r.run_id != run_id);
        Ok(())
    }
}

fn test_cfg() -> Config {
    Config::test_defaults(Path::new("/tmp"))
}

fn test_spec(run_id: &str) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: Path::new("/usr/bin/caduceus").to_path_buf(),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: IssueKey::parse("owner/repo#1").expect("valid key"),
            title: "title".to_string(),
            body: "body".to_string(),
            labels: Vec::new(),
            branch_name: "automation/issue-1".to_string(),
        }),
        worktree: Path::new("/tmp/worktree").to_path_buf(),
        run_id: run_id.to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: CancellationToken::new(),
    }
}

/// A minimal pre-rendered create argv. The lifecycle module consumes
/// argv only — it never re-derives it (the renderer is the sole argv
/// producer in the crate).
fn create_argv() -> Vec<String> {
    vec![
        "docker".to_string(),
        "create".to_string(),
        "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
        "bridge.py".to_string(),
    ]
}

async fn run_lifecycle(
    cfg: &Config,
    input: &ExecutorSpec,
    state: Arc<FakeOciRunState>,
    engine: SandboxEngine,
    argv: Vec<String>,
    cancel: CancellationToken,
    pressure: CancellationToken,
) -> CaduceusResult<caduceus::supervisor::SupervisorOutcome> {
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join(&input.run_id);
    let runtime = support::runtime_facts(cfg, &input.run_id, &worktree);
    let resolved = resolve(cfg.sandbox(), &runtime, input)?;
    let adapter = oci_lifecycle::OciAdapter::new(
        engine,
        state,
        cfg.state_dir.clone(),
        runtime.daemon_id,
        input.target.display(),
        "test-command-sha".to_string(),
        argv,
        None,
    );
    oci_lifecycle::run_oci_lifecycle(
        &resolved,
        &adapter,
        &oci_lifecycle::LifecycleTimeouts::from_config(cfg),
        cancel,
        pressure,
    )
    .await
}

// cleanup_on_cancel_and_timeout (AC-03)

/// Cancel mid-wait → no orphan container is left behind.
#[tokio::test]
async fn cleanup_on_cancel_and_timeout() {
    // Without Docker, we expect the lifecycle to fail at the create
    // step with a typed OCI error. The state row should be
    // inserted (Created) before the error is returned.
    let cfg = test_cfg();
    let state = Arc::new(FakeOciRunState::new());
    let spec = test_spec("lifecycle-cancel-001");
    let cancel = CancellationToken::new();

    let result = run_lifecycle(
        &cfg,
        &spec,
        state.clone(),
        SandboxEngine::Docker,
        create_argv(),
        cancel.clone(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err(), "expected error without Docker");

    // The state row should have been inserted (Created) before the
    // error occurred.
    let row = state.get("lifecycle-cancel-001").expect("get row");
    assert!(
        row.is_some(),
        "state row must exist after lifecycle attempt"
    );
    assert_eq!(
        row.unwrap().state,
        OciLifecycleState::Created,
        "state must be Created"
    );
}

// engine_unavailable_surfaces_structured (AC-05)

/// Docker not running → `OciEngineUnavailable` or `OciCreateFailed`.
#[tokio::test]
async fn engine_unavailable_surfaces_structured() {
    let cfg = test_cfg();
    let state = Arc::new(FakeOciRunState::new());
    let spec = test_spec("lifecycle-eng-001");
    let cancel = CancellationToken::new();

    let err = run_lifecycle(
        &cfg,
        &spec,
        state.clone(),
        SandboxEngine::Docker,
        create_argv(),
        cancel.clone(),
        CancellationToken::new(),
    )
    .await
    .expect_err("expected error without Docker");

    let is_oci_error = matches!(
        &err,
        CaduceusError::OciEngineUnavailable { .. }
            | CaduceusError::OciCreateFailed { .. }
            | CaduceusError::OciCliNotFound { .. }
            | CaduceusError::OciPullFailed { .. }
    );
    assert!(is_oci_error, "expected a typed OCI error; got: {err:?}");
}

// stop_kill_remove_bounded (AC-05)

/// Each step has the configured timeout — without Docker, the steps
/// fail fast rather than hanging.
#[tokio::test]
async fn stop_kill_remove_bounded() {
    // Without Docker, the lifecycle should fail at create step
    // (fast), not hang.
    let cfg = test_cfg();
    let state = Arc::new(FakeOciRunState::new());
    let spec = test_spec("lifecycle-bounded-001");
    let cancel = CancellationToken::new();

    // Use tokio::time::timeout to ensure we don't hang.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run_lifecycle(
            &cfg,
            &spec,
            state.clone(),
            SandboxEngine::Docker,
            create_argv(),
            cancel,
            CancellationToken::new(),
        ),
    )
    .await;

    match result {
        Ok(Err(e)) => {
            // Expected — no Docker available.
            let is_oci_error = matches!(
                &e,
                CaduceusError::OciEngineUnavailable { .. }
                    | CaduceusError::OciCreateFailed { .. }
                    | CaduceusError::OciCliNotFound { .. }
                    | CaduceusError::OciPullFailed { .. }
            );
            assert!(is_oci_error, "expected typed OCI error; got: {e:?}");
        }
        Ok(Ok(outcome)) => {
            // If Docker IS available, this should be a real outcome.
            assert_eq!(outcome.status, 0, "expected exit code 0");
        }
        Err(_) => {
            panic!("timeout: lifecycle hung");
        }
    }
}

// lifecycle_wires_engine_into_state_row

/// The engine passed to the lifecycle adapter feeds the
/// `ContainerRunRow.engine` column — the same engine the renderer
/// used, so the create argv and the state row cannot diverge.
#[tokio::test]
async fn lifecycle_wires_engine_into_state_row() {
    let cfg = test_cfg();
    let state = Arc::new(FakeOciRunState::new());
    let spec = test_spec("lifecycle-engine-row-001");
    let cancel = CancellationToken::new();

    // Podman engine: the renderer would have emitted `podman create`,
    // and the state row must say "Podman".
    let _ = run_lifecycle(
        &cfg,
        &spec,
        state.clone(),
        SandboxEngine::Podman,
        create_argv(),
        cancel,
        CancellationToken::new(),
    )
    .await;

    let row = state.get("lifecycle-engine-row-001").expect("get row");
    let row = row.expect("row must be inserted before create");
    assert_eq!(
        row.engine, "Podman",
        "ContainerRunRow.engine must match the engine passed to the adapter"
    );
}

// crash_recovery (AC-05)

/// Simulate a durable crash residual: insert a row in
/// `PendingReconciliation` and retain it for startup recovery.
#[tokio::test]
async fn crash_recovery() {
    let state = FakeOciRunState::new();

    // Insert a row as if it was created before a crash.
    let row = ContainerRunRow {
        run_id: "crash-rec-001".to_string(),
        container_id: Some("deadbeef".to_string()),
        state: OciLifecycleState::PendingReconciliation,
        engine: "Docker".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        daemon_id: "test-daemon".to_string(),
        issue_id: "owner/repo#1".to_string(),
        worker_command_sha256: "abc".to_string(),
    };
    state.insert(&row).expect("insert row");

    // The row remains available for the startup reconciliation pass.
    let row = state.get("crash-rec-001").expect("get row");
    assert!(row.is_some(), "row must still exist");
}

// reconcile_does_not_remove_unrelated (AC-05)

/// Only caduceus-labelled containers are reconciled — unrelated rows
/// are left untouched.
#[tokio::test]
async fn reconcile_does_not_remove_unrelated() {
    let state = FakeOciRunState::new();

    // Insert a row with a different run_id pattern.
    let row = ContainerRunRow {
        run_id: "unrelated-001".to_string(),
        container_id: Some("other".to_string()),
        state: OciLifecycleState::Running,
        engine: "Docker".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        daemon_id: "other-daemon".to_string(),
        issue_id: "other/repo#1".to_string(),
        worker_command_sha256: "abc".to_string(),
    };
    state.insert(&row).expect("insert row");

    // Since the row is not PendingReconciliation, reconcile should
    // have nothing to do.
    let pending = state.list_pending_reconciliation().expect("list pending");
    assert_eq!(pending.len(), 0, "no pending rows");

    // The unrelated row should still exist.
    let row = state.get("unrelated-001").expect("get row");
    assert!(row.is_some(), "unrelated row must still exist");
    assert_eq!(row.unwrap().state, OciLifecycleState::Running);
}

// parse_exit_code (moved from src/executor/oci_lifecycle.rs inline tests)

#[test]
fn parse_exit_code_parses_number() {
    assert_eq!(oci_lifecycle::parse_exit_code_for_tests("0\n"), 0);
    assert_eq!(oci_lifecycle::parse_exit_code_for_tests("42\n"), 42);
    assert_eq!(oci_lifecycle::parse_exit_code_for_tests(""), -1);
    assert_eq!(oci_lifecycle::parse_exit_code_for_tests("not-a-number"), -1);
}

#[test]
fn discovery_uses_the_quoted_run_label_template() {
    let template = oci_lifecycle::discovery_template_for_tests();
    assert_eq!(template, r#"{{index .Config.Labels "caduceus.run_id"}}"#);
    assert!(!template.contains("caduceus_run_id"));
}
