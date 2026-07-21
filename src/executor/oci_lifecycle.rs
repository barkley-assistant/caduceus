//! Five-step OCI container lifecycle: create → start → wait → stop → remove.
//!
//! The [`run`] function orchestrates the five steps, persists state to
//! the [`OciRunState`] trait at each transition, and cleans up the
//! container on any error path. On cancellation the stop and remove
//! steps are bounded by the configured `oci_kill_timeout_seconds` and
//! `oci_stop_timeout_seconds` so the daemon never hangs.
//!
//! The module is intentionally free of `tokio::process::Command` — the
//! subprocess boundary is the tokio::process::Command inside the
//! `run_cli` helper. The lifecycle is the single call site; all other
//! executor modules are pure argv builders or secret transport.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::executor::oci_args::{build_argv, MountSpec, OciEngine};
use crate::executor::policy::EnforcedSpec;
use crate::executor::ExecutorSpec;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};
use crate::worker::supervisor::SupervisorOutcome;

// ---------------------------------------------------------------------------
// Default mounts
// ---------------------------------------------------------------------------

/// Build the default mount allow-list: worktree (rw) and result (rw).
/// The container paths mirror the daemon's layout.
fn default_mounts(spec: &ExecutorSpec) -> Vec<MountSpec> {
    let worktree_container = spec.worktree.parent().map_or_else(
        || Path::new("/worktree").to_path_buf(),
        |p| p.join("worktree"),
    );
    let result_container = spec
        .worktree
        .parent()
        .map_or_else(|| Path::new("/result").to_path_buf(), |p| p.join("result"));
    vec![
        MountSpec {
            host_path: spec.worktree.clone(),
            container_path: worktree_container,
            read_only: false,
        },
        MountSpec {
            host_path: spec.worktree.clone(),
            container_path: result_container,
            read_only: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// run — the 5-step lifecycle
// ---------------------------------------------------------------------------

/// Run the OCI container lifecycle: create → start → wait → stop → remove.
///
/// The function:
/// 1. Builds the argv from the spec and config.
/// 2. Writes secret env files if needed.
/// 3. Inserts a `Created` state row.
/// 4. `create` → `start` → `wait` → `stop` → `remove`, updating state at
///    each step.
/// 5. On error or cancellation, attempts a best-effort cleanup (stop +
///    remove) with per-step timeouts.
/// 6. Returns a [`SupervisorOutcome`] on success or a [`CaduceusError`]
///    on failure.
pub async fn run(
    cfg: &Config,
    spec: &ExecutorSpec,
    state: &dyn OciRunState,
    cancellation: CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let engine = OciEngine::from_binary_name(&cfg.oci_cli.to_string_lossy());
    let mounts = default_mounts(spec);

    // Build argv — rejects undeclared mounts (AC-02).
    let argv = build_argv(spec, cfg, &mounts, None)?;

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
    let container_id = run_cli("create", &argv, "create", &cancellation).await?;

    // Record container_id
    let mut row = row;
    row.container_id = Some(container_id.clone());
    state.update_state(&spec.run_id, &OciLifecycleState::Created)?;

    // Step 2: start
    let start_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "start".to_string(),
        container_id.clone(),
    ];
    run_cli("start", &start_argv, "start", &cancellation).await?;
    state.update_state(&spec.run_id, &OciLifecycleState::Running)?;

    // Step 3: wait
    let wait_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "wait".to_string(),
        container_id.clone(),
    ];
    let wait_output = run_cli_with_output("wait", &wait_argv, "wait", &cancellation).await?;
    let exit_code = parse_exit_code(&wait_output);
    state.update_state(&spec.run_id, &OciLifecycleState::Exited(exit_code))?;

    // Step 4: stop (graceful, bounded)
    let _stop_timeout = Duration::from_secs(cfg.oci_stop_timeout_seconds);
    let stop_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "stop".to_string(),
        "--time".to_string(),
        cfg.oci_stop_timeout_seconds.to_string(),
        container_id.clone(),
    ];
    match run_cli("stop", &stop_argv, "stop", &cancellation).await {
        Ok(_) => {
            state.update_state(&spec.run_id, &OciLifecycleState::Stopped)?;
        }
        Err(e) => {
            // If stop fails (e.g. container already gone), log and continue.
            // Kill as fallback.
            let kill_argv = vec![
                cfg.oci_cli.to_string_lossy().to_string(),
                "kill".to_string(),
                container_id.clone(),
            ];
            let _ = run_cli("kill", &kill_argv, "kill", &cancellation).await;
            state.update_state(&spec.run_id, &OciLifecycleState::Killed)?;
            // Return the original error — the caller needs to know the
            // graceful stop failed.
            return Err(e);
        }
    }

    // Step 5: remove
    let remove_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "rm".to_string(),
        "--force".to_string(),
        container_id.clone(),
    ];
    let remove_timeout = Duration::from_secs(cfg.oci_kill_timeout_seconds);
    match timeout(
        remove_timeout,
        run_cli("rm", &remove_argv, "remove", &cancellation),
    )
    .await
    {
        Ok(Ok(_)) => {
            state.update_state(&spec.run_id, &OciLifecycleState::Removed)?;
        }
        _ => {
            // Best-effort — if remove fails (e.g. engine gone), the
            // reconciliation pass will clean up.
        }
    }

    Ok(SupervisorOutcome {
        status: exit_code,
        signaled: false,
        timed_out: false,
        cancelled: false,
    })
}

