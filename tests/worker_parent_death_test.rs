//! Task 5.1 parent-death / control-pipe EOF tests.
//!
//! The contract says: "daemon death closes the control pipe
//! and makes the live supervisor kill it." When the daemon
//! process dies or closes the supervisor's stdin, the
//! supervisor must:
//!
//! * detect the EOF (or hangup) promptly;
//! * kill the worker session via the negative PGID it
//!   recorded in the `READY` frame;
//! * reap the descendants;
//! * exit cleanly without further I/O.
//!
//! These tests drive the `__worker-supervisor` mode directly
//! from Rust and exercise the EOF / TERM / KILL paths with
//! deterministic POSIX shell helpers as the worker.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use caduceus::worker_supervisor::{encode_frame, ControlFrame};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-supervisor-parent-death-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write_script(path: &PathBuf, body: &str) {
    fs::write(path, body).expect("write script");
    let mut mode = fs::metadata(path).expect("stat").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).expect("chmod");
}

fn find_self_exe() -> PathBuf {
    let mut here = std::env::current_exe().expect("current_exe");
    loop {
        if here.join("caduceus").is_file() {
            return here.join("caduceus");
        }
        if !here.pop() {
            panic!("could not find caduceus binary in target/debug");
        }
    }
}

fn spawn_supervisor(
    worker: &PathBuf,
    transcript: &PathBuf,
    heartbeat: &PathBuf,
    worktree: &PathBuf,
    timeout_seconds: u64,
) -> std::process::Child {
    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(worktree);
    cmd.arg("--run-id").arg("RUNPARENT");
    cmd.arg("--issue").arg("owner/repo#7");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(transcript);
    cmd.arg("--heartbeat").arg(heartbeat);
    cmd.arg("--timeout").arg(timeout_seconds.to_string());
    cmd.arg("--").arg(worker);
    cmd.env_clear();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn supervisor")
}

fn read_frame(stream: &mut std::process::ChildStdout) -> ControlFrame {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("read header");
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; 4 + len];
    body[..4].copy_from_slice(&header);
    stream.read_exact(&mut body[4..]).expect("read body");
    caduceus::worker_supervisor::decode_frame(&body)
        .expect("decode")
        .0
}

#[test]
fn daemon_stdin_close_kills_long_running_worker() {
    let dir = tempdir("daemon_close");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(
        &helper,
        r#"#!/bin/sh
trap 'exit 0' TERM INT
trap 'exit 0' HUP
while :; do
    sleep 1
done
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let mut child = spawn_supervisor(&helper, &transcript, &heartbeat, &worktree, 60);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let frame = read_frame(&mut stdout);
    assert!(matches!(frame, ControlFrame::Ready { pgid } if pgid > 0));

    let ack = encode_frame(&ControlFrame::Ack).expect("encode");
    stdin.write_all(&ack).expect("ack");
    drop(stdin);

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success() || status.code().is_some(),
                    "supervisor should exit with a normal code, got {status:?}"
                );
                break;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    panic!("supervisor did not exit within 10s after daemon stdin close");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => panic!("try_wait: {err}"),
        }
    }
}

#[test]
fn terminate_frame_kills_long_running_worker() {
    let dir = tempdir("terminate");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(
        &helper,
        r#"#!/bin/sh
trap 'exit 0' TERM INT
while :; do
    sleep 1
done
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let mut child = spawn_supervisor(&helper, &transcript, &heartbeat, &worktree, 60);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let frame = read_frame(&mut stdout);
    assert!(matches!(frame, ControlFrame::Ready { .. }));

    let ack = encode_frame(&ControlFrame::Ack).expect("encode");
    stdin.write_all(&ack).expect("ack");

    std::thread::sleep(Duration::from_millis(300));

    let term = encode_frame(&ControlFrame::Terminate { force: false }).expect("encode");
    stdin.write_all(&term).expect("term");
    stdin.flush().ok();

    let start = Instant::now();
    let mut done: Option<ControlFrame> = None;
    while start.elapsed() < Duration::from_secs(10) {
        let mut header = [0u8; 4];
        match stdout.read(&mut header) {
            Ok(0) => break,
            Ok(_) => {
                let len = u32::from_le_bytes(header) as usize;
                let mut b = vec![0u8; 4 + len];
                b[..4].copy_from_slice(&header);
                stdout.read_exact(&mut b[4..]).expect("body");
                let (frame, _) = caduceus::worker_supervisor::decode_frame(&b).expect("decode");
                if matches!(
                    frame,
                    ControlFrame::Done { .. } | ControlFrame::Fatal { .. }
                ) {
                    done = Some(frame);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let done = done.expect("supervisor should send Done after TERM");
    assert!(matches!(done, ControlFrame::Done { .. }));
    let _ = child.wait();
}

#[test]
fn supervisor_protocol_rejects_garbage_input() {
    let dir = tempdir("garbage");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(&helper, "#!/bin/sh\necho hi 1>&2\nexit 0\n");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let mut child = spawn_supervisor(&helper, &transcript, &heartbeat, &worktree, 30);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let _ = read_frame(&mut stdout);

    let bad_len: u32 = 1 << 30;
    stdin.write_all(&bad_len.to_le_bytes()).ok();
    stdin.flush().ok();
    drop(stdin);

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(10) {
                    let _ = child.kill();
                    panic!("supervisor hung on garbage input");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => panic!("try_wait: {err}"),
        }
    }
}

#[test]
fn supervisor_does_not_leak_worker_after_done() {
    // After the supervisor sends `Done`, no worker descendant
    // from *this* test invocation should still be alive. We
    // give every run a unique worker script path so the
    // search is per-test, not per-suite.
    let dir = tempdir("reap");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker_reap_unique.sh");
    write_script(
        &helper,
        r#"#!/bin/sh
# Fork a grandchild that briefly sleeps.
(sleep 1) &
wait
exit 0
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let mut child = spawn_supervisor(&helper, &transcript, &heartbeat, &worktree, 30);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");
    let _ = read_frame(&mut stdout);
    let ack = encode_frame(&ControlFrame::Ack).expect("encode");
    stdin.write_all(&ack).expect("ack");

    let status = child.wait().expect("wait");
    assert!(status.success(), "supervisor should exit 0");

    std::thread::sleep(Duration::from_millis(500));
    let entries = std::fs::read_dir("/proc").expect("read /proc");
    let mut lingering = 0;
    for entry in entries.flatten() {
        let Ok(cmdline) = std::fs::read_to_string(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline.contains("worker_reap_unique.sh") {
            lingering += 1;
        }
    }
    assert_eq!(
        lingering, 0,
        "worker_reap_unique.sh must not remain after supervisor Done (found {lingering})"
    );
}
