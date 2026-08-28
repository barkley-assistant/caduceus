//! Five-step OCI container lifecycle: create → start → wait → stop → remove.
//!
//! The [`run_with_argv`] function orchestrates the five steps, persists
//! state to the [`OciRunState`] trait at each transition, and cleans up
//! the container on any error path. On cancellation the stop and remove
//! steps are bounded by the configured `sandbox.kill_timeout_seconds` and
//! `sandbox.stop_timeout_seconds` so the daemon never hangs.
//!
//! Two cancellation sources are honored during the wait step: the
//! daemon-shutdown token and the disk-pressure watchdog token
//! (issue #245). Cancellation from either token never early-returns —
//! the run falls through the stop → capture → remove sequence so no
//! container is ever leaked.
//!
//! The module is intentionally free of `tokio::process::Command` at the
//! *public* boundary — the subprocess calls live in the private
//! `run_cli` helpers. The lifecycle is the single call site; all other
//! executor modules are pure argv builders or secret transport.
//!
//! `run_with_argv` is the **sole** entry point. It receives a
//! pre-rendered `create` argv from the resolution → renderer pipeline
//! in `src/executor/oci.rs`; it never re-derives argv itself.

use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::executor::sandbox_spec::SandboxEngine;
use crate::executor::ExecutorSpec;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};
use crate::worker::supervisor::{BoundedTranscriptWriter, SupervisorOutcome};

/// Pinned cap for the daemon-side OCI diagnostic capture
/// (`engine.log`). A diagnostic tail only needs the final failure
/// output; independent of the worker-facing `transcript_max_bytes`
/// (a different audience and budget).
pub const OCI_DIAGNOSTIC_MAX_BYTES: u64 = 1024 * 1024;

/// Read chunk size for the diagnostic capture — memory stays bounded
/// to one chunk while the writer keeps the last-N-bytes tail.
const OCI_DIAGNOSTIC_CHUNK_BYTES: usize = 8 * 1024;

