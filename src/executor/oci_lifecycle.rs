//! Crash-safe OCI lifecycle and startup reconciliation.
//!
//! This module intentionally has one orchestration core.  The executor and
//! reconciliation code only prepare an [`OciAdapter`] and call that core;
//! argv construction, teardown bounds, removal confirmation, and heartbeat
//! ownership therefore cannot drift between the two paths.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::executor::oci_env_file::OciEnvFile;
use crate::executor::sandbox_renderer::{
    OCI_DAEMON_ID_DISCOVERY_TEMPLATE, OCI_DAEMON_LABEL, OCI_RUN_ID_DISCOVERY_TEMPLATE,
    OCI_RUN_ID_PS_TEMPLATE, OCI_RUN_LABEL,
};
use crate::executor::sandbox_spec::{SandboxEngine, SandboxSpec};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};
use crate::worker::supervisor::{
    clear_heartbeat, write_heartbeat_record, BoundedTranscriptWriter, Heartbeat, SupervisorOutcome,
    WorkerRunPaths, HEARTBEAT_FILE_VERSION,
};

/// Maximum time spent on one engine command during teardown.
pub const OCI_DIAGNOSTIC_MAX_BYTES: u64 = 1024 * 1024;

/// Timeout values used by the single lifecycle path.
#[derive(Clone, Debug)]
pub struct LifecycleTimeouts {
    pub worker_timeout: Duration,
    pub stop_grace: Duration,
    pub kill_timeout: Duration,
}

impl LifecycleTimeouts {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            worker_timeout: Duration::from_secs(cfg.worker_timeout_seconds),
            stop_grace: Duration::from_secs(cfg.sandbox().stop_timeout_seconds),
            kill_timeout: Duration::from_secs(cfg.sandbox().kill_timeout_seconds),
        }
    }
}

/// The only adapter used by lifecycle and reconciliation.  It owns the
/// engine binary, all lifecycle argv builders, the durable state handle, and
/// the one create argv rendered by the sandbox adapter.
pub struct OciAdapter {
    engine: SandboxEngine,
    binary: PathBuf,
    state: Option<Arc<dyn OciRunState>>,
    state_dir: PathBuf,
    daemon_id: String,
    issue_id: String,
    issue: crate::github::issue::IssueKey,
    worker_command_sha256: String,
    create_argv: Vec<String>,
    env_file: Mutex<Option<OciEnvFile>>,
}

impl std::fmt::Debug for OciAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OciAdapter")
            .field("engine", &self.engine)
            .field("binary", &self.binary)
            .field("state_dir", &self.state_dir)
            .field("daemon_id", &self.daemon_id)
            .field("issue_id", &self.issue_id)
            .finish()
    }
}

