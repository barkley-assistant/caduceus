//! Lifecycle wait-cancellation and bounded diagnostic-capture tests
//! driven by a stub OCI engine (issue #245, tasks 11.1 / 11.3).
//!
//! The lifecycle invokes the engine binary by name (`docker`), so
//! these tests prepend a stub-script directory to `PATH` and drive
//! the canonical lifecycle end-to-end without a real container engine:
//!
//! - wait cancellation from the **watchdog token** falls through
//!   stop → capture → rm and reports disk pressure with no leaked
//!   container (state reaches `Removed`);
//! - wait cancellation from the **daemon shutdown token** reports a
//!   cancelled supervisor outcome after fresh-token cleanup confirms
//!   removal;
//! - over-cap engine logs are persisted bounded (≤ 1 MiB + marker)
//!   under `<state_dir>/oci-runs/<run_id>/engine.log` with a
//!   truncation marker.
//!
//! Every test mutates the process `PATH`, so all tests in this binary
//! are `#[serial]` (exclusive among themselves; other test binaries
//! run in separate processes with their own environment).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use serial_test::serial;
use tokio_util::sync::CancellationToken;

use caduceus::executor::oci_lifecycle;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine};
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusResult;
use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};

#[allow(dead_code)]
mod support;

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

    fn update_container_id(&self, run_id: &str, container_id: &str) -> CaduceusResult<()> {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|r| r.run_id == run_id)
        {
            row.container_id = Some(container_id.to_string());
        }
        Ok(())
    }

    fn list_by_daemon_id(&self, daemon_id: &str) -> CaduceusResult<Vec<ContainerRunRow>> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.daemon_id == daemon_id)
            .cloned()
            .collect())
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

async fn run_lifecycle(
    cfg: &Config,
    input: &ExecutorSpec,
    state: Arc<FakeOciRunState>,
    cancel: CancellationToken,
    pressure: CancellationToken,
) -> CaduceusResult<caduceus::supervisor::SupervisorOutcome> {
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join(&input.run_id);
    let facts = support::runtime_facts(cfg, &input.run_id, &worktree);
    let resolved = resolve(cfg.sandbox(), &facts, input).expect("sandbox resolves");
    let adapter = oci_lifecycle::OciAdapter::new(
        SandboxEngine::Docker,
        state,
        cfg.state_dir.clone(),
        facts.daemon_id,
        input.issue.clone(),
        input.issue.display_key(),
        "test-command-sha".to_string(),
        create_argv(),
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

#[tokio::test]
#[serial]
async fn canonical_lifecycle_runs_end_to_end() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let input = test_spec("stub-canonical-lifecycle");
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join(&input.run_id);
    let facts = support::runtime_facts(&cfg, &input.run_id, &worktree);
    let resolved = resolve(cfg.sandbox(), &facts, &input).expect("sandbox resolves");
    let adapter = oci_lifecycle::OciAdapter::new(
        SandboxEngine::Docker,
        state.clone(),
        cfg.state_dir.clone(),
        "test-daemon".to_string(),
        input.issue.clone(),
        input.issue.display_key(),
        "test-command-sha".to_string(),
        create_argv(),
        None,
    );

    let outcome = oci_lifecycle::run_oci_lifecycle(
        &resolved,
        &adapter,
        &oci_lifecycle::LifecycleTimeouts::from_config(&cfg),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("canonical lifecycle succeeds");
    assert_eq!(outcome.status, 0);
    assert!(stub.removed.exists());
    assert_eq!(
        state.get(&input.run_id).expect("get").expect("row").state,
        OciLifecycleState::Removed
    );
}

#[tokio::test]
#[serial]
async fn container_id_is_persisted_before_start_failure() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let _stub = StubEngine::new();
    std::env::set_var("STUB_START_FAIL", "1");
    let state = Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-start-failure");
    let error = run_lifecycle(
        &cfg,
        &spec,
        state.clone(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect_err("start failure must be surfaced");
    assert!(format!("{error:?}").contains("OciStartFailed"));
    assert_eq!(
        state
            .get("stub-start-failure")
            .expect("get")
            .expect("row")
            .container_id
            .as_deref(),
        Some("stub-container-000")
    );
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
  start) touch "$DIR/started"; if [ -n "$STUB_START_FAIL" ]; then exit 1; fi ;;
  wait)
    if [ -n "$STUB_WAIT_SLEEP" ]; then sleep "$STUB_WAIT_SLEEP"; fi
    echo 0 ;;
  ps) if [ -n "$STUB_PS_OUTPUT" ]; then printf "%s\n" "$STUB_PS_OUTPUT"; fi ;;
  stop) touch "$DIR/stopped"; if [ -n "$STUB_STOP_FAIL" ]; then exit 1; fi ;;
  kill) touch "$DIR/killed"; if [ -n "$STUB_KILL_FAIL" ]; then exit 1; fi ;;
  rm) touch "$DIR/removed"; if [ -n "$STUB_RM_FAIL" ]; then exit 1; fi ;;
  inspect)
    if [ -n "$STUB_INSPECT_PRESENT" ]; then echo "container still present"; else echo "container not found" >&2; exit 1; fi ;;
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
    killed: PathBuf,
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
            killed: dir.path().join("killed"),
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
        std::env::remove_var("STUB_STOP_FAIL");
        std::env::remove_var("STUB_START_FAIL");
        std::env::remove_var("STUB_KILL_FAIL");
        std::env::remove_var("STUB_RM_FAIL");
        std::env::remove_var("STUB_INSPECT_PRESENT");
        std::env::remove_var("STUB_PS_OUTPUT");
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

