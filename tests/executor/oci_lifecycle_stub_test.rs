//! Lifecycle wait-cancellation and bounded diagnostic-capture tests
//! driven by a stub OCI engine (issue #245, tasks 11.1 / 11.3).
//!
//! The lifecycle invokes the engine binary by name (`docker`), so
//! these tests prepend a stub-script directory to `PATH` and drive
//! `run_with_argv` end-to-end without a real container engine:
//!
//! - wait cancellation from the **watchdog token** falls through
//!   stop → capture → rm and reports `Cancelled` with no leaked
//!   container (state reaches `Removed`);
//! - wait cancellation from the **daemon shutdown token** reports
//!   `Cancelled`; the cleanup steps check only the daemon token
//!   (which is itself cancelled), so the state reaches `Killed` and
//!   the reconciliation pass finishes the `rm` — the pre-existing
//!   daemon-shutdown stop-path contract;
//! - over-cap engine logs are persisted bounded (≤ 1 MiB + marker)
//!   under `<state_dir>/oci-runs/<run_id>/engine.log` with a
//!   truncation marker.
//!
//! Every test mutates the process `PATH`, so all tests in this binary
//! are `#[serial]` (exclusive among themselves; other test binaries
//! run in separate processes with their own environment).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serial_test::serial;
use tokio_util::sync::CancellationToken;

use caduceus::executor::oci_lifecycle;
use caduceus::executor::sandbox_spec::SandboxEngine;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::{CaduceusError, CaduceusResult};
use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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
        self.rows.lock().unwrap().push(row.clone());
        Ok(())
    }

    fn update_state(&self, run_id: &str, state: &OciLifecycleState) -> CaduceusResult<()> {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|r| r.run_id == run_id)
        {
            row.state = state.clone();
        }
        Ok(())
    }

    fn list_pending_reconciliation(&self) -> CaduceusResult<Vec<ContainerRunRow>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.state == OciLifecycleState::PendingReconciliation)
            .cloned()
            .collect())
    }

    fn get(&self, run_id: &str) -> CaduceusResult<Option<ContainerRunRow>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned())
    }

    fn delete(&self, run_id: &str) -> CaduceusResult<()> {
        self.rows.lock().unwrap().retain(|r| r.run_id != run_id);
        Ok(())
    }
}

fn test_spec(run_id: &str) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: Path::new("/usr/bin/caduceus").to_path_buf(),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worktree: Path::new("/tmp/worktree").to_path_buf(),
        run_id: run_id.to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: CancellationToken::new(),
        issue_title: "title".to_string(),
        issue_body: "body".to_string(),
        labels: Vec::new(),
        branch_name: "automation/issue-1".to_string(),
    }
}

fn create_argv() -> Vec<String> {
    vec![
        "docker".to_string(),
        "create".to_string(),
        "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
        "bridge.py".to_string(),
    ]
}

/// Stub engine script. Behavior is env-tunable:
/// - `STUB_WAIT_SLEEP`: seconds `wait` sleeps before printing `0`
///   (lets the test cancel mid-wait);
/// - `STUB_LOG_BYTES`: byte count `logs` emits (over-feeds the
///   diagnostic capture when large).
const STUB_SCRIPT: &str = r#"#!/bin/sh
DIR="$(dirname "$0")"
case "$1" in
  create) echo "stub-container-000" ;;
  start) touch "$DIR/started" ;;
  wait)
    if [ -n "$STUB_WAIT_SLEEP" ]; then sleep "$STUB_WAIT_SLEEP"; fi
    echo 0 ;;
  stop) touch "$DIR/stopped" ;;
  kill) touch "$DIR/killed" ;;
  rm) touch "$DIR/removed" ;;
  logs)
    if [ -n "$STUB_LOG_BYTES" ]; then
      head -c "$STUB_LOG_BYTES" /dev/zero | tr '\0' 'x'
    else
      echo "stub engine log line"
    fi ;;
  *) exit 0 ;;
esac
"#;

/// Prepends a stub-engine directory to `PATH` and restores the
/// original on drop. Also clears the stub's tuning env vars on drop.
struct StubEngine {
    _dir: tempfile::TempDir,
    started: PathBuf,
    stopped: PathBuf,
    removed: PathBuf,
}

impl StubEngine {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("stub tempdir");
        let script = dir.path().join("docker");
        std::fs::write(&script, STUB_SCRIPT).expect("write stub script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub script");
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", dir.path().display()));
        Self {
            started: dir.path().join("started"),
            stopped: dir.path().join("stopped"),
            removed: dir.path().join("removed"),
            _dir: dir,
        }
    }
}

impl Drop for StubEngine {
    fn drop(&mut self) {
        // Rebuild PATH without the stub dir (best-effort restore).
        // The tempdir removal alone already breaks the stub, but a
        // clean PATH keeps later serial tests honest.
        if let Ok(path) = std::env::var("PATH") {
            let stub_dir = self._dir.path().to_string_lossy().to_string();
            let filtered: Vec<&str> = path.split(':').filter(|p| *p != stub_dir).collect();
            std::env::set_var("PATH", filtered.join(":"));
        }
        std::env::remove_var("STUB_WAIT_SLEEP");
        std::env::remove_var("STUB_LOG_BYTES");
    }
}