impl OciAdapter {
    /// Construct the adapter for a real executor run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: SandboxEngine,
        state: Arc<dyn OciRunState>,
        state_dir: PathBuf,
        daemon_id: String,
        issue: crate::github::issue::IssueKey,
        issue_id: String,
        worker_command_sha256: String,
        create_argv: Vec<String>,
        env_file: Option<OciEnvFile>,
    ) -> Self {
        let binary = create_argv
            .first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(engine.binary_name()));
        Self {
            engine,
            binary,
            state: Some(state),
            state_dir,
            daemon_id,
            issue,
            issue_id,
            worker_command_sha256,
            create_argv,
            env_file: Mutex::new(env_file),
        }
    }

    fn for_reconciliation(cfg: &Config, state: Arc<dyn OciRunState>, daemon_id: &str) -> Self {
        Self {
            engine: cfg.sandbox().engine,
            binary: PathBuf::from(cfg.sandbox().engine.binary_name()),
            state: Some(state),
            state_dir: cfg.state_dir.clone(),
            daemon_id: daemon_id.to_string(),
            issue: crate::github::issue::IssueKey {
                owner: String::new(),
                repo: String::new(),
                number: 0,
            },
            issue_id: String::new(),
            worker_command_sha256: String::new(),
            create_argv: Vec::new(),
            env_file: Mutex::new(None),
        }
    }

    fn for_discovery(cfg: &Config, daemon_id: &str) -> Self {
        Self {
            engine: cfg.sandbox().engine,
            binary: PathBuf::from(cfg.sandbox().engine.binary_name()),
            state: None,
            state_dir: cfg.state_dir.clone(),
            daemon_id: daemon_id.to_string(),
            issue: crate::github::issue::IssueKey {
                owner: String::new(),
                repo: String::new(),
                number: 0,
            },
            issue_id: String::new(),
            worker_command_sha256: String::new(),
            create_argv: Vec::new(),
            env_file: Mutex::new(None),
        }
    }

    pub fn engine(&self) -> SandboxEngine {
        self.engine
    }

    pub fn state(&self) -> Option<&Arc<dyn OciRunState>> {
        self.state.as_ref()
    }

    pub fn argv_create(&self) -> Vec<String> {
        self.create_argv.clone()
    }

    pub fn argv_start(&self, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "start".to_string(),
            container_id.to_string(),
        ]
    }

    pub fn argv_wait(&self, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "wait".to_string(),
            container_id.to_string(),
        ]
    }

    pub fn argv_stop(&self, grace: Duration, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "stop".to_string(),
            "--time".to_string(),
            grace.as_secs().to_string(),
            container_id.to_string(),
        ]
    }

    pub fn argv_kill(&self, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "kill".to_string(),
            container_id.to_string(),
        ]
    }

    pub fn argv_rm_force(&self, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "rm".to_string(),
            "--force".to_string(),
            container_id.to_string(),
        ]
    }

    pub fn argv_inspect(&self, container_id: &str) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            OCI_RUN_ID_DISCOVERY_TEMPLATE.to_string(),
            container_id.to_string(),
        ]
    }

    fn take_env_file(&self) {
        let _ = self.env_file.lock().expect("OCI env-file mutex").take();
    }
}

/// Run one OCI container through the sole crash-safe lifecycle.
pub async fn run_oci_lifecycle(
    spec: &SandboxSpec,
    adapter: &OciAdapter,
    cfg: &LifecycleTimeouts,
    cancel: CancellationToken,
    pressure: CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let state = adapter
        .state
        .as_ref()
        .ok_or_else(|| CaduceusError::Other("OCI lifecycle adapter has no state".to_string()))?;
    let input = RunInput {
        run_id: spec.name().to_string(),
        issue_id: adapter.issue_id.clone(),
        issue: adapter.issue.clone(),
        worker_command_sha256: adapter.worker_command_sha256.clone(),
    };
    run_lifecycle_core(&input, state.as_ref(), adapter, cfg, cancel, pressure).await
}

struct RunInput {
    run_id: String,
    issue_id: String,
    issue: crate::github::issue::IssueKey,
    worker_command_sha256: String,
}

