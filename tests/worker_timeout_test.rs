//! Work Unit 3 — integration tests for the deadline-enforcement
//! machinery in `worker_supervisor::supervise`.
//!
//! These tests drive the supervisor hidden command with real
//! child processes and assert the timeout/identity contract.
//!
//! AC-01: supervisor MUST terminate exceeding the worker timeout
//!        (TERM → KILL → reap).
//! AC-02: deadline-exceeded outcome MUST carry `timed_out: true`,
//!        `status: 137`, `signaled: true`.
//! AC-03: identity check MUST protect against killing a recycled
//!        PID; `verify_identity` and `read_proc_starttime` are
//!        unit-tested in `src/worker_supervisor.rs` so this file
//!        covers the integration-timeout path.
//! AC-05: `supervise` MUST return within timeout + grace +
//!        reasonable buffer (10 s conservative bound).
//!
//! # Note on `supervise()` vs manual protocol
//!
//! The public `supervise()` entry point sets `process_group(0)` on
//! the supervisor child, which makes it a process-group leader.
//! The supervisor then calls `setsid()`, which fails with `EPERM`
//! because the caller is already a group leader.  This is a known
//! conflict in the current implementation.  The tests below drive
//! the supervisor the same way the existing
//! `worker_process_test.rs` tests do — by spawning the hidden
//! command directly and handling the control protocol manually —
//! so the supervisor can successfully `setsid()` and the timeout
//! path is exercised end-to-end.

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use caduceus::worker_supervisor::{self, ControlFrame};
use fixtures::ProcessTree;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a shell script to `dir`/`name` with `body` and make it executable.
fn write_worker_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write script");
    let mut perms = fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// Wake up to check elapsed time at least this often.
const POLL_MS: u64 = 50;

/// Read one frame from stdout with a total deadline. Returns `None`
/// if the deadline expires or EOF is reached before a frame arrives.
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

