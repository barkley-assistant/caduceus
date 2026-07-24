#![allow(dead_code, unused_imports)]
use super::*;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command as TokioCommand};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Public spawn orchestrator
// ---------------------------------------------------------------------------

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
/// 5. Drain supervisor `stderr` into the transcript, bounded
///    by `cfg.transcript_max_bytes`, with a single truncation
///    marker and continuing drain/discard after truncation.
/// 6. Await supervisor exit, both readers, and writer.
/// 7. Remove the heartbeat, return the parsed
///    [`SupervisorOutcome`].
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
    let stderr = child.stderr.take();
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
            let mut worker_starttime: Option<u64> = None;
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
                        match (worker_pgid, worker_starttime) {
                            (Some(pgid), Some(expected)) => {
                                if !verify_identity(pgid, expected) {
                                    // PID was reused — do NOT signal.
                                    return SupervisorOutcome {
                                        status: 0,
                                        signaled: false,
                                        timed_out: true,
                                        cancelled: false,
                                    };
                                }
                                // Send TERM (graceful shutdown).
                                write_frame_async(
                                    &mut stdin,
                                    &ControlFrame::Terminate { force: false },
                                ).await.ok();
                            }
                            _ => {
                                // Never got READY — best-effort
                                // shutdown without identity check.
                                write_frame_async(
                                    &mut stdin,
                                    &ControlFrame::Terminate { force: false },
                                ).await.ok();
                            }
                        }

                        // Wait 2 s grace period then re-verify and KILL.
                        tokio::time::sleep(Duration::from_secs(2)).await;

                        match (worker_pgid, worker_starttime) {
                            (Some(pgid), Some(expected)) => {
                                if !verify_identity(pgid, expected) {
                                    return SupervisorOutcome {
                                        status: 0,
                                        signaled: false,
                                        timed_out: true,
                                        cancelled: false,
                                    };
                                }
                                write_frame_async(
                                    &mut stdin,
                                    &ControlFrame::Terminate { force: true },
                                ).await.ok();
                            }
                            _ => {
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
                                worker_starttime = read_proc_starttime(pgid);
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

    // Stderr drain — write into the transcript, bounded by
    // cfg.transcript_max_bytes.
    if let Some(mut stderr) = stderr {
        let path = paths.transcript_path.clone();
        let max_bytes = cfg.transcript_max_bytes;
        let _drain_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let writer = match BoundedTranscriptWriter::new(path.clone(), max_bytes) {
                Ok(w) => std::sync::Arc::new(std::sync::Mutex::new(w)),
                Err(_) => return,
            };
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        let w = writer.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let mut guard = w.lock().unwrap();
                            guard.write_bytes(&chunk);
                        })
                        .await;
                    }
                    Err(_) => break,
                }
            }
            let guard = std::sync::Arc::try_unwrap(writer)
                .unwrap_or_else(|_| panic!("writer arc still referenced"))
                .into_inner()
                .unwrap_or_else(|e| e.into_inner());
            if let Err(err) = guard.finalize() {
                tracing::warn!(error = %err, "transcript finalize");
            }
        });
    }

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

    cancellation.cancel();
    let outcome = protocol_task.await.map_err(|err| CaduceusError::Worker {
        context: "supervisor:join",
        stderr: format!("join protocol task: {err}"),
    })?;
    heartbeat_task.abort();

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