/// Poll for a marker file with a bounded budget (the stub creates
/// markers synchronously, so a couple of seconds is generous). If the
/// lifecycle finishes before the marker appears, panic with its result
/// — that is a real failure, not a timing miss.
async fn wait_for_marker(
    marker: &Path,
    budget: Duration,
    lifecycle_result: &mut tokio::sync::mpsc::Receiver<String>,
) {
    let started = std::time::Instant::now();
    loop {
        if marker.exists() {
            return;
        }
        if let Ok(result) = lifecycle_result.try_recv() {
            panic!("lifecycle finished before the start marker appeared: {result}");
        }
        assert!(
            started.elapsed() < budget,
            "stub engine never reached the start step"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn engine_log_path(cfg: &Config, run_id: &str) -> PathBuf {
    cfg.state_dir
        .join("oci-runs")
        .join(run_id)
        .join("engine.log")
}

// ---------------------------------------------------------------------------
// Wait cancellation — both token sources through the same stop path
// ---------------------------------------------------------------------------

/// Watchdog-token cancellation mid-wait: the run falls through the
/// stop → capture → rm sequence (never an early return), reports
/// `Cancelled`, and reaches `Removed` — no container leak.
#[tokio::test]
#[serial]
async fn watchdog_cancellation_proceeds_through_stop_capture_rm() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-watchdog-cancel");
    let shutdown = CancellationToken::new();
    let watchdog = CancellationToken::new();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);
    let lifecycle = tokio::spawn({
        let cfg = cfg.clone();
        let state = std::sync::Arc::clone(&state);
        let shutdown = shutdown.clone();
        let watchdog = watchdog.clone();
        async move {
            let outcome = oci_lifecycle::run_with_argv(
                &cfg,
                &spec,
                &*state,
                SandboxEngine::Docker,
                create_argv(),
                shutdown,
                watchdog,
            )
            .await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });

    // Deterministic cancel point: the stub's start step creates the
    // marker right before the wait step begins.
    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    watchdog.cancel();

    let result = lifecycle
        .await
        .expect("lifecycle task joins")
        .expect_err("watchdog cancellation must report Cancelled");
    assert!(
        matches!(result, CaduceusError::Cancelled),
        "expected Cancelled; got: {result:?}"
    );

    // Cleanup ran: stop and rm markers exist, state is Removed, and
    // the bounded diagnostic capture was persisted.
    assert!(stub.stopped.exists(), "stop step must run");
    assert!(stub.removed.exists(), "rm step must run after cancellation");
    let row = state
        .get("stub-watchdog-cancel")
        .expect("get")
        .expect("row");
    assert_eq!(row.state, OciLifecycleState::Removed);
    assert!(engine_log_path(&cfg, "stub-watchdog-cancel").exists());
}

/// Daemon-shutdown-token cancellation mid-wait: the run also falls
/// through to the cleanup sequence and reports `Cancelled`. The
/// cleanup steps check ONLY the daemon token — which is itself
/// cancelled — so the state lands in `Killed` and the reconciliation
/// pass finishes the `rm` (the pre-existing daemon-shutdown stop-path
/// contract; no early return, no hang).
#[tokio::test]
#[serial]
async fn shutdown_cancellation_falls_through_cleanup_and_reports_cancelled() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-shutdown-cancel");
    let shutdown = CancellationToken::new();
    let watchdog = CancellationToken::new();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);
    let lifecycle = tokio::spawn({
        let cfg = cfg.clone();
        let state = std::sync::Arc::clone(&state);
        let shutdown = shutdown.clone();
        async move {
            let outcome = oci_lifecycle::run_with_argv(
                &cfg,
                &spec,
                &*state,
                SandboxEngine::Docker,
                create_argv(),
                shutdown,
                watchdog,
            )
            .await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });

    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    shutdown.cancel();

    let result = lifecycle
        .await
        .expect("lifecycle task joins")
        .expect_err("shutdown cancellation must report Cancelled");
    assert!(
        matches!(result, CaduceusError::Cancelled),
        "expected Cancelled; got: {result:?}"
    );

    // The wait cancellation did not early-return: the stop step was
    // reached (its pre-spawn check refused to spawn because the daemon
    // token is cancelled) and the kill fallback recorded Killed.
    let row = state
        .get("stub-shutdown-cancel")
        .expect("get")
        .expect("row");
    assert_eq!(row.state, OciLifecycleState::Killed);
    assert!(
        !stub.removed.exists(),
        "rm is left to reconciliation when the daemon token is cancelled"
    );
}

// ---------------------------------------------------------------------------
// Bounded diagnostic capture (task 11.3)
// ---------------------------------------------------------------------------

/// A container emitting > 1 MiB of engine logs yields a persisted
/// diagnostic that (a) exists under
/// `<state_dir>/oci-runs/<run_id>/engine.log`, (b) is bounded at the
/// 1 MiB cap (plus the small truncation marker), and (c) carries the
/// truncation marker when over-fed.
#[tokio::test]
#[serial]
async fn diagnostic_capture_is_bounded_with_truncation_marker() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let _stub = StubEngine::new();
    std::env::set_var("STUB_LOG_BYTES", "2097152"); // 2 MiB > 1 MiB cap
    let state = FakeOciRunState::new();
    let spec = test_spec("stub-capture-bounded");

    let outcome = oci_lifecycle::run_with_argv(
        &cfg,
        &spec,
        &state,
        SandboxEngine::Docker,
        create_argv(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("stub lifecycle completes");

    assert_eq!(outcome.status, 0, "stub wait prints 0");

    let path = engine_log_path(&cfg, "stub-capture-bounded");
    assert!(path.exists(), "engine.log must be persisted");
    let meta = std::fs::metadata(&path).expect("stat engine.log");
    // Hard cap: the writer's first-N semantics plus the marker stay
    // within the cap plus a small marker allowance.
    assert!(
        meta.len() <= oci_lifecycle::OCI_DIAGNOSTIC_MAX_BYTES + 128,
        "engine.log must be bounded at the 1 MiB cap, got {} bytes",
        meta.len()
    );
    let body = std::fs::read_to_string(&path).expect("read engine.log");
    assert!(
        body.contains("...<truncated"),
        "over-fed capture must carry the truncation marker"
    );
}
