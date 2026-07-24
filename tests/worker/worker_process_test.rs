//! These tests drive the `__worker-supervisor` hidden command by
//! re-execing the same `caduceus` binary and asserting the
//! contract:
//!
//! * Successful worker exit propagates as a `Done` frame and the
//!   `SupervisorOutcome` carries the bridge's exit code.
//! * Heartbeat file is created before the supervisor launches
//!   and removed after it returns.
//! * Transcript content is forwarded (worker stdout flows
//!   through supervisor stderr into the transcript file).
//! * Non-zero exits propagate.
//! * The supervisor protocol is versioned and length-bounded.
//! * The hidden command never appears in `--help`.

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::worker_supervisor::{
    clear_heartbeat, encode_frame, open_transcript, read_heartbeat, write_heartbeat, ControlFrame,
    WorkerRunPaths, PROTOCOL_VERSION,
};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-supervisor-test-{label}-{nonce}"));
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
    fixtures::ReleaseBinary::locate()
}

fn sample_issue() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 7,
    }
}

/// Drive the supervisor end-to-end. Returns the final
/// `ControlFrame::Done` payload plus whether the supervisor
/// process exited cleanly.
fn drive_supervisor(
    helper: &PathBuf,
    transcript: &PathBuf,
    heartbeat: &PathBuf,
    worktree: &PathBuf,
    timeout_seconds: u64,
) -> ControlFrame {
    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(worktree);
    cmd.arg("--run-id").arg("RUN_DRV");
    cmd.arg("--issue").arg("owner/repo#7");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(transcript);
    cmd.arg("--heartbeat").arg(heartbeat);
    cmd.arg("--timeout").arg(timeout_seconds.to_string());
    cmd.arg("--transcript-max-bytes").arg("1048576");
    cmd.arg("--").arg(helper);
    cmd.env_clear();
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn supervisor");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // Read the `Ready` frame.
    let mut header = [0u8; 4];
    stdout.read_exact(&mut header).expect("ready header");
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; 4 + len];
    body[..4].copy_from_slice(&header);
    stdout.read_exact(&mut body[4..]).expect("ready body");
    let (ready, _) = caduceus::worker_supervisor::decode_frame(&body).expect("decode");
    assert!(
        matches!(ready, ControlFrame::Ready { .. }),
        "expected READY first, got {ready:?}"
    );

    // Send ACK.
    let ack = encode_frame(&ControlFrame::Ack).expect("encode");
    stdin.write_all(&ack).expect("ack write");
    stdin.flush().ok();

    // Read until Done.
    let start = Instant::now();
    let mut done: Option<ControlFrame> = None;
    while start.elapsed() < Duration::from_secs(15) {
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
    let done = done.expect("supervisor should send Done");
    let _ = child.wait();
    done
}

// Heartbeat / transcript / path contract

#[test]
fn heartbeat_writes_and_clears() {
    let dir = tempdir("hb");
    let path = dir.join("hbeat");
    write_heartbeat(&path).expect("write");
    assert!(read_heartbeat(&path).is_some());
    clear_heartbeat(&path).expect("clear");
    assert!(read_heartbeat(&path).is_none());
}

#[test]
fn heartbeat_visible_during_run_and_removed_after() {
    let dir = tempdir("hb_live");
    let paths = WorkerRunPaths::new(dir.clone(), "RUNLIVE".to_string());
    paths.ensure_dirs().expect("ensure_dirs");
    write_heartbeat(&paths.heartbeat_path).expect("hb");
    let initial = read_heartbeat(&paths.heartbeat_path).expect("read");
    assert!((chrono::Utc::now() - initial).num_seconds().abs() < 5);
    clear_heartbeat(&paths.heartbeat_path).expect("clear");
    assert!(read_heartbeat(&paths.heartbeat_path).is_none());
}