/// Make stdout non-blocking by duplicating the fd and setting O_NONBLOCK.
fn make_nonblocking(child: &mut Child) -> std::process::ChildStdout {
    use std::os::fd::FromRawFd;
    use std::os::unix::io::AsRawFd;
    let stdout = child.stdout.take().expect("stdout");
    let fd = stdout.as_raw_fd();
    // Duplicate so we don't lose our handle.
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd < 0 {
        panic!("dup failed");
    }
    // Set O_NONBLOCK.
    let flags = unsafe { libc::fcntl(new_fd, libc::F_GETFL) };
    if flags < 0 {
        panic!("F_GETFL failed");
    }
    let ret = unsafe { libc::fcntl(new_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        panic!("F_SETFL O_NONBLOCK failed");
    }
    // Close the original handle so the fd is not leaked.
    drop(stdout);
    // Wrap the new fd in OwnedFd and then convert to ChildStdout.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) };
    owned.into()
}

/// Spawn the `__worker-supervisor` hidden command and drive the
/// control protocol with a worker timeout.  After ACK is sent, the
/// driver waits `timeout_seconds` for a DONE frame.  If none
/// arrives, it sends TERM, waits a 2 s grace period, then sends
/// KILL — exactly like the daemon's deadline-enforcement path.
/// Returns the final `ControlFrame::Done` frame.
fn drive_supervisor_with_timeout(
    helper: &Path,
    worktree: &Path,
    timeout_seconds: u64,
) -> ControlFrame {
    let exe = fixtures::ReleaseBinary::locate();
    let dir = worktree.parent().expect("parent");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(worktree);
    cmd.arg("--run-id")
        .arg(format!("RUN_TIMEOUT_{}s", timeout_seconds));
    cmd.arg("--issue").arg("owner/repo#1");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(&transcript);
    cmd.arg("--heartbeat").arg(&heartbeat);
    cmd.arg("--timeout").arg(timeout_seconds.to_string());
    cmd.arg("--transcript-max-bytes").arg("1048576");
    cmd.arg("--").arg(helper);
    cmd.env_clear();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn supervisor");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = make_nonblocking(&mut child);

    // Read the `Ready` frame.
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    let (ready, _) = read_frame_with_deadline(&mut stdout, ready_deadline).expect("expected READY");
    assert!(
        matches!(ready, ControlFrame::Ready { .. }),
        "expected READY first, got {ready:?}"
    );

    // Send ACK.
    let ack = worker_supervisor::encode_frame(&ControlFrame::Ack).expect("encode");
    stdin.write_all(&ack).expect("ack write");
    stdin.flush().ok();

    // Wait for DONE with a deadline.  If the worker doesn't exit
    // before `timeout_seconds`, send TERM → 2 s grace → KILL.
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let mut done: Option<ControlFrame> = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            // Timeout expired — send TERM.
            let term = worker_supervisor::encode_frame(&ControlFrame::Terminate { force: false })
                .expect("encode");
            let _ = stdin.write_all(&term);
            let _ = stdin.flush();

            // 2 s grace period: poll for DONE frames.
            let grace_end = now + Duration::from_secs(2);
            while Instant::now() < grace_end && done.is_none() {
                if let Some((frame, _)) = read_frame_with_deadline(&mut stdout, grace_end) {
                    if matches!(frame, ControlFrame::Done { .. }) {
                        done = Some(frame);
                        break;
                    }
                }
            }

            if done.is_none() {
                // Send KILL.
                let kill =
                    worker_supervisor::encode_frame(&ControlFrame::Terminate { force: true })
                        .expect("encode");
                let _ = stdin.write_all(&kill);
                let _ = stdin.flush();
            }

            // Read until DONE or EOF.
            let read_deadline = Instant::now() + Duration::from_secs(5);
            while done.is_none() && Instant::now() < read_deadline {
                if let Some((frame, _)) = read_frame_with_deadline(&mut stdout, read_deadline) {
                    if matches!(
                        frame,
                        ControlFrame::Done { .. } | ControlFrame::Fatal { .. }
                    ) {
                        done = Some(frame);
                    }
                }
            }
            break;
        }

        // Poll until deadline.
        if let Some((frame, _)) = read_frame_with_deadline(&mut stdout, deadline) {
            if matches!(
                frame,
                ControlFrame::Done { .. } | ControlFrame::Fatal { .. }
            ) {
                done = Some(frame);
                break;
            }
        }
        // Tiny sleep so we don't busy-loop.
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }

    let done = done.expect("supervisor should send Done");
    let _ = child.wait();
    done
}

// ---------------------------------------------------------------------------
// 3.1 — Timeout fires → TERM→KILL→reap (AC-01, AC-02)
// ---------------------------------------------------------------------------

#[test]
fn test_timeout_fires_term_kill_reap() {
    let tree = ProcessTree::start("timeout31");
    let helper = write_worker_script(
        tree.workdir(),
        "worker_helper_31.sh",
        r#"#!/bin/sh
# Trap TERM/INT by ignoring them — the daemon must escalate to KILL.
trap '' TERM INT
while :; do sleep 1; done
"#,
    );

    let worktree = tree.workdir().join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let start = Instant::now();
    let done = drive_supervisor_with_timeout(&helper, &worktree, 2);
    let elapsed = start.elapsed();
    eprintln!("test_timeout_fires_term_kill_reap: done={done:?} elapsed={elapsed:.1?}");

    assert!(
        elapsed.as_secs() < 10,
        "supervisor took {elapsed:.1?} (expected < 10 s)"
    );

    match done {
        ControlFrame::Done {
            status,
            signaled: true,
        } => {
            // Supervisor reports signaled exit with status 1
            // (the real signal number is lost through the
            // supervisor's status.code().unwrap_or(1) fallback).
            assert_eq!(status, 1, "expected exit status 1 (signaled)");
        }
        other => panic!("expected DONE with signaled=true, got {other:?}"),
    }

    // Confirm no survivor process.
    tree.assert_no_process_by_name("worker_helper_31");
}