/// Run the OCI container lifecycle with a pre-built argv from the
/// resolution → renderer pipeline.
///
/// The `engine` parameter is passed explicitly (not re-read from
/// `cfg`) so the create argv and the start/wait/stop/rm argvs cannot
/// diverge from the renderer's engine; it also feeds the
/// `ContainerRunRow.engine` column.
///
/// Two cancellation tokens drive the wait step:
///
/// * `cancellation` — the daemon-shutdown token (unchanged contract);
/// * `watchdog` — the disk-pressure watchdog token (issue #245).
///
/// Cancellation from either token falls through the stop → capture →
/// remove sequence (never an early return) and reports
/// [`CaduceusError::Cancelled`]; the watchdog surfaces its typed
/// refusal separately through `DiskPressureGuard::try_acquire_oci`.
pub async fn run_with_argv(
    cfg: &Config,
    spec: &ExecutorSpec,
    state: &dyn OciRunState,
    engine: SandboxEngine,
    argv: Vec<String>,
    cancellation: CancellationToken,
    watchdog: CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    // Insert a Created state row.
    let now = chrono::Utc::now().to_rfc3339();
    let row = ContainerRunRow {
        run_id: spec.run_id.clone(),
        container_id: None,
        state: OciLifecycleState::Created,
        engine: format!("{engine:?}"),
        created_at: now.clone(),
        updated_at: now.clone(),
        daemon_id: derive_daemon_id(cfg),
        issue_id: spec.issue.display_key(),
        worker_command_sha256: sha256_of(&spec.worker_command.join(" ")),
    };
    state.insert(&row)?;

    // Step 1: create
    let container_id = run_cli("create", &argv, "create", &cancellation, &watchdog).await?;

    // Record container_id
    let mut row = row;
    row.container_id = Some(container_id.clone());
    state.update_state(&spec.run_id, &OciLifecycleState::Created)?;

    // Step 2: start
    let start_argv = vec![
        engine.binary_name().to_string(),
        "start".to_string(),
        container_id.clone(),
    ];
    run_cli("start", &start_argv, "start", &cancellation, &watchdog).await?;
    state.update_state(&spec.run_id, &OciLifecycleState::Running)?;

    // Step 3: wait — select over the wait command and BOTH
    // cancellation sources (daemon shutdown + disk-pressure watchdog).
    // Cancellation from either token must NOT early-return: it falls
    // through the stop → capture → rm sequence below so the running
    // container is never leaked (issue #245).
    let wait_argv = vec![
        engine.binary_name().to_string(),
        "wait".to_string(),
        container_id.clone(),
    ];
    enum WaitOutcome {
        /// `wait` completed and printed an exit code.
        Completed(String),
        /// Either token cancelled — the run was terminated.
        Cancelled,
        /// `wait` itself failed (engine error); preserved and
        /// surfaced after cleanup so the container is not leaked.
        Failed(CaduceusError),
    }
    let wait_outcome = tokio::select! {
        result = run_cli_with_output("wait", &wait_argv, "wait", &cancellation, &watchdog) => {
            match result {
                Ok(output) => WaitOutcome::Completed(output),
                // A pre-spawn cancellation check fired inside the
                // helper — same as the token branches below.
                Err(CaduceusError::Cancelled) => WaitOutcome::Cancelled,
                Err(err) => WaitOutcome::Failed(err),
            }
        }
        _ = watchdog.cancelled() => WaitOutcome::Cancelled,
        _ = cancellation.cancelled() => WaitOutcome::Cancelled,
    };
    let exit_code = match &wait_outcome {
        WaitOutcome::Completed(output) => {
            let code = parse_exit_code(output);
            state.update_state(&spec.run_id, &OciLifecycleState::Exited(code))?;
            code
        }
        _ => -1,
    };

    // Step 4: stop (graceful, bounded). Runs on EVERY path — normal
    // exit, watchdog breach, daemon shutdown, wait failure — so no
    // container leaks. The cleanup steps check ONLY the daemon
    // `cancellation` token: a watchdog breach must never prevent
    // cleanup of the container it just cancelled.
    let stop_argv = vec![
        engine.binary_name().to_string(),
        "stop".to_string(),
        "--time".to_string(),
        cfg.sandbox().stop_timeout_seconds.to_string(),
        container_id.clone(),
    ];
    match run_cli("stop", &stop_argv, "stop", &cancellation, &cancellation).await {
        Ok(_) => {
            state.update_state(&spec.run_id, &OciLifecycleState::Stopped)?;
        }
        Err(e) => {
            // If stop fails (e.g. container already gone), log and continue.
            // Kill as fallback.
            let kill_argv = vec![
                engine.binary_name().to_string(),
                "kill".to_string(),
                container_id.clone(),
            ];
            let _ = run_cli("kill", &kill_argv, "kill", &cancellation, &cancellation).await;
            state.update_state(&spec.run_id, &OciLifecycleState::Killed)?;
            // Preserve the original wait failure when one existed —
            // the stop failure is secondary diagnostics on that path.
            return Err(match wait_outcome {
                WaitOutcome::Failed(err) => err,
                _ => e,
            });
        }
    }

    // Step 4.5: bounded daemon-side diagnostic capture (issue #245).
    // After the container has exited (logs are complete) but BEFORE
    // `rm` destroys them. Best-effort — never fails the run.
    capture_engine_logs(cfg, engine, &container_id, &spec.run_id).await;

    // Step 5: remove
    let remove_argv = vec![
        engine.binary_name().to_string(),
        "rm".to_string(),
        "--force".to_string(),
        container_id.clone(),
    ];
    let remove_timeout = Duration::from_secs(cfg.sandbox().kill_timeout_seconds);
    match timeout(
        remove_timeout,
        run_cli("rm", &remove_argv, "remove", &cancellation, &cancellation),
    )
    .await
    {
        Ok(Ok(_)) => {
            state.update_state(&spec.run_id, &OciLifecycleState::Removed)?;
        }
        _ => {
            // Best-effort — if remove fails, reconciliation cleans up.
        }
    }

    match wait_outcome {
        WaitOutcome::Completed(_) => Ok(SupervisorOutcome {
            status: exit_code,
            signaled: false,
            timed_out: false,
            cancelled: false,
        }),
        // Watchdog breach or daemon shutdown: the run reports as
        // cancelled to the tick, which classifies retries as usual.
        WaitOutcome::Cancelled => Err(CaduceusError::Cancelled),
        WaitOutcome::Failed(err) => Err(err),
    }
}

/// Capture `<engine> logs <container_id>` into a bounded diagnostic
/// file at `<state_dir>/oci-runs/<run_id>/engine.log`, capped at
/// [`OCI_DIAGNOSTIC_MAX_BYTES`] via [`BoundedTranscriptWriter`] (which
/// opens 0600 with `O_NOFOLLOW` and keeps the last-N-bytes tail).
/// stdout and stderr are drained concurrently in
/// [`OCI_DIAGNOSTIC_CHUNK_BYTES`] chunks so a chatty stream cannot
/// deadlock the other pipe. Every failure is logged and never
/// propagates — the capture is best-effort diagnostics.
async fn capture_engine_logs(
    cfg: &Config,
    engine: SandboxEngine,
    container_id: &str,
    run_id: &str,
) {
    let result = capture_engine_logs_inner(cfg, engine, container_id, run_id).await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "OCI diagnostic capture failed (best-effort)");
    }
}