async fn run_lifecycle_core(
    input: &RunInput,
    state: &dyn OciRunState,
    adapter: &OciAdapter,
    cfg: &LifecycleTimeouts,
    cancel: CancellationToken,
    pressure: CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let now = chrono::Utc::now().to_rfc3339();
    state.insert(&ContainerRunRow {
        run_id: input.run_id.clone(),
        container_id: None,
        state: OciLifecycleState::Created,
        engine: format!("{:?}", adapter.engine),
        created_at: now.clone(),
        updated_at: now,
        daemon_id: adapter.daemon_id.clone(),
        issue_id: input.issue_id.clone(),
        worker_command_sha256: input.worker_command_sha256.clone(),
    })?;

    if cancel.is_cancelled() || pressure.is_cancelled() {
        return Err(CaduceusError::Cancelled);
    }
    let created = bounded_command(&adapter.argv_create(), cfg.kill_timeout, "create").await;
    // The env file contains the complete worker environment and must disappear
    // immediately after create, regardless of create's result.
    adapter.take_env_file();
    let container_id = created?;
    state.update_container_id(&input.run_id, &container_id)?;

    let start = adapter.argv_start(&container_id);
    if let Err(err) = bounded_command(&start, cfg.kill_timeout, "start").await {
        let _ = teardown_container(input, state, adapter, &container_id, cfg, false).await;
        return Err(err);
    }
    state.update_state(&input.run_id, &OciLifecycleState::Running)?;

    let paths = WorkerRunPaths::new(adapter.state_dir.clone(), input.run_id.clone());
    paths.ensure_dirs()?;
    let started_at = chrono::Utc::now();
    write_heartbeat_record(
        &Heartbeat {
            version: HEARTBEAT_FILE_VERSION,
            run_id: input.run_id.clone(),
            pid: std::process::id(),
            started_at,
            updated_at: started_at,
            target: input.issue.display_key(),
            transcript_path: paths.transcript_path.clone(),
        },
        &paths.heartbeat_path,
    )?;
    let heartbeat_cancel = CancellationToken::new();
    let heartbeat_task =
        spawn_heartbeat(paths.clone(), input, started_at, heartbeat_cancel.clone());

    let wait_argv = adapter.argv_wait(&container_id);
    enum Race {
        Exit(i32),
        Pressure,
        Cancelled,
        TimedOut,
        Failed(CaduceusError),
    }
    let race = tokio::select! {
        biased;
        output = command_output(&wait_argv, "wait") => {
            match output {
                Ok(value) => Race::Exit(parse_exit_code(&value)),
                Err(err) => Race::Failed(err),
            }
        }
        _ = pressure.cancelled() => Race::Pressure,
        _ = cancel.cancelled() => Race::Cancelled,
        _ = tokio::time::sleep(cfg.worker_timeout) => Race::TimedOut,
    };
    heartbeat_cancel.cancel();
    let _ = heartbeat_task.await;
    let _ = clear_heartbeat(&paths.heartbeat_path);

    let (outcome, exited) = match race {
        Race::Exit(code) => {
            state.update_state(&input.run_id, &OciLifecycleState::Exited(code))?;
            (
                SupervisorOutcome {
                    status: code,
                    signaled: false,
                    timed_out: false,
                    cancelled: false,
                    disk_pressure: false,
                },
                true,
            )
        }
        Race::Pressure => (
            SupervisorOutcome {
                status: 125,
                signaled: true,
                timed_out: false,
                cancelled: false,
                disk_pressure: true,
            },
            false,
        ),
        Race::Cancelled => (
            SupervisorOutcome {
                status: 130,
                signaled: true,
                timed_out: false,
                cancelled: true,
                disk_pressure: false,
            },
            false,
        ),
        Race::TimedOut => (
            SupervisorOutcome {
                status: 124,
                signaled: true,
                timed_out: true,
                cancelled: false,
                disk_pressure: false,
            },
            false,
        ),
        Race::Failed(err) => {
            let removed =
                teardown_container(input, state, adapter, &container_id, cfg, false).await;
            if !removed {
                tracing::warn!(run_id = %input.run_id, "OCI wait failed and removal is pending reconciliation");
            }
            return Err(err);
        }
    };

    // Capture diagnostics while the container still exists, before rm.
    // This is best effort and cannot change the supervisor outcome.
    capture_engine_logs(
        adapter,
        &container_id,
        &adapter.state_dir.join("oci-runs").join(&input.run_id),
    )
    .await;

    let _ = teardown_container(input, state, adapter, &container_id, cfg, exited).await;
    Ok(outcome)
}

