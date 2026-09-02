//! Asserts the ACK-gate contract of the worker supervisor: the
//! supervisor must not spawn the worker process until the daemon
//! has acknowledged the `READY(pgid)` frame.
//!
//! The assertion reads `/proc` via `collect_descendants`, so this
//! test is Linux-only (mirroring the `#[cfg(target_os = "linux")]`
//! gates on the `/proc`-dependent helpers in the supervisor).

#![cfg(target_os = "linux")]

use caduceus::worker_supervisor::{collect_descendants, decode_frame, encode_frame, ControlFrame};
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn write_script(path: &PathBuf, body: &str) {
    fs::write(path, body).expect("write script");
    let mut mode = fs::metadata(path).expect("stat").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).expect("chmod");
}

fn find_self_exe() -> PathBuf {
    fixtures::ReleaseBinary::locate()
}

#[test]
fn no_child_pid_before_ack() {
    let dir = tempdir("ackgate");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    // A long-lived worker so the descendant is observable.
    write_script(&helper, "#!/bin/sh\nsleep 30\n");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(&worktree);
    cmd.arg("--run-id").arg("RUNACK");
    cmd.arg("--issue").arg("owner/repo#7");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(&transcript);
    cmd.arg("--heartbeat").arg(&heartbeat);
    cmd.arg("--timeout").arg("60");
    cmd.arg("--transcript-max-bytes").arg("1048576");
    cmd.arg("--").arg(&helper);
    cmd.env_clear();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn supervisor");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let supervisor_pid = child.id() as i32;

    // Read the `Ready` frame.
    let mut header = [0u8; 4];
    stdout.read_exact(&mut header).expect("ready header");
    let len = u32::from_le_bytes(header) as usize;
    let mut ready_body = vec![0u8; 4 + len];
    ready_body[..4].copy_from_slice(&header);
    stdout.read_exact(&mut ready_body[4..]).expect("ready body");
    let (ready, _) = decode_frame(&ready_body).expect("decode ready");
    assert!(
        matches!(ready, ControlFrame::Ready { .. }),
        "expected READY first, got {ready:?}"
    );

    // Before ACK: no worker may exist yet.
    assert!(
        collect_descendants(supervisor_pid).is_empty(),
        "supervisor spawned a child before the daemon ACKed"
    );

    // Send ACK; the worker must now appear.
    let ack = encode_frame(&ControlFrame::Ack).expect("encode ack");
    stdin.write_all(&ack).expect("ack write");
    stdin.flush().ok();

    let start = Instant::now();
    loop {
        if !collect_descendants(supervisor_pid).is_empty() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "worker never spawned after ACK"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Send TERM (graceful), expect a DONE frame, then reap.
    let term = encode_frame(&ControlFrame::Terminate { force: false }).expect("encode term");
    stdin.write_all(&term).expect("term write");
    stdin.flush().ok();

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut done = None;
    while Instant::now() < deadline {
        let mut h = [0u8; 4];
        match stdout.read(&mut h) {
            Ok(0) => break,
            Ok(_) => {
                let l = u32::from_le_bytes(h) as usize;
                let mut body = vec![0u8; 4 + l];
                body[..4].copy_from_slice(&h);
                stdout.read_exact(&mut body[4..]).expect("done body");
                let (frame, _) = decode_frame(&body).expect("decode frame");
                if matches!(frame, ControlFrame::Done { .. }) {
                    done = Some(frame);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(done.is_some(), "supervisor should send DONE after TERM");
    let _ = child.wait();
}