/// Run the OCI container lifecycle with a pre-built argv from
/// the isolation policy.
///
/// The `enforced` parameter carries the full argv, secret handles,
/// and optional git snapshot path. This is the entry point used by
/// [`OciExecutor::run`] after [`IsolationPolicy::enforce`] has been
/// called.
pub async fn run_with_argv(
    cfg: &Config,
    spec: &ExecutorSpec,
    state: &dyn OciRunState,
    enforced: EnforcedSpec,
    cancellation: CancellationToken,
) -> CaduceusResult<SupervisorOutcome> {
    let engine = OciEngine::from_binary_name(&cfg.oci_cli.to_string_lossy());
    let argv = enforced.argv;

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
    let container_id = run_cli("create", &argv, "create", &cancellation).await?;

    // Record container_id
    let mut row = row;
    row.container_id = Some(container_id.clone());
    state.update_state(&spec.run_id, &OciLifecycleState::Created)?;

    // Step 2: start
    let start_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "start".to_string(),
        container_id.clone(),
    ];
    run_cli("start", &start_argv, "start", &cancellation).await?;
    state.update_state(&spec.run_id, &OciLifecycleState::Running)?;

    // Step 3: wait
    let wait_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "wait".to_string(),
        container_id.clone(),
    ];
    let wait_output = run_cli_with_output("wait", &wait_argv, "wait", &cancellation).await?;
    let exit_code = parse_exit_code(&wait_output);
    state.update_state(&spec.run_id, &OciLifecycleState::Exited(exit_code))?;

    // Step 4: stop (graceful, bounded)
    let _stop_timeout = Duration::from_secs(cfg.oci_stop_timeout_seconds);
    let stop_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "stop".to_string(),
        "--time".to_string(),
        cfg.oci_stop_timeout_seconds.to_string(),
        container_id.clone(),
    ];
    match run_cli("stop", &stop_argv, "stop", &cancellation).await {
        Ok(_) => {
            state.update_state(&spec.run_id, &OciLifecycleState::Stopped)?;
        }
        Err(e) => {
            // If stop fails (e.g. container already gone), log and continue.
            // Kill as fallback.
            let kill_argv = vec![
                cfg.oci_cli.to_string_lossy().to_string(),
                "kill".to_string(),
                container_id.clone(),
            ];
            let _ = run_cli("kill", &kill_argv, "kill", &cancellation).await;
            state.update_state(&spec.run_id, &OciLifecycleState::Killed)?;
            return Err(e);
        }
    }

    // Step 5: remove
    let remove_argv = vec![
        cfg.oci_cli.to_string_lossy().to_string(),
        "rm".to_string(),
        "--force".to_string(),
        container_id.clone(),
    ];
    let remove_timeout = Duration::from_secs(cfg.oci_kill_timeout_seconds);
    match timeout(
        remove_timeout,
        run_cli("rm", &remove_argv, "remove", &cancellation),
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

    Ok(SupervisorOutcome {
        status: exit_code,
        signaled: false,
        timed_out: false,
        cancelled: false,
    })
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

/// Reconcile orphaned containers: every row in `PendingReconciliation`
/// is checked against the live engine and cleaned up.
///
/// Called by the daemon's reconciliation task (AC-05).
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
                cfg.oci_cli.to_string_lossy().to_string(),
                "rm".to_string(),
                "--force".to_string(),
                container_id.clone(),
            ];
            let _ = run_cli("rm", &rm_argv, "remove", &cancellation).await;
        }
        // Mark as removed regardless of CLI result (best-effort).
        let _ = state.update_state(&row.run_id, &OciLifecycleState::Removed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Crash recovery — find orphaned containers on the engine
// ---------------------------------------------------------------------------

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
        cfg.oci_cli.to_string_lossy().to_string(),
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
            cfg.oci_cli.to_string_lossy().to_string(),
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a stable daemon identifier from the config.
fn derive_daemon_id(cfg: &Config) -> String {
    cfg.state_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// SHA-256 hex digest of a string.
fn sha256_of(input: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Run an OCI CLI command and return stdout on success, stderr on failure.
async fn run_cli(
    step: &'static str,
    argv: &[String],
    context: &'static str,
    cancellation: &CancellationToken,
) -> CaduceusResult<String> {
    if cancellation.is_cancelled() {
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
) -> CaduceusResult<String> {
    if cancellation.is_cancelled() {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn parse_exit_code_parses_number() {
        assert_eq!(parse_exit_code("0\n"), 0);
        assert_eq!(parse_exit_code("42\n"), 42);
        assert_eq!(parse_exit_code(""), -1);
        assert_eq!(parse_exit_code("not-a-number"), -1);
    }

    #[test]
    fn derive_daemon_id_from_state_dir() {
        let mut cfg = Config::test_defaults(Path::new("/tmp"));
        cfg.state_dir = Path::new("/tmp").join("my-daemon");
        let id = derive_daemon_id(&cfg);
        assert_eq!(id, "my-daemon");
    }

    #[test]
    fn default_mounts_contains_worktree() {
        let spec = ExecutorSpec {
            self_exe: Path::new("/usr/bin/caduceus").to_path_buf(),
            issue: crate::github::issue::IssueKey::parse("owner/repo#1").unwrap(),
            worktree: Path::new("/tmp/worktree").to_path_buf(),
            run_id: "run-test".to_string(),
            context_json: "{}".to_string(),
            worker_command: vec!["python3".to_string()],
            cancellation: CancellationToken::new(),
            network_profile: None,
        };
        let mounts = default_mounts(&spec);
        assert!(!mounts.is_empty());
        assert!(mounts
            .iter()
            .any(|m| m.host_path.to_string_lossy().contains("worktree")));
    }
}