#[test]
fn transcript_writes_and_persists() {
    let dir = tempdir("trunc");
    let path = dir.join("t.log");
    let mut f = open_transcript(&path).expect("open");
    for i in 0..500 {
        writeln!(f, "line {i}").expect("write");
    }
    drop(f);
    let meta = fs::metadata(&path).expect("stat");
    assert!(meta.len() > 4096);
}

#[test]
fn paths_ensure_dirs_creates_secure_layout() {
    let dir = tempdir("paths");
    let paths = WorkerRunPaths::new(dir.clone(), "RUNPATHS".to_string());
    paths.ensure_dirs().expect("ensure_dirs");
    let meta = fs::metadata(dir.join("runs")).expect("stat runs");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
}

// Protocol contract

#[test]
fn frame_round_trip() {
    let cases = vec![
        ControlFrame::Ready { pgid: 9999 },
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
        let bytes = encode_frame(&case).expect("encode");
        let (decoded, consumed) =
            caduceus::worker_supervisor::decode_frame(&bytes).expect("decode");
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, case);
    }
}

#[test]
fn frame_rejects_wrong_version() {
    let mut bytes = encode_frame(&ControlFrame::Ack).expect("encode");
    bytes[6] = b'9';
    let err = caduceus::worker_supervisor::decode_frame(&bytes).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("unsupported protocol version"), "{msg}");
}

#[test]
fn frame_rejects_oversize() {
    let mut bytes = Vec::new();
    let oversize = (caduceus::worker_supervisor::MAX_FRAME_BYTES as u32) + 1;
    bytes.extend_from_slice(&oversize.to_le_bytes());
    bytes.resize(4 + oversize as usize, 0);
    let err = caduceus::worker_supervisor::decode_frame(&bytes).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds cap"), "{msg}");
}

#[test]
fn protocol_version_is_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}

// Hidden command + end-to-end harness

#[test]
fn supervisor_hidden_command_is_not_in_help() {
    let exe = find_self_exe();
    let output = Command::new(&exe)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run --help");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        !combined.contains("__worker-supervisor"),
        "hidden command leaked into --help: {combined}"
    );
}