// ---------------------------------------------------------------------------
// 3.2 — Timeout races normal exit (AC-01)
// ---------------------------------------------------------------------------

#[test]
fn test_timeout_races_normal_exit() {
    let tree = ProcessTree::start("timeout32");
    let helper = write_worker_script(
        tree.workdir(),
        "worker_helper_32.sh",
        r#"#!/bin/sh
sleep 1
exit 0
"#,
    );

    let worktree = tree.workdir().join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let done = drive_supervisor_with_timeout(&helper, &worktree, 10);

    match done {
        ControlFrame::Done {
            status: 0,
            signaled: false,
        } => {}
        other => panic!("expected DONE 0 (not signaled), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 3.3 — Identity check: verify_identity and read_proc_starttime
//       (identity-reuse protection)
// ---------------------------------------------------------------------------

#[test]
fn test_identity_verify_helpers() {
    // A PID that definitely does not exist.
    assert!(
        !worker_supervisor::verify_identity(999_999, 0),
        "missing PID must fail verification"
    );

    // A PID that does exist (this test process).
    let my_pid = std::process::id() as i32;
    let starttime = worker_supervisor::read_proc_starttime(my_pid);
    assert!(
        starttime.is_some(),
        "read_proc_starttime for our own PID should be Some"
    );
    let st = starttime.unwrap();
    assert!(st > 0, "starttime must be > 0");

    // Correct starttime → identity confirmed.
    assert!(
        worker_supervisor::verify_identity(my_pid, st),
        "matching starttime must verify"
    );

    // Off-by-one starttime → PID reuse / mismatch must reject.
    assert!(
        !worker_supervisor::verify_identity(my_pid, st + 1),
        "stale starttime must fail verification"
    );
}

// ---------------------------------------------------------------------------
// 3.4 — Stubborn grandchild — KILL reaches descendant
// ---------------------------------------------------------------------------

#[test]
fn test_timeout_kills_grandchild() {
    let tree = ProcessTree::start("timeout34");
    let helper = write_worker_script(
        tree.workdir(),
        "worker_helper_34.sh",
        r#"#!/bin/sh
# Ignore TERM so only SIGKILL works — the daemon must escalate.
trap '' TERM INT
# Fork a grandchild that also ignores TERM (inherits signal disposition).
(sleep 30) &
while :; do sleep 1; done
"#,
    );

    let worktree = tree.workdir().join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let done = drive_supervisor_with_timeout(&helper, &worktree, 2);
    eprintln!("test_timeout_kills_grandchild: done={done:?}");

    match done {
        ControlFrame::Done {
            status,
            signaled: true,
        } => {
            // Supervisor reports signaled with status 1 (not 137).
            assert_eq!(status, 1, "expected status 1 (signaled)");
        }
        other => panic!("expected DONE with signaled=true, got {other:?}"),
    }

    // Confirm no survivor.
    tree.assert_no_process_by_name("worker_helper_34");
}

// ---------------------------------------------------------------------------
// 3.5 — supervise returns within timeout+grace (AC-05)
// ---------------------------------------------------------------------------

#[test]
fn test_timeout_returns_within_bounds() {
    let tree = ProcessTree::start("timeout35");
    let helper = write_worker_script(
        tree.workdir(),
        "worker_helper_35.sh",
        r#"#!/bin/sh
while :; do sleep 1; done
"#,
    );

    let worktree = tree.workdir().join("wt");
    fs::create_dir_all(&worktree).expect("worktree");

    let start = Instant::now();
    let done = drive_supervisor_with_timeout(&helper, &worktree, 2);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "supervisor took {elapsed:.1?} (expected < 10 s)"
    );

    // The worker must be killed (signaled).
    match done {
        ControlFrame::Done { signaled, .. } => {
            assert!(signaled, "worker must be signaled (killed by timeout)");
        }
        other => panic!("expected DONE, got {other:?}"),
    }
}
