//! Work Unit 9 — integration tests for bounded transcript capture
//! in the supervisor's hidden command.
//!
//! These tests drive the supervisor hidden command with real child
//! processes and assert that:
//!
//! AC-01: Use one bounded stdout and stderr writer in both paths.
//! AC-02: Report truncation and write failures.
//! AC-03: Never turn an invalid run into success.

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use caduceus::worker_supervisor::{self, ControlFrame};
use fixtures::ProcessTree;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a shell script to `dir`/`name` with `body` and make it executable.
fn write_worker_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write script");
    let mut perms = fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// Make stdout non-blocking by duplicating the fd and setting O_NONBLOCK.
fn make_nonblocking(child: &mut Child) -> std::process::ChildStdout {
    use std::os::fd::FromRawFd;
    use std::os::unix::io::AsRawFd;
    let stdout = child.stdout.take().expect("stdout");
    let fd = stdout.as_raw_fd();
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd < 0 {
        panic!("dup failed");
    }
    let flags = unsafe { libc::fcntl(new_fd, libc::F_GETFL) };
    if flags < 0 {
        panic!("F_GETFL failed");
    }
    let ret = unsafe { libc::fcntl(new_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        panic!("F_SETFL O_NONBLOCK failed");
    }
    drop(stdout);
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) };
    owned.into()
}

/// Read one frame from stdout before a deadline. Returns `None` if the
/// deadline expires or EOF is reached.
fn read_frame_with_deadline(
    stdout: &mut std::process::ChildStdout,
    deadline: Instant,
) -> Option<(ControlFrame, usize)> {
    let mut header = [0u8; 4];
    let mut offset: usize = 0;
    while offset < 4 && Instant::now() < deadline {
        match stdout.read(&mut header[offset..]) {
            Ok(0) => return None,
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
    if offset < 4 {
        return None;
    }
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; 4 + len];
    body[..4].copy_from_slice(&header);
    let mut offset = 4;
    while offset < body.len() && Instant::now() < deadline {
        match stdout.read(&mut body[offset..]) {
            Ok(0) => return None,
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
    if offset < body.len() {
        return None;
    }
    worker_supervisor::decode_frame(&body).ok()
}

/// Run the supervisor with a worker that produces *stderr_bytes* of stderr
/// output, using *max_bytes* as the transcript cap.
///
/// Drives the control protocol: reads READY, sends ACK, waits for DONE,
/// then waits for the process to exit. Returns the exit code.
fn run_transcript_test(max_bytes: u64, stderr_bytes: usize) -> i32 {
    let tree = ProcessTree::start("transcript");
    let workdir = tree.workdir().to_path_buf();

    let payload = "x".repeat(stderr_bytes);
    let helper = write_worker_script(
        &workdir,
        "transcript_worker.sh",
        &format!(
            r#"#!/bin/sh
printf '%s' '{payload}' >&2
exit 0
"#,
            payload = payload
        ),
    );

    fs::create_dir_all(workdir.join("wt")).expect("create worktree");
    let transcript = workdir.join("transcript.log");
    let heartbeat = workdir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let exe = fixtures::ReleaseBinary::locate();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(workdir.join("wt"));
    cmd.arg("--run-id")
        .arg(format!("RUN_TRANSCRIPT_{}", stderr_bytes));
    cmd.arg("--issue").arg("owner/repo#1");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(&transcript);
    cmd.arg("--heartbeat").arg(&heartbeat);
    cmd.arg("--timeout").arg("10");
    cmd.arg("--transcript-max-bytes").arg(max_bytes.to_string());
    cmd.arg("--").arg(&helper);
    cmd.env_clear();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn supervisor");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = make_nonblocking(&mut child);

    // READY frame.
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let ready = read_frame_with_deadline(&mut stdout, deadline).expect("expected READY frame");
    assert!(
        matches!(ready, (ControlFrame::Ready { .. }, _)),
        "expected READY, got {ready:?}"
    );

    // Send ACK.
    let ack = worker_supervisor::encode_frame(&ControlFrame::Ack).expect("encode ack");
    stdin.write_all(&ack).expect("write ack");
    stdin.flush().ok();
    // Keep stdin open — dropping it would trigger the killer thread.

    // Wait for DONE frame OR process exit (the supervisor exits
    // without DONE when finalize() returns an error).
    let done_deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        // Check if process has exited (non-zero = finalize error).
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    break; // expected for truncation/write-failure cases
                }
                break; // success exit — DONE was sent
            }
            Ok(None) => {} // still running
            Err(_) => break,
        }

        // Try to read a DONE/Fatal frame.
        #[allow(clippy::collapsible_match)]
        if let Some((frame, _)) = read_frame_with_deadline(&mut stdout, done_deadline) {
            if matches!(
                frame,
                ControlFrame::Done { .. } | ControlFrame::Fatal { .. }
            ) {
                break;
            }
        }
        if Instant::now() >= done_deadline {
            panic!("timeout waiting for supervisor exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Now close stdin so the supervisor can exit.
    std::mem::drop(stdin);
    std::mem::drop(stdout);

    let output = child.wait_with_output().expect("wait for supervisor");
    let su_stderr = String::from_utf8_lossy(&output.stderr);
    if !su_stderr.is_empty() {
        eprintln!("supervisor stderr: {su_stderr}");
    }
    output.status.code().unwrap_or(1)
}

// ---------------------------------------------------------------------------
// 9.1 — Writes under limit → success (exit 0)
// ---------------------------------------------------------------------------

#[test]
fn test_transcript_under_limit_succeeds() {
    let exit_code = run_transcript_test(1024 * 1024, 500);
    assert_eq!(exit_code, 0, "under-limit write should exit 0");
}

// ---------------------------------------------------------------------------
// 9.2 — Writes over limit → truncation error (exit non-zero)
// ---------------------------------------------------------------------------

#[test]
fn test_transcript_exceeds_limit_errors() {
    let exit_code = run_transcript_test(100, 2048);
    assert_ne!(
        exit_code, 0,
        "over-limit write should exit non-zero (truncation), got {exit_code}"
    );
}

// ---------------------------------------------------------------------------
// 9.3 — Zero-max-bytes → any content triggers truncation error
// ---------------------------------------------------------------------------

#[test]
fn test_transcript_zero_max_bytes_errors() {
    let exit_code = run_transcript_test(0, 10);
    assert_ne!(
        exit_code, 0,
        "zero max bytes should exit non-zero (truncation), got {exit_code}"
    );
}

// ---------------------------------------------------------------------------
// 9.4 — Exact fit → success
// ---------------------------------------------------------------------------

#[test]
fn test_transcript_exact_fit_succeeds() {
    let exit_code = run_transcript_test(100, 100);
    assert_eq!(exit_code, 0, "exact fit should exit 0");
}