#[tokio::test]
#[serial]
async fn heartbeat_refreshes_during_oci_wait_and_stops_after_resolution() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-heartbeat-refresh");
    let watchdog = CancellationToken::new();

    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);
    let lifecycle = tokio::spawn({
        let cfg = cfg.clone();
        let state = std::sync::Arc::clone(&state);
        let watchdog = watchdog.clone();
        async move {
            let outcome =
                run_lifecycle(&cfg, &spec, state, CancellationToken::new(), watchdog).await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });

    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    let heartbeat = cfg.state_dir.join("runs/stub-heartbeat-refresh.heartbeat");
    let first = std::fs::read_to_string(&heartbeat).expect("initial heartbeat");
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    let refreshed = std::fs::read_to_string(&heartbeat).expect("refreshed heartbeat");
    assert_ne!(
        first, refreshed,
        "OCI heartbeat must refresh at the trusted cadence"
    );

    watchdog.cancel();
    lifecycle
        .await
        .expect("lifecycle task joins")
        .expect("cleanup completes");
    assert!(
        !heartbeat.exists(),
        "heartbeat must stop and be removed after resolution"
    );
}

#[tokio::test]
#[serial]
async fn teardown_escalates_and_requires_confirmed_absence() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    std::env::set_var("STUB_STOP_FAIL", "1");
    std::env::set_var("STUB_KILL_FAIL", "1");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-teardown-escalation");
    let watchdog = CancellationToken::new();
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);
    let lifecycle = tokio::spawn({
        let cfg = cfg.clone();
        let state = std::sync::Arc::clone(&state);
        let watchdog = watchdog.clone();
        async move {
            let outcome =
                run_lifecycle(&cfg, &spec, state, CancellationToken::new(), watchdog).await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });
    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    watchdog.cancel();
    lifecycle
        .await
        .expect("lifecycle task joins")
        .expect("cleanup completes");
    assert!(stub.stopped.exists());
    assert!(stub.killed.exists());
    assert!(stub.removed.exists());
    assert_eq!(
        state
            .get("stub-teardown-escalation")
            .expect("get")
            .expect("row")
            .state,
        OciLifecycleState::Removed
    );

    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    std::env::set_var("STUB_INSPECT_PRESENT", "1");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-teardown-pending");
    let watchdog = CancellationToken::new();
    let (result_tx, mut result_rx) = tokio::sync::mpsc::channel(1);
    let lifecycle = tokio::spawn({
        let cfg = cfg.clone();
        let state = std::sync::Arc::clone(&state);
        let watchdog = watchdog.clone();
        async move {
            let outcome =
                run_lifecycle(&cfg, &spec, state, CancellationToken::new(), watchdog).await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });
    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    watchdog.cancel();
    lifecycle
        .await
        .expect("lifecycle task joins")
        .expect("cleanup completes");
    assert_eq!(
        state
            .get("stub-teardown-pending")
            .expect("get")
            .expect("row")
            .state,
        OciLifecycleState::PendingReconciliation
    );
}

fn reconciliation_row(
    run_id: &str,
    container_id: Option<&str>,
    state: OciLifecycleState,
) -> ContainerRunRow {
    ContainerRunRow {
        run_id: run_id.to_string(),
        container_id: container_id.map(str::to_string),
        state,
        engine: "Docker".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        daemon_id: "test-daemon".to_string(),
        issue_id: "owner/repo#1".to_string(),
        worker_command_sha256: "test-command-sha".to_string(),
    }
}

#[tokio::test]
#[serial]
async fn startup_reconciliation_repairs_rows_and_removes_orphans() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let stub = StubEngine::new();
    let state = Arc::new(FakeOciRunState::new());

    // A labeled container without a row is an orphan and must use the
    // same stop -> kill -> rm path as an active lifecycle.
    std::env::set_var(
        "STUB_PS_OUTPUT",
        "orphan-container\ttest-daemon\torphan-run",
    );
    std::env::set_var("STUB_STOP_FAIL", "1");
    std::env::set_var("STUB_KILL_FAIL", "1");
    oci_lifecycle::reconcile_installation(
        &cfg,
        Arc::clone(&state) as Arc<dyn OciRunState>,
        "test-daemon",
        CancellationToken::new(),
    )
    .await
    .expect("orphan reconciliation");
    assert!(stub.stopped.exists(), "orphan stop must be attempted");
    assert!(stub.killed.exists(), "orphan stop must escalate to kill");
    assert!(stub.removed.exists(), "orphan must be force removed");

    // A restarted non-terminal row with no persisted ID adopts the ID
    // from the caduceus.run_id label before teardown.
    std::env::set_var(
        "STUB_PS_OUTPUT",
        "repair-container\ttest-daemon\trepair-run",
    );
    state
        .insert(&reconciliation_row(
            "repair-run",
            None,
            OciLifecycleState::Running,
        ))
        .expect("insert repair row");
    oci_lifecycle::reconcile_installation(
        &cfg,
        Arc::clone(&state) as Arc<dyn OciRunState>,
        "test-daemon",
        CancellationToken::new(),
    )
    .await
    .expect("row repair reconciliation");
    let repaired = state
        .get("repair-run")
        .expect("get repaired row")
        .expect("repaired row exists");
    assert_eq!(repaired.container_id.as_deref(), Some("repair-container"));
    assert_eq!(repaired.state, OciLifecycleState::Removed);
}