async fn capture_engine_logs(adapter: &OciAdapter, container_id: &str, run_dir: &std::path::Path) {
    let path = run_dir.join("engine.log");
    let Some(parent) = path.parent() else { return };
    let _ = std::fs::create_dir_all(parent);
    let mut writer = match BoundedTranscriptWriter::new(path.clone(), OCI_DIAGNOSTIC_MAX_BYTES) {
        Ok(writer) => writer,
        Err(err) => {
            tracing::warn!(error = %err, "OCI diagnostic capture setup failed");
            return;
        }
    };
    let result = async {
        let mut child = Command::new(&adapter.binary)
            .args(["logs", container_id])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| CaduceusError::Other(format!("engine logs spawn: {err}")))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let mut out = vec![0_u8; 8 * 1024];
        let mut err = vec![0_u8; 8 * 1024];
        let (mut stdout_open, mut stderr_open) = (true, true);
        while stdout_open || stderr_open {
            tokio::select! {
                read = stdout.read(&mut out), if stdout_open => match read {
                    Ok(0) | Err(_) => stdout_open = false,
                    Ok(n) => writer.write_bytes(&out[..n]),
                },
                read = stderr.read(&mut err), if stderr_open => match read {
                    Ok(0) | Err(_) => stderr_open = false,
                    Ok(n) => writer.write_bytes(&err[..n]),
                },
            }
        }
        let _ = child.wait().await;
        let _ = crate::worker::supervisor::truncate_transcript(&path, OCI_DIAGNOSTIC_MAX_BYTES);
        writer.finalize()
    }
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "OCI diagnostic capture failed");
    }
}

fn spawn_heartbeat(
    paths: WorkerRunPaths,
    input: &RunInput,
    started_at: chrono::DateTime<chrono::Utc>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let run_id = input.run_id.clone();
    let target = input.issue.display_key();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    let now = chrono::Utc::now();
                    let record = Heartbeat {
                        version: HEARTBEAT_FILE_VERSION,
                        run_id: run_id.clone(),
                        pid: std::process::id(),
                        started_at,
                        updated_at: now,
                        target: target.clone(),
                        transcript_path: paths.transcript_path.clone(),
                    };
                    if write_heartbeat_record(&record, &paths.heartbeat_path).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Shared bounded stop → kill → rm → inspect teardown.  The local token is
/// intentionally fresh and is never a child of the parent run token.
async fn teardown_container(
    input: &RunInput,
    state: &dyn OciRunState,
    adapter: &OciAdapter,
    container_id: &str,
    cfg: &LifecycleTimeouts,
    already_exited: bool,
) -> bool {
    let _fresh_lifecycle_token = CancellationToken::new();
    let stop = bounded_command(
        &adapter.argv_stop(cfg.stop_grace, container_id),
        cfg.stop_grace,
        "stop",
    )
    .await;
    if stop.is_ok() {
        let _ = state.update_state(&input.run_id, &OciLifecycleState::Stopped);
    } else if !already_exited
        && bounded_command(&adapter.argv_kill(container_id), cfg.kill_timeout, "kill")
            .await
            .is_ok()
    {
        let _ = state.update_state(&input.run_id, &OciLifecycleState::Killed);
    }

    let _ = bounded_command(&adapter.argv_rm_force(container_id), cfg.kill_timeout, "rm").await;
    let absent = confirm_absent(adapter, container_id, cfg.kill_timeout).await;
    let _ = state.update_state(
        &input.run_id,
        if absent {
            &OciLifecycleState::Removed
        } else {
            &OciLifecycleState::PendingReconciliation
        },
    );
    absent
}

async fn confirm_absent(adapter: &OciAdapter, container_id: &str, timeout: Duration) -> bool {
    match bounded_command_with_status(&adapter.argv_inspect(container_id), timeout, "inspect").await
    {
        Ok(_) => false,
        Err((_, stderr)) => {
            let lower = stderr.to_ascii_lowercase();
            lower.contains("no such")
                || lower.contains("not found")
                || lower.contains("does not exist")
        }
    }
}

async fn bounded_command(
    argv: &[String],
    limit: Duration,
    step: &'static str,
) -> CaduceusResult<String> {
    match bounded_command_with_status(argv, limit, step).await {
        Ok(stdout) => Ok(stdout),
        Err((error, _stderr)) => Err(error),
    }
}

/// Wait is deliberately not wrapped in a second timeout: its timeout is the
/// explicit timer arm in the biased race above. Dropping the future on a
/// cancellation/pressure win still leaves teardown responsible for stopping
/// the engine container.
async fn command_output(argv: &[String], step: &'static str) -> CaduceusResult<String> {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).kill_on_drop(true);
    match command.output().await {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        Ok(output) => Err(to_oci_error(step, &String::from_utf8_lossy(&output.stderr))),
        Err(err) => Err(to_oci_error(step, &err.to_string())),
    }
}

async fn bounded_command_with_status(
    argv: &[String],
    limit: Duration,
    step: &'static str,
) -> Result<String, (CaduceusError, String)> {
    if argv.is_empty() {
        return Err((
            to_oci_error(step, "empty OCI argv"),
            "empty OCI argv".to_string(),
        ));
    }
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).kill_on_drop(true);
    let result = tokio::time::timeout(limit, command.output()).await;
    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            let detail = err.to_string();
            return Err((to_oci_error(step, &detail), detail));
        }
        Err(_) => {
            let detail = format!("{step} command timed out");
            return Err((to_oci_error(step, &detail), detail));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout.trim().to_string())
    } else {
        Err((to_oci_error(step, &stderr), stderr))
    }
}