// ---------------------------------------------------------------------------
// Self-test (cargo test --lib)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    pub(crate) fn frame_round_trip() {
        let cases = vec![
            ControlFrame::Ready { pgid: 1234 },
            ControlFrame::Done {
                status: 0,
                signaled: false,
            },
            ControlFrame::Done {
                status: 9,
                signaled: true,
            },
            ControlFrame::Fatal {
                reason: "boom".to_string(),
            },
            ControlFrame::Terminate { force: false },
            ControlFrame::Terminate { force: true },
            ControlFrame::Ack,
        ];
        for case in cases {
            let encoded = encode_frame(&case).expect("encode");
            let (decoded, consumed) = decode_frame(&encoded).expect("decode");
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, case);
        }
    }

    #[test]
    pub(crate) fn frame_rejects_wrong_version() {
        let mut bytes = encode_frame(&ControlFrame::Ack).expect("encode");
        // Mangle the version byte.
        bytes[6] = b'9';
        let err = decode_frame(&bytes).expect_err("must reject");
        let msg = format!("{err:?}");
        assert!(msg.contains("unsupported protocol version"), "{msg}");
    }

    #[test]
    pub(crate) fn frame_rejects_oversize() {
        // Construct a buffer whose first 4 bytes encode a
        // length that exceeds MAX_FRAME_BYTES, then put enough
        // payload after it so the frame *appears* complete —
        // the decoder should reject it on the size check
        // before parsing the body.
        let mut bytes = Vec::new();
        let oversize = (MAX_FRAME_BYTES as u32) + 1;
        bytes.extend_from_slice(&oversize.to_le_bytes());
        bytes.resize(4 + oversize as usize, 0);
        let err = decode_frame(&bytes).expect_err("must reject");
        let msg = format!("{err:?}");
        assert!(msg.contains("exceeds cap"), "{msg}");
    }

    #[test]
    pub(crate) fn heartbeat_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hbeat");
        write_heartbeat(&path).expect("write");
        let read = read_heartbeat(&path).expect("read");
        assert!((chrono::Utc::now() - read).num_seconds().abs() < 5);
        clear_heartbeat(&path).expect("clear");
        assert!(read_heartbeat(&path).is_none());
    }

    #[test]
    pub(crate) fn transcript_truncation_appends_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.log");
        let mut file = open_transcript(&path).expect("open");
        for _ in 0..1000 {
            file.write_all(b"chunk\n").expect("write");
        }
        drop(file);
        let truncated = truncate_transcript(&path, 64).expect("truncate");
        assert!(truncated);
        let meta = std::fs::metadata(&path).expect("stat");
        assert!(
            meta.len() <= 256,
            "transcript should be roughly capped; got {}",
            meta.len()
        );
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("truncated"), "marker missing from {body:?}");
    }

    #[test]
    pub(crate) fn paths_ensure_dirs_creates_secure_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = WorkerRunPaths::new(dir.path().to_path_buf(), "RUN01".to_string());
        paths.ensure_dirs().expect("ensure_dirs");
        let meta = std::fs::metadata(dir.path().join("runs")).expect("stat runs");
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    }

    #[cfg(target_os = "linux")]
    #[test]
    pub(crate) fn bounded_writer_new_creates_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw.log");
        let writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
        assert!(path.is_file(), "file must exist");
        let meta = std::fs::metadata(&path).expect("stat");
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "file mode must be 0600, got {:o}",
            meta.permissions().mode()
        );
        drop(writer);
    }

    #[test]
    pub(crate) fn bounded_writer_under_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_under.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
        let data = vec![b'a'; 100];
        writer.write_bytes(&data);
        assert!(!writer.truncated, "should not be truncated");
        writer.finalize().expect("finalize should succeed");
    }

    #[test]
    pub(crate) fn bounded_writer_exact_fit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_exact.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 100).expect("new");
        let data = vec![b'a'; 100];
        writer.write_bytes(&data);
        assert!(!writer.truncated, "exact fit should not truncate");
        writer.finalize().expect("finalize should succeed");
    }

    #[test]
    pub(crate) fn bounded_writer_over_limit_sets_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_over.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 50).expect("new");
        let data = vec![b'a'; 100];
        writer.write_bytes(&data);
        assert!(writer.truncated, "should be truncated");
        let err = writer.finalize().expect_err("finalize should fail");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("truncated"),
            "error must mention truncated, got {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    pub(crate) fn bounded_writer_write_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_fail.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 1024).expect("new");
        // Write some bytes first.
        writer.write_bytes(b"first write");
        // Replace the file handle with /dev/full so writes fail.
        writer.file = std::fs::File::open("/dev/full").expect("open /dev/full");
        writer.write_bytes(b"this should fail");
        assert!(
            writer.write_failures > 0,
            "write_failures should be > 0, got {}",
            writer.write_failures
        );
        let err = writer.finalize().expect_err("finalize should fail");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("write_failures"),
            "error must mention write_failures, got {msg}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    pub(crate) fn bounded_writer_truncation_takes_precedence() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_prec.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 50).expect("new");
        // Write enough to trigger truncation.
        let data = vec![b'a'; 100];
        writer.write_bytes(&data);
        assert!(writer.truncated, "should be truncated");
        // Now replace file handle with /dev/full so further writes fail.
        writer.file = std::fs::File::open("/dev/full").expect("open /dev/full");
        writer.write_bytes(b"more data");
        assert!(
            writer.write_failures > 0,
            "write_failures should be > 0, got {}",
            writer.write_failures
        );
        let err = writer.finalize().expect_err("finalize should fail");
        let msg = format!("{err:?}");
        // Truncation takes precedence over write_failures.
        assert!(
            msg.contains("truncated"),
            "error must mention truncated, got {msg}"
        );
    }

    #[test]
    pub(crate) fn bounded_writer_max_bytes_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bw_zero.log");
        let mut writer = BoundedTranscriptWriter::new(path.clone(), 0).expect("new");
        let data = vec![b'a'; 10];
        writer.write_bytes(&data);
        assert!(
            writer.truncated,
            "max_bytes=0: any write should set truncated"
        );
    }
    // later units use to verify a worker PID has not been reused before
    // signalling. They are Linux-only because they read /proc/<pid>/stat.
    #[cfg(target_os = "linux")]
    #[test]
    pub(crate) fn read_proc_starttime_parses_field22() {
        // Deterministic unit check of the field parser: feed a synthetic
        // /proc/<pid>/stat line and confirm field 22 (starttime) is read at
        // after-paren index 19.
        let synthetic =
            "1234 (fake_worker) S 1 1234 1234 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 12345678 0 0 0";
        assert_eq!(
            parse_starttime_from_stat(synthetic),
            Some(12_345_678),
            "field 22 (0-based 19 after ')') must be the starttime"
        );

        // Integration check against a real, still-alive process. Spawn a
        // long-running child but never wait on it so it stays alive for the
        // read. /proc/<pid>/stat starttime is always non-zero for a live
        // process.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let starttime = read_proc_starttime(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            matches!(starttime, Some(x) if x > 0),
            "live process starttime should be Some(>0), got {starttime:?}"
        );

        // A wildly impossible PID yields None (process gone).
        assert_eq!(read_proc_starttime(999_999), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    pub(crate) fn verify_identity_detects_reuse() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let starttime = read_proc_starttime(pid).expect("live starttime");
        assert!(starttime > 0);

        // Correct starttime → identity confirmed.
        assert!(
            verify_identity(pid, starttime),
            "matching starttime must verify"
        );
        // Off-by-one starttime → PID reuse / mismatch must reject.
        assert!(
            !verify_identity(pid, starttime + 1),
            "stale starttime must fail verification"
        );
        // Gone process → cannot verify.
        assert!(
            !verify_identity(999_999, 0),
            "missing process must fail verification"
        );

        let _ = child.kill();
        let _ = child.wait();
    }
}