#[tokio::test]
#[serial]
async fn reconciliation_confirms_pending_absence_before_resolving() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let _stub = StubEngine::new();
    let state = Arc::new(FakeOciRunState::new());
    state
        .insert(&reconciliation_row(
            "pending-confirmation",
            Some("pending-container"),
            OciLifecycleState::PendingReconciliation,
        ))
        .expect("insert pending row");

    // The engine still reports the container: a failed rm must not be
    // interpreted as a confirmed absence, and the row remains pending.
    std::env::set_var("STUB_INSPECT_PRESENT", "1");
    oci_lifecycle::reconcile_installation(
        &cfg,
        Arc::clone(&state) as Arc<dyn OciRunState>,
        "test-daemon",
        CancellationToken::new(),
    )
    .await
    .expect("pending reconciliation");
    assert_eq!(
        state
            .get("pending-confirmation")
            .expect("get pending")
            .expect("pending row")
            .state,
        OciLifecycleState::PendingReconciliation
    );
}

#[tokio::test]
#[serial]
async fn label_round_trip_discovers_only_this_installation() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let _stub = StubEngine::new();
    let state = FakeOciRunState::new();
    std::env::set_var(
        "STUB_PS_OUTPUT",
        "own-container\ttest-daemon\town-run\nforeign-container\tother-daemon\tforeign-run\nunlabeled-container\t\tunlabeled-run",
    );

    let orphans = oci_lifecycle::find_orphans(&cfg, &state, "test-daemon")
        .await
        .expect("enumerate labeled containers");
    assert_eq!(orphans, vec!["own-container"]);
    assert_eq!(
        oci_lifecycle::daemon_discovery_template_for_tests(),
        r#"{{index .Config.Labels "caduceus.daemon_id"}}"#
    );
    assert_eq!(
        oci_lifecycle::discovery_template_for_tests(),
        r#"{{index .Config.Labels "caduceus.run_id"}}"#
    );
}