fn to_oci_error(step: &'static str, detail: &str) -> CaduceusError {
    match step {
        "create" => CaduceusError::OciCreateFailed {
            context: step,
            stderr: detail.to_string(),
        },
        "start" => CaduceusError::OciStartFailed {
            context: step,
            stderr: detail.to_string(),
        },
        "wait" => CaduceusError::OciWaitFailed {
            context: step,
            stderr: detail.to_string(),
        },
        "stop" => CaduceusError::OciStopFailed {
            context: step,
            stderr: detail.to_string(),
        },
        "rm" => CaduceusError::OciRemoveFailed {
            context: step,
            stderr: detail.to_string(),
        },
        _ => CaduceusError::OciEngineUnavailable {
            detail: detail.to_string(),
        },
    }
}

/// Reconcile this installation's containers and unresolved rows at startup.
pub async fn reconcile_installation(
    cfg: &Config,
    state: Arc<dyn OciRunState>,
    daemon_id: &str,
    cancellation: CancellationToken,
) -> CaduceusResult<()> {
    let adapter = OciAdapter::for_reconciliation(cfg, Arc::clone(&state), daemon_id);
    let rows = state.list_by_daemon_id(daemon_id)?;
    let containers = list_labeled_containers(&adapter, daemon_id).await?;
    let mut seen_runs = std::collections::HashSet::new();

    for (container_id, run_id) in containers {
        if cancellation.is_cancelled() {
            break;
        }
        seen_runs.insert(run_id.clone());
        let row = rows.iter().find(|row| row.run_id == run_id).cloned();
        match row {
            Some(row) if !row.state.is_terminal() => {
                if row.container_id.as_deref() != Some(container_id.as_str()) {
                    let _ = state.update_container_id(&row.run_id, &container_id);
                }
                let input = RunInput {
                    run_id: row.run_id.clone(),
                    issue_id: row.issue_id.clone(),
                    issue: parse_issue_key(&row.issue_id),
                    worker_command_sha256: row.worker_command_sha256.clone(),
                };
                let _ = teardown_container(
                    &input,
                    state.as_ref(),
                    &adapter,
                    &container_id,
                    &LifecycleTimeouts::from_config(cfg),
                    false,
                )
                .await;
            }
            Some(row) => {
                let input = RunInput {
                    run_id: row.run_id.clone(),
                    issue_id: row.issue_id.clone(),
                    issue: parse_issue_key(&row.issue_id),
                    worker_command_sha256: row.worker_command_sha256.clone(),
                };
                let _ = teardown_container(
                    &input,
                    state.as_ref(),
                    &adapter,
                    &container_id,
                    &LifecycleTimeouts::from_config(cfg),
                    false,
                )
                .await;
            }
            None => {
                let input = RunInput {
                    run_id,
                    issue_id: String::new(),
                    issue: adapter.issue.clone(),
                    worker_command_sha256: String::new(),
                };
                let _ = teardown_container(
                    &input,
                    state.as_ref(),
                    &adapter,
                    &container_id,
                    &LifecycleTimeouts::from_config(cfg),
                    false,
                )
                .await;
            }
        }
    }

    // A non-terminal row whose labeled container is gone is safe to resolve
    // only after inspect confirms absence.  No rm result is assumed.
    for row in rows {
        if row.state == OciLifecycleState::Removed || seen_runs.contains(&row.run_id) {
            continue;
        }
        let absent = match row.container_id.as_deref() {
            Some(cid) => {
                confirm_absent(
                    &adapter,
                    cid,
                    Duration::from_secs(cfg.sandbox().reconcile_timeout_seconds),
                )
                .await
            }
            None if row.state != OciLifecycleState::PendingReconciliation => true,
            None => false,
        };
        if absent {
            let _ = state.update_state(&row.run_id, &OciLifecycleState::Removed);
        } else if row.state != OciLifecycleState::PendingReconciliation {
            let _ = state.update_state(&row.run_id, &OciLifecycleState::PendingReconciliation);
        }
    }
    Ok(())
}