async fn capture_engine_logs_inner(
    cfg: &Config,
    engine: SandboxEngine,
    container_id: &str,
    run_id: &str,
) -> CaduceusResult<()> {
    // The run dir exists (created by the pre-flight engine probe);
    // create it best-effort for direct lifecycle callers.
    let path = cfg
        .state_dir
        .join("oci-runs")
        .join(run_id)
        .join("engine.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut writer = BoundedTranscriptWriter::new(path.clone(), OCI_DIAGNOSTIC_MAX_BYTES)?;

    let mut child = Command::new(engine.binary_name())
        .args(["logs", container_id])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CaduceusError::Other(format!("engine logs spawn for {container_id}: {e}")))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");

    let mut out_chunk = vec![0u8; OCI_DIAGNOSTIC_CHUNK_BYTES];
    let mut err_chunk = vec![0u8; OCI_DIAGNOSTIC_CHUNK_BYTES];
    loop {
        tokio::select! {
            read = stdout.read(&mut out_chunk) => {
                match read {
                    Ok(0) | Err(_) => {
                        // stdout drained — drain stderr to EOF.
                        loop {
                            match stderr.read(&mut err_chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => writer.write_bytes(&err_chunk[..n]),
                            }
                        }
                        break;
                    }
                    Ok(n) => writer.write_bytes(&out_chunk[..n]),
                }
            }
            read = stderr.read(&mut err_chunk) => {
                match read {
                    Ok(0) | Err(_) => {
                        // stderr drained — drain stdout to EOF.
                        loop {
                            match stdout.read(&mut out_chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => writer.write_bytes(&out_chunk[..n]),
                            }
                        }
                        break;
                    }
                    Ok(n) => writer.write_bytes(&err_chunk[..n]),
                }
            }
        }
    }
    let _ = child.wait().await;
    // Enforce the hard cap: the writer truncates once on first
    // overflow but keeps appending the remaining stream (its
    // drain-must-keep-running contract), so re-truncate after the
    // stream drains to bound the persisted artifact at the cap plus
    // the small truncation marker.
    let _ = crate::worker::supervisor::truncate_transcript(&path, OCI_DIAGNOSTIC_MAX_BYTES);
    writer.finalize()
}

/// Reconcile orphaned containers: every row in `PendingReconciliation`
/// is checked against the live engine and cleaned up by the daemon's
/// reconciliation task.
pub async fn reconcile(
    cfg: &Config,
    state: &dyn OciRunState,
    cancellation: CancellationToken,
) -> CaduceusResult<()> {
    let pending = state.list_pending_reconciliation()?;
    for row in &pending {
        if cancellation.is_cancelled() {
            break;
        }
        // Try to remove the container if it still exists.
        if let Some(ref container_id) = row.container_id {
            let rm_argv = vec![
                cfg.sandbox().engine.binary_name().to_string(),
                "rm".to_string(),
                "--force".to_string(),
                container_id.clone(),
            ];
            let _ = run_cli("rm", &rm_argv, "remove", &cancellation, &cancellation).await;
        }
        // Mark as removed regardless of CLI result (best-effort).
        let _ = state.update_state(&row.run_id, &OciLifecycleState::Removed);
    }
    Ok(())
}