#[tokio::test]
#[serial]
async fn crash_window_matrix_leaves_reconciliation_safe_residuals() {
    let cfg = Config::test_defaults(Path::new("/tmp"));
    let _stub = StubEngine::new();
    let state = Arc::new(FakeOciRunState::new());
    let cases = [
        ("before-create", None, OciLifecycleState::Created, ""),
        (
            "after-create-before-update",
            None,
            OciLifecycleState::Created,
            "created-container\ttest-daemon\tafter-create-before-update",
        ),
        (
            "during-run",
            Some("running-container"),
            OciLifecycleState::Running,
            "running-container\ttest-daemon\tduring-run",
        ),
        (
            "during-teardown",
            Some("teardown-container"),
            OciLifecycleState::PendingReconciliation,
            "teardown-container\ttest-daemon\tduring-teardown",
        ),
        (
            "after-rm-before-removed",
            Some("removed-container"),
            OciLifecycleState::Running,
            "",
        ),
    ];

    for (run_id, container_id, lifecycle_state, ps_output) in cases {
        state
            .insert(&reconciliation_row(run_id, container_id, lifecycle_state))
            .expect("insert crash residual");
        std::env::set_var("STUB_PS_OUTPUT", ps_output);
        oci_lifecycle::reconcile_installation(
            &cfg,
            Arc::clone(&state) as Arc<dyn OciRunState>,
            "test-daemon",
            CancellationToken::new(),
        )
        .await
        .expect("reconcile crash residual");
        let repaired = state
            .get(run_id)
            .expect("get residual")
            .expect("residual row");
        assert_eq!(repaired.state, OciLifecycleState::Removed);
        if run_id == "after-create-before-update" {
            assert_eq!(repaired.container_id.as_deref(), Some("created-container"));
        }
    }
}

// ---------------------------------------------------------------------------
// Wait cancellation — both token sources through the same stop path
// ---------------------------------------------------------------------------

/// Watchdog-token cancellation mid-wait: the run falls through the
/// stop → capture → rm sequence (never an early return), reports
/// disk pressure, and reaches `Removed` — no container leak.
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
            let outcome = run_lifecycle(&cfg, &spec, state, shutdown, watchdog).await;
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
        .expect("fresh-token cleanup must complete");
    assert!(
        result.disk_pressure,
        "watchdog outcome must remain distinct"
    );

    // Cleanup ran: stop and rm markers exist, state is Removed, and
    // the bounded diagnostic capture was persisted.
    assert!(stub.stopped.exists(), "stop step must run");
    assert!(
        !stub.killed.exists(),
        "kill must be skipped after graceful stop"
    );
    assert!(stub.removed.exists(), "rm step must run after cancellation");
    let row = state
        .get("stub-watchdog-cancel")
        .expect("get")
        .expect("row");
    assert_eq!(row.state, OciLifecycleState::Removed);
    assert!(engine_log_path(&cfg, "stub-watchdog-cancel").exists());
}

/// Daemon-shutdown-token cancellation mid-wait: the run also falls
/// through to the cleanup sequence and reports `Cancelled`. Cleanup
/// runs under a fresh lifecycle token even though the parent token is
/// cancelled.
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
            let outcome = run_lifecycle(&cfg, &spec, state, shutdown, watchdog).await;
            let _ = result_tx.send(format!("{outcome:?}")).await;
            outcome
        }
    });

    wait_for_marker(&stub.started, Duration::from_secs(10), &mut result_rx).await;
    shutdown.cancel();

    let result = lifecycle
        .await
        .expect("lifecycle task joins")
        .expect("fresh-token cleanup must complete");
    assert!(result.cancelled, "shutdown outcome must remain cancelled");

    // The wait cancellation did not early-return: stop and rm ran on
    // the fresh lifecycle token and removal was confirmed.
    let row = state
        .get("stub-shutdown-cancel")
        .expect("get")
        .expect("row");
    assert_eq!(row.state, OciLifecycleState::Removed);
    assert!(
        stub.removed.exists(),
        "rm must run despite parent cancellation"
    );
}

#[tokio::test]
#[serial]
async fn worker_timeout_reports_timed_out_after_cleanup() {
    let mut cfg = Config::test_defaults(Path::new("/tmp"));
    cfg.worker_timeout_seconds = 1;
    let stub = StubEngine::new();
    std::env::set_var("STUB_WAIT_SLEEP", "10");
    let state = std::sync::Arc::new(FakeOciRunState::new());
    let spec = test_spec("stub-worker-timeout");
    let result = run_lifecycle(
        &cfg,
        &spec,
        state.clone(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("timeout cleanup must complete");
    assert!(result.timed_out);
    assert!(stub.removed.exists());
    assert_eq!(
        state
            .get("stub-worker-timeout")
            .expect("get")
            .expect("row")
            .state,
        OciLifecycleState::Removed
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

    let outcome = run_lifecycle(
        &cfg,
        &spec,
        Arc::new(state),
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