async fn list_labeled_containers(
    adapter: &OciAdapter,
    daemon_id: &str,
) -> CaduceusResult<Vec<(String, String)>> {
    let ps = vec![
        adapter.binary.display().to_string(),
        "ps".to_string(),
        "-a".to_string(),
        "--filter".to_string(),
        format!("label={OCI_DAEMON_LABEL}={daemon_id}"),
        "--format".to_string(),
        format!("{{{{.ID}}}}\t{OCI_DAEMON_ID_DISCOVERY_TEMPLATE}\t{OCI_RUN_ID_PS_TEMPLATE}"),
    ];
    let ids = bounded_command(&ps, Duration::from_secs(30), "ps").await?;
    let mut result = Vec::new();
    for line in ids.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, '\t');
        let Some(container_id) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(found_daemon_id) = fields.next().map(str::trim) else {
            continue;
        };
        let Some(run_id) = fields.next().map(str::trim) else {
            continue;
        };
        if !container_id.is_empty() && found_daemon_id == daemon_id && !run_id.is_empty() {
            result.push((container_id.to_string(), run_id.to_string()));
        }
    }
    Ok(result)
}

/// Find labeled containers with no live non-terminal row. The discovery
/// query uses the same quoted template as the reconciliation path.
pub async fn find_orphans(
    cfg: &Config,
    state: &dyn OciRunState,
    daemon_id: &str,
) -> CaduceusResult<Vec<String>> {
    let adapter = OciAdapter::for_discovery(cfg, daemon_id);
    let containers = list_labeled_containers(&adapter, daemon_id).await?;
    let mut orphans = Vec::new();
    for (cid, run_id) in containers {
        match state.get(&run_id)? {
            Some(row) if !row.state.is_terminal() => {}
            _ => orphans.push(cid),
        }
    }
    Ok(orphans)
}

fn parse_issue_key(value: &str) -> crate::github::issue::IssueKey {
    crate::github::issue::IssueKey::parse(value).unwrap_or(crate::github::issue::IssueKey {
        owner: String::new(),
        repo: String::new(),
        number: 0,
    })
}

fn parse_exit_code(output: &str) -> i32 {
    output.trim().parse().unwrap_or(-1)
}

#[doc(hidden)]
pub fn parse_exit_code_for_tests(output: &str) -> i32 {
    parse_exit_code(output)
}

#[doc(hidden)]
pub fn discovery_template_for_tests() -> &'static str {
    OCI_RUN_ID_DISCOVERY_TEMPLATE
}

#[doc(hidden)]
pub fn ps_run_template_for_tests() -> &'static str {
    OCI_RUN_ID_PS_TEMPLATE
}

#[doc(hidden)]
pub fn daemon_discovery_template_for_tests() -> &'static str {
    OCI_DAEMON_ID_DISCOVERY_TEMPLATE
}

#[allow(dead_code)]
const _RUN_LABEL_KEY: &str = OCI_RUN_LABEL;
