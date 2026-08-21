use super::{
    build_supervisor_command, clear_heartbeat, read_frame_async, write_frame_async,
    write_heartbeat_record, ControlFrame, Heartbeat, SupervisorOutcome, WorkerRunPaths,
    HEARTBEAT_FILE_VERSION, MAX_FRAME_BYTES,
};

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use tokio::process::{Child, Command as TokioCommand};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::worker::supervisor::process_lifecycle::{
    decide_deadline_kill, DeadlineKillDecision, IDENTITY,
};

// Public spawn orchestrator

/// Top-level worker supervision entry point used by the
/// orchestration loop. The implementation here is the
/// canonical production spawn path:
///
/// 1. Open the transcript and heartbeat files in secure
///    mode before the supervisor is launched.
/// 2. Spawn the same binary in `__worker-supervisor` mode
///    with the cleared worker environment, the worktree path,
///    the run id, the canonical `CADUCEUS_*` context payload,
///    and the worker command.
/// 3. The supervisor's `stdin`/`stdout` are the daemon's
///    control/status pipes (inherited FDs, per the contract).
/// 4. Read `READY(pgid)` from the supervisor's stdout, send
///    `ACK` over its stdin so the supervisor opens the exec
///    gate.
/// 5. Await supervisor exit and the protocol reader.
/// 6. Remove the heartbeat, return the parsed
///    [`SupervisorOutcome`].
///
/// The supervisor owns the transcript (worker stdout+stderr); the
/// daemon does not open it. Supervisor diagnostics inherit to the
/// daemon's stderr.
///
/// `cancellation` is the daemon's
/// `tokio_util::sync::CancellationToken`. When triggered, the
/// daemon sends `TERM` to the supervisor and waits up to 2
/// seconds before escalating to `KILL`.
#[allow(clippy::too_many_arguments)]
pub async fn supervise(
    self_exe: &Path,
    cfg: &crate::infra::config::Config,
    issue: &IssueKey,
    worktree: &Path,
    run_id: &str,
    context_json: &str,
    worker_command: &[String],
    cancellation: tokio_util::sync::CancellationToken,
    issue_title: &str,
    issue_body: &str,
    labels: &[String],
    branch_name: &str,
) -> CaduceusResult<SupervisorOutcome> {
    let paths = WorkerRunPaths::new(cfg.state_dir.clone(), run_id.to_string());
    paths.ensure_dirs()?;
    let started_at = Utc::now();
    write_heartbeat_record(
        &Heartbeat {
            version: HEARTBEAT_FILE_VERSION,
            run_id: run_id.to_string(),
            pid: std::process::id(),
            started_at,
            updated_at: started_at,
            issue_key: issue.clone(),
            transcript_path: paths.transcript_path.clone(),
        },
        &paths.heartbeat_path,
    )?;

    let mut outcome = SupervisorOutcome {
        status: 1,
        signaled: false,
        timed_out: false,
        cancelled: false,
    };

    let spawn_result = run_supervisor(
        self_exe,
        cfg,
        issue,
        worktree,
        run_id,
        context_json,
        worker_command,
        &paths,
        cancellation,
        issue_title,
        issue_body,
        labels,
        branch_name,
    )
    .await;

    let result = match spawn_result {
        Ok(out) => {
            outcome = out;
            Ok(())
        }
        Err(err) => {
            tracing::warn!(error = %err, run_id, "supervisor failed; cleaning up");
            Err(err)
        }
    };

    if let Err(err) = clear_heartbeat(&paths.heartbeat_path) {
        tracing::warn!(error = %err, run_id, "heartbeat cleanup failed");
    }

    result.map(|_| outcome)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_supervisor(
    self_exe: &Path,
    cfg: &crate::infra::config::Config,
    issue: &IssueKey,
    worktree: &Path,
    run_id: &str,
    context_json: &str,
    worker_command: &[String],
    paths: &WorkerRunPaths,
    cancellation: tokio_util::sync::CancellationToken,
    issue_title: &str,
    issue_body: &str,
    labels: &[String],
    branch_name: &str,
) -> CaduceusResult<SupervisorOutcome> {
    let cmd = build_supervisor_command(
        self_exe,
        worktree,
        run_id,
        issue,
        context_json,
        worker_command,
        &paths.transcript_path,
        &paths.heartbeat_path,
        cfg.worker_timeout_seconds,
        cfg.transcript_max_bytes,
        issue_title,
        issue_body,
        labels,
        branch_name,
    );

    // Convert to a tokio command for async I/O.
    //
    // Do NOT call `process_group(0)` here: the supervisor becomes a
    // process-group leader via that call, but it then calls `setsid()`
    // to create a fresh session. `setsid()` fails with EPERM when the
    // caller is already a process-group leader, so pre-setting the pg
    // would break every worker run. The supervisor's own `setsid()`
    // puts it in a fresh session (whose PGID == its PID), which is
    // exactly the "fresh process-group leader for the whole supervisor
    // subtree" the daemon needs to broadcast to. The supervisor reports
    // that PGID in its READY frame.
    let mut tokio_cmd: TokioCommand = cmd.into();
    tokio_cmd.kill_on_drop(true);
    let mut child: Child = tokio_cmd.spawn().map_err(|err| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: format!("spawn __worker-supervisor: {err}"),
    })?;
    let started_at = Utc::now();

    let mut stdin = child.stdin.take().ok_or_else(|| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: "supervisor stdin was not piped".to_string(),
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| CaduceusError::Worker {
        context: "supervisor:spawn",
        stderr: "supervisor stdout was not piped".to_string(),
    })?;
    // Capture the worker timeout into an owned value so the
    // `'static` protocol task can read it without borrowing `cfg`.
    let worker_timeout_seconds = cfg.worker_timeout_seconds;

    // Protocol loop. Reads `READY(pgid)` → sends `ACK`;
    // reads `DONE` → returns; reads `FATAL` → returns error.
    // On timeout (cfg.worker_timeout_seconds), verifies worker
    // identity before signalling, sends TERM, waits 2 s,
    // re-verifies, then sends KILL.
    let protocol_task = {
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            let mut buf = Vec::with_capacity(MAX_FRAME_BYTES);
            // Track worker identity captured at READY for
            // PID-reuse checks before signalling.
            let mut worker_pgid: Option<i32> = None;
            let mut worker_start_ticks: Option<u64> = None;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        write_frame_async(&mut stdin, &ControlFrame::Terminate { force: false }).await.ok();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        write_frame_async(&mut stdin, &ControlFrame::Terminate { force: true }).await.ok();
                        return SupervisorOutcome {
                            status: 130,
                            signaled: true,
                            timed_out: false,
                            cancelled: true,
                        };
                    }
                    _ = tokio::time::sleep(Duration::from_secs(worker_timeout_seconds)) => {
                        // Deadline reached. Verify worker identity
                        // before signalling to avoid killing an
                        // unrelated process whose PID was recycled.
                        match decide_deadline_kill(IDENTITY, worker_pgid, worker_start_ticks) {
                            DeadlineKillDecision::Suppress => {
                                // PID was reused — do NOT signal.
                                return SupervisorOutcome {
                                    status: 0,
                                    signaled: false,
                                    timed_out: true,
                                    cancelled: false,
                                };
                            }
                            DeadlineKillDecision::Signal | DeadlineKillDecision::BestEffort => {
                                // Send TERM (graceful shutdown).
                                write_frame_async(
                                    &mut stdin,
                                    &ControlFrame::Terminate { force: false },
                                ).await.ok();
                            }
                        }

                        // Wait 2 s grace period then re-verify and KILL.
                        tokio::time::sleep(Duration::from_secs(2)).await;

                        match decide_deadline_kill(IDENTITY, worker_pgid, worker_start_ticks) {
                            DeadlineKillDecision::Suppress => {
                                return SupervisorOutcome {
                                    status: 0,
                                    signaled: false,
                                    timed_out: true,
                                    cancelled: false,
                                };
                            }
                            DeadlineKillDecision::Signal | DeadlineKillDecision::BestEffort => {
                                write_frame_async(
                                    &mut stdin,
                                    &ControlFrame::Terminate { force: true },
                                ).await.ok();
                            }
                        }

                        return SupervisorOutcome {
                            status: 137,
                            signaled: true,
                            timed_out: true,
                            cancelled: false,
                        };
                    }
                    frame = read_frame_async(&mut stdout, &mut buf) => {
                        let frame = match frame {
                            Ok(Some(f)) => f,
                            Ok(None) => {
                                // EOF — supervisor closed stdout.
                                return SupervisorOutcome {
                                    status: 0,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            Err(err) => return err.into_outcome(),
                        };
                        match frame {
                            ControlFrame::Ready { pgid } => {
                                write_frame_async(&mut stdin, &ControlFrame::Ack).await.ok();
                                // Capture worker identity for
                                // PID-reuse checks before
                                // deadline signalling.
                                worker_pgid = Some(pgid);
                                worker_start_ticks = IDENTITY.start_ticks(pgid);
                            }
                            ControlFrame::Done { status, signaled } => {
                                return SupervisorOutcome {
                                    status,
                                    signaled,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            ControlFrame::Fatal { reason } => {
                                tracing::warn!(reason, "supervisor reported FATAL");
                                return SupervisorOutcome {
                                    status: 1,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                            ControlFrame::Ack | ControlFrame::Terminate { .. } => {
                                tracing::warn!(opcode = ?frame.opcode(), "unexpected frame from supervisor");
                                return SupervisorOutcome {
                                    status: 1,
                                    signaled: false,
                                    timed_out: false,
                                    cancelled: false,
                                };
                            }
                        }
                    }
                }
            }
        })
    };

    // Heartbeat refresh: every 5s while the worker is alive.
    let hb_path = paths.heartbeat_path.clone();
    let hb_cancel = cancellation.clone();
    let started_at_copy = started_at;
    let issue_clone = issue.clone();
    let transcript_path_clone = paths.transcript_path.clone();
    let run_id_string = run_id.to_string();
    let heartbeat_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if hb_cancel.is_cancelled() {
                break;
            }
            let record = Heartbeat {
                version: HEARTBEAT_FILE_VERSION,
                run_id: run_id_string.clone(),
                pid: std::process::id(),
                started_at: started_at_copy,
                updated_at: Utc::now(),
                issue_key: issue_clone.clone(),
                transcript_path: transcript_path_clone.clone(),
            };
            if write_heartbeat_record(&record, &hb_path).is_err() {
                break;
            }
        }
    });

    // Await the supervisor child.
    let supervisor_status = child.wait().await.map_err(|err| CaduceusError::Worker {
        context: "supervisor:wait",
        stderr: format!("wait: {err}"),
    })?;

    // Drain the protocol task before firing the cleanup cancel. The
    // supervisor child is already dead, so its stdout closes and the
    // task's pending `read_frame_async` resolves to either the buffered
    // `DONE` frame or EOF. Cancelling first would race that buffered
    // `DONE`: the `biased` select! polls the cancel arm first, so a
    // worker that exited 0 was reported as `status: 130, cancelled:
    // true`. Firing the cancel after the join keeps it a no-op cleanup
    // signal while the protocol task still reports the real outcome.
    //
    // Cleanup (cancel + heartbeat abort) runs before the `?` so a
    // panicked protocol task still tears down the heartbeat loop
    // instead of leaking it until process exit.
    let outcome_result = protocol_task.await;
    cancellation.cancel();
    heartbeat_task.abort();
    let outcome = outcome_result.map_err(|err| CaduceusError::Worker {
        context: "supervisor:join",
        stderr: format!("join protocol task: {err}"),
    })?;

    let signaled = supervisor_status.code().is_none();
    let _ = signaled;
    Ok(outcome)
}

/// Helper trait extension so `CaduceusError` can map itself to
/// an outcome in the protocol task.
trait IntoOutcome {
    fn into_outcome(self) -> SupervisorOutcome;
}

impl IntoOutcome for CaduceusError {
    fn into_outcome(self) -> SupervisorOutcome {
        SupervisorOutcome {
            status: 1,
            signaled: false,
            timed_out: false,
            cancelled: false,
        }
    }
}