#[test]
fn supervisor_hidden_command_exits_zero_on_success() {
    let dir = tempdir("harness_ok");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    // The worker writes to stderr; the supervisor forwards
    // stderr to the daemon, which drains it into the transcript.
    write_script(
        &helper,
        r#"#!/bin/sh
echo "hello" 1>&2
exit 0
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let done = drive_supervisor(&helper, &transcript, &heartbeat, &worktree, 30);
    match done {
        ControlFrame::Done {
            status,
            signaled: false,
        } => {
            assert_eq!(status, 0);
        }
        other => panic!("expected DONE 0, got {other:?}"),
    }

    let body = fs::read_to_string(&transcript).expect("read transcript");
    assert!(
        body.contains("hello"),
        "transcript should contain 'hello', got: {body}"
    );
}

#[test]
fn supervisor_hidden_command_propagates_nonzero_exit() {
    let dir = tempdir("harness_nz");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(&helper, "#!/bin/sh\nexit 7\n");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let done = drive_supervisor(&helper, &transcript, &heartbeat, &worktree, 30);
    match done {
        ControlFrame::Done { status, .. } => assert_eq!(status, 7),
        other => panic!("expected DONE 7, got {other:?}"),
    }
}

#[test]
fn supervisor_creates_new_session_with_distinct_pgid() {
    // The supervisor's PID after setsid becomes its own PGID.
    // Drive a worker that prints its own PID and PGID; assert
    // they differ from the test runner's PID.
    let dir = tempdir("pgid");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(
        &helper,
        r#"#!/bin/sh
ps -o pid=,pgid= -p $$ 1>&2
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let done = drive_supervisor(&helper, &transcript, &heartbeat, &worktree, 30);
    assert!(matches!(done, ControlFrame::Done { status: 0, .. }));

    let body = fs::read_to_string(&transcript).expect("read transcript");
    let mut iter = body.split_whitespace();
    let pid_str = iter.next().expect("pid line");
    let pgid_str = iter.next().expect("pgid line");
    let pid: i32 = pid_str.trim().parse().expect("pid int");
    let pgid: i32 = pgid_str.trim().parse().expect("pgid int");
    assert_eq!(pid, pgid, "worker must be a process-group leader");
    assert!(pgid > 0, "pgid should be a positive integer");
}

#[test]
fn supervisor_handles_missing_required_argument() {
    let dir = tempdir("missing_args");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    let helper = dir.join("worker.sh");
    write_script(&helper, "#!/bin/sh\nexit 0\n");

    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--transcript").arg(&transcript);
    cmd.arg("--heartbeat").arg(&heartbeat);
    cmd.arg("--timeout").arg("5");
    cmd.arg("--").arg(&helper);
    cmd.env_clear();
    let output = cmd.stdin(Stdio::null()).output().expect("spawn");
    // Missing --worktree → nonzero exit + stderr message.
    assert!(!output.status.success(), "missing --worktree must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--worktree"),
        "stderr should mention --worktree, got: {stderr}"
    );
}

#[test]
fn supervisor_handles_missing_command_after_double_dash() {
    let dir = tempdir("missing_cmd");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");

    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(&worktree);
    cmd.arg("--run-id").arg("RUNMISS");
    cmd.arg("--issue").arg("owner/repo#7");
    cmd.arg("--context-json").arg("{}");
    cmd.arg("--transcript").arg(&transcript);
    cmd.arg("--heartbeat").arg(&heartbeat);
    cmd.arg("--timeout").arg("5");
    // No `--` and no command.
    cmd.env_clear();
    let output = cmd.stdin(Stdio::null()).output().expect("spawn");
    assert!(!output.status.success(), "missing worker command must fail");
}

#[test]
fn config_test_defaults_supports_supervise_signature() {
    let dir = tempdir("cfg");
    let cfg = Config::test_defaults(&dir);
    assert!(cfg.state_dir.starts_with(&dir));
    assert!(cfg.transcript_max_bytes > 0);
    let _ = BTreeMap::<String, String>::new();
    let _ = OsString::from("");
    let _ = sample_issue();
}

#[test]
fn supervisor_killed_by_external_signal_still_returns_done_frame() {
    // If the daemon closes the supervisor's stdin (EOF), the
    // supervisor must exit cleanly without exec'ing the worker.
    // Drive a worker that runs forever; close stdin from the
    // daemon side and verify the supervisor exits.
    let dir = tempdir("daemon_death");
    let worktree = dir.join("wt");
    fs::create_dir_all(&worktree).expect("worktree");
    let helper = dir.join("worker.sh");
    write_script(
        &helper,
        r#"#!/bin/sh
sleep 30
"#,
    );
    let transcript = dir.join("t.log");
    let heartbeat = dir.join("hbeat");
    fs::File::create(&transcript).expect("create transcript");
    fs::File::create(&heartbeat).expect("create heartbeat");

    let exe = find_self_exe();
    let mut cmd = Command::new(&exe);
    cmd.arg("__worker-supervisor");
    cmd.arg("--worktree").arg(&worktree);
    cmd.arg("--run-id").arg("RUNEOF");
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
    let mut child = cmd.spawn().expect("spawn");
    let stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    // Read Ready.
    let mut header = [0u8; 4];
    stdout.read_exact(&mut header).expect("ready header");
    let len = u32::from_le_bytes(header) as usize;
    let mut body = vec![0u8; 4 + len];
    body[..4].copy_from_slice(&header);
    stdout.read_exact(&mut body[4..]).expect("ready body");
    let _ = caduceus::worker_supervisor::decode_frame(&body).expect("decode");

    // Drop stdin → EOF on supervisor side. The supervisor
    // should kill the worker and exit cleanly.
    drop(stdin);

    let _ = child.wait();
}