/// Find containers on the engine that have caduceus labels but no
/// corresponding non-removed state row. These are containers that
/// were created before the daemon crashed.
///
/// Returns a list of container IDs that should be stopped and removed.
pub async fn find_orphans(
    cfg: &Config,
    state: &dyn OciRunState,
    daemon_id: &str,
) -> CaduceusResult<Vec<String>> {
    // List all containers with caduceus.daemon_id label.
    let ps_argv = vec![
        cfg.sandbox().engine.binary_name().to_string(),
        "ps".to_string(),
        "-a".to_string(),
        "--filter".to_string(),
        format!("label=caduceus.daemon_id={daemon_id}"),
        "--format".to_string(),
        "{{.ID}}".to_string(),
    ];

    let output = run_cli_raw("ps", &ps_argv, "ps").await?;
    let engine_ids: Vec<String> = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if engine_ids.is_empty() {
        return Ok(vec![]);
    }

    // Check each against our state.
    let mut orphans = Vec::new();
    for cid in &engine_ids {
        // We need to find the run_id from the container labels.
        // Query the container inspect for caduceus.run_id.
        let inspect_argv = vec![
            cfg.sandbox().engine.binary_name().to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Config.Labels.caduceus_run_id}}".to_string(),
            cid.clone(),
        ];
        let inspect_output = run_cli_raw("inspect", &inspect_argv, "inspect").await?;
        let run_id = inspect_output.trim().to_string();
        if run_id.is_empty() {
            continue;
        }
        // Check if we have a state row for this run_id that is not Removed.
        match state.get(&run_id) {
            Ok(Some(row)) => {
                if row.state == OciLifecycleState::Removed {
                    continue;
                }
            }
            _ => {
                // No state row or error — this is an orphan.
                orphans.push(cid.clone());
            }
        }
    }

    Ok(orphans)
}

/// Derive a stable daemon identifier from the config.
fn derive_daemon_id(cfg: &Config) -> String {
    crate::executor::sandbox_spec::derive_daemon_id(cfg)
}

/// SHA-256 hex digest of a string.
fn sha256_of(input: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Run an OCI CLI command and return stdout on success, stderr on failure.
///
/// The pre-spawn check consults BOTH the daemon `cancellation` token
/// and the `watchdog` token. Cleanup call sites pass
/// `&cancellation` as `watchdog` so a watchdog breach never blocks
/// cleanup of the container it just cancelled.
async fn run_cli(
    step: &'static str,
    argv: &[String],
    context: &'static str,
    cancellation: &CancellationToken,
    watchdog: &CancellationToken,
) -> CaduceusResult<String> {
    if cancellation.is_cancelled() || watchdog.is_cancelled() {
        return Err(CaduceusError::Cancelled);
    }

    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
        .map_err(|e| to_oci_error(step, context, &e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(to_oci_error(step, context, &stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run an OCI CLI command and return the full stdout text (for wait parsing).
async fn run_cli_with_output(
    step: &'static str,
    argv: &[String],
    context: &'static str,
    cancellation: &CancellationToken,
    watchdog: &CancellationToken,
) -> CaduceusResult<String> {
    if cancellation.is_cancelled() || watchdog.is_cancelled() {
        return Err(CaduceusError::Cancelled);
    }

    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
        .map_err(|e| to_oci_error(step, context, &e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(to_oci_error(step, context, &stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run an OCI CLI command and return raw stdout (no trim).
async fn run_cli_raw(
    step: &'static str,
    argv: &[String],
    context: &'static str,
) -> CaduceusResult<String> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
        .map_err(|e| to_oci_error(step, context, &e.to_string()))?;

    if !output.status.success() {
        // Engine not found / not running → OciEngineUnavailable
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(to_oci_error(step, context, &stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Map a CLI failure to the correct typed error.
fn to_oci_error(step: &'static str, context: &'static str, detail: &str) -> CaduceusError {
    match step {
        "create" => CaduceusError::OciCreateFailed {
            context,
            stderr: detail.to_string(),
        },
        "start" => CaduceusError::OciStartFailed {
            context,
            stderr: detail.to_string(),
        },
        "wait" => CaduceusError::OciWaitFailed {
            context,
            stderr: detail.to_string(),
        },
        "stop" => CaduceusError::OciStopFailed {
            context,
            stderr: detail.to_string(),
        },
        "rm" => CaduceusError::OciRemoveFailed {
            context,
            stderr: detail.to_string(),
        },
        _ => CaduceusError::OciEngineUnavailable {
            detail: detail.to_string(),
        },
    }
}

/// Parse the exit code from `docker wait` / `podman wait` output.
fn parse_exit_code(output: &str) -> i32 {
    output.trim().parse().unwrap_or(-1)
}

/// Test seam: expose the wait-output exit-code parser so integration
/// tests can assert the parse contract. Identical to the private
/// [`parse_exit_code`].
#[doc(hidden)]
pub fn parse_exit_code_for_tests(output: &str) -> i32 {
    parse_exit_code(output)
}

/// Test seam: expose the daemon-id derivation so integration tests
/// can assert the state-dir contract. Identical to the private
/// [`derive_daemon_id`].
#[doc(hidden)]
pub fn derive_daemon_id_for_tests(cfg: &Config) -> String {
    derive_daemon_id(cfg)
}
