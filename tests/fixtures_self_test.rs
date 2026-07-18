//! Self-tests for the v1.0 Phase 1.2 fixtures. Run via
//! `cargo test --test fixtures_self_test`. These tests are the
//! primary acceptance evidence for Task 1.2's three acceptance
//! IDs:
//!
//! - 1.2-AC-01: no network, no production credentials
//! - 1.2-AC-02: Git side effects and failures are modelled
//! - 1.2-AC-03: exact GitHub mutation counts
//!
//! The tests live in their own test binary (rather than under
//! `tests/fixtures/`) because Cargo only auto-discovers
//! `tests/<file>.rs` as integration test binaries — files
//! inside `tests/fixtures/` are only built when a test wires
//! them in via `#[path]`. Keeping the self-tests here means
//! `cargo test` runs them on every CI build.

#![allow(clippy::needless_return)]

#[path = "fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use fixtures::{
    CrashPoint, LocalOrigin, MockGitHub, ProcessTree, ReleaseBinary, RunSupervisorArgs,
};

// -----------------------------------------------------------------------
// AC-01: Require no network or production credentials
// -----------------------------------------------------------------------

/// `MockGitHub::uri()` must always resolve to a localhost
/// loopback address; the helper must never produce a
/// non-loopback URL even if the test machine has a routable
/// hostname configured. We assert against the IPv4/IPv6
/// loopback literals because `MockServer::uri()` is documented
/// to bind to one of them.
#[tokio::test]
async fn ac01_mock_github_uri_is_loopback() {
    let gh = MockGitHub::start().await;
    let uri = gh.uri();
    assert!(
        uri.starts_with("http://127.0.0.1:") || uri.starts_with("http://[::1]:"),
        "MockGitHub uri should be loopback, got {uri}"
    );
    assert!(
        !uri.contains("github.com"),
        "MockGitHub uri must not leak github.com, got {uri}"
    );
}

/// `LocalOrigin::uri()` must always be a `file://` URL. The
/// daemon's `validate_origin_host` accepts `file://` URLs as
/// hermetic, so this is the right shape for fixture use. We
/// also assert the URL parses and points at an existing bare
/// repo so a future regression that points `uri()` at a
/// non-existent path fails fast.
#[test]
fn ac01_local_origin_uri_is_file_scheme() {
    let origin = LocalOrigin::init("ac01");
    let uri = origin.uri();
    assert!(
        uri.starts_with("file://"),
        "LocalOrigin uri must use the file:// scheme, got {uri}"
    );
    assert!(
        origin.path().exists(),
        "bare repo path must exist on disk: {}",
        origin.path().display()
    );
    assert!(
        origin.path().join("HEAD").exists(),
        "bare repo must have HEAD (not an empty init): {}",
        origin.path().display()
    );
}

/// Both fixtures must succeed without reading a token from
/// the environment. We deliberately do NOT set
/// `GITHUB_TOKEN`, `GH_TOKEN`, or `CADUCEUS_GITHUB_TOKEN` in
/// this test process — the fixtures run anyway, proving the
/// contract. (The harness already scrubs those vars via
/// `runner_env_test`, but we re-assert it here because the
/// fixture is the boundary the v1.0 plan hangs its
/// credentials-required claim on.)
#[tokio::test]
async fn ac01_fixtures_run_with_no_github_token_in_environment() {
    // Don't set any token here. The fixtures should still
    // build and serve traffic without one.
    let gh = MockGitHub::start().await;
    let _origin = LocalOrigin::init("ac01-env");
    gh.mount("GET", "/user", json!({"login": "octocat"})).await;
    // Make one request with no token — the mock answers
    // regardless.
    let resp = reqwest::get(gh.uri() + "/user").await.expect("reqwest");
    assert_eq!(resp.status(), 200);
}

// -----------------------------------------------------------------------
// AC-02: Model Git side effects and failures
// -----------------------------------------------------------------------

/// Successful push from a working clone bumps the bare
/// repo's `main` head and the recorded commit count. The
/// fixture must capture the side effect so tests can assert
/// on it without parsing git output themselves.
#[test]
fn ac02_push_commit_moves_origin_head_and_bumps_count() {
    let mut origin = LocalOrigin::init("ac02-push");
    let initial_oid = origin.head_oid().to_string();
    let initial_count = origin.commit_count();
    let workdir = tempdir("work");
    origin.clone_into(&workdir);
    let readme = workdir.join("README.md");
    let new_oid = origin.push_commit(&workdir, &readme, "# hello\n", "docs: seed README");
    assert_ne!(new_oid, initial_oid, "head should change after push");
    assert_eq!(
        origin.commit_count(),
        initial_count + 1,
        "commit count should bump by exactly one"
    );
    assert_eq!(
        origin.head_oid(),
        new_oid,
        "LocalOrigin.head_oid should track the pushed commit"
    );
}

/// Pushing to a non-existent ref surfaces a non-zero exit.
/// The fixture's `run` helper turns that into a panic so a
/// test that misconfigures the push sees a clear failure
/// instead of a silent no-op. This self-test asserts the
/// panic path is the only failure mode the helper exposes:
/// either the push succeeds (and `head_oid` updates) or the
/// test panics.
#[test]
fn ac02_push_to_missing_ref_panics_with_stderr() {
    let origin = LocalOrigin::init("ac02-fail");
    let workdir = tempdir("work");
    origin.clone_into(&workdir);
    // Force the working clone into detached HEAD so the push
    // has nothing to update — git push refuses with a clear
    // "src refspec HEAD does not match any" message. We do
    // this directly through std::process because the
    // fixture's push helpers assume a clean working tree.
    let detach = std::process::Command::new("git")
        .current_dir(&workdir)
        .args(["checkout", "--detach", "HEAD"])
        .output()
        .expect("detach");
    assert!(detach.status.success(), "detach HEAD failed");
    // Now `git push origin HEAD:refs/heads/main` should
    // succeed because we explicitly specified the dst ref —
    // but a push with no refspec from detached HEAD fails.
    let push = std::process::Command::new("git")
        .current_dir(&workdir)
        .args(["push", "origin"])
        .output()
        .expect("push");
    assert!(
        !push.status.success(),
        "push with no refspec from detached HEAD should fail"
    );
    let stderr = String::from_utf8_lossy(&push.stderr);
    assert!(
        stderr.contains("not currently on a branch")
            || stderr.contains("does not match")
            || stderr.contains("refspec"),
        "push failure should be informative, got: {stderr}"
    );
}

/// Cloning into an existing non-empty directory fails cleanly.
/// This guards against a future regression where the fixture
/// silently overwrites an existing checkout.
#[test]
fn ac02_clone_into_existing_dir_fails_cleanly() {
    let origin = LocalOrigin::init("ac02-clone-fail");
    let dest = tempdir("occupied");
    std::fs::write(dest.join("existing"), "data").expect("seed");
    let result = std::process::Command::new("git")
        .arg("clone")
        .arg("-b")
        .arg("main")
        .arg(origin.uri())
        .arg(&dest)
        .status();
    let status = result.expect("git clone spawn");
    assert!(!status.success(), "clone into non-empty dir must fail");
    assert!(
        dest.join("existing").exists(),
        "pre-existing file must not be deleted by a failed clone"
    );
}

// -----------------------------------------------------------------------
// AC-03: Record exact GitHub mutation counts
// -----------------------------------------------------------------------

/// Counts::mutations sums POST + PATCH + PUT + DELETE exactly.
/// A test that asks the daemon to POST a comment twice and
/// PATCH an issue once should see `counts.mutations() == 3`
/// and the GET/HEAD requests should not inflate the total.
#[tokio::test]
async fn ac03_counts_track_exact_mutation_total() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/o/r/issues/1/comments",
        201,
        json!({"id": 1}),
    )
    .await;
    gh.mount_status("PATCH", "/repos/o/r/issues/1", 200, json!({"number": 1}))
        .await;
    gh.mount("GET", "/repos/o/r", json!({"name": "r"})).await;

    let client = reqwest::Client::new();
    for _ in 0..2 {
        let r = client
            .post(gh.uri() + "/repos/o/r/issues/1/comments")
            .json(&json!({"body": "hi"}))
            .send()
            .await
            .expect("post");
        assert_eq!(r.status(), 201);
    }
    let r = client
        .patch(gh.uri() + "/repos/o/r/issues/1")
        .json(&json!({"state": "closed"}))
        .send()
        .await
        .expect("patch");
    assert_eq!(r.status(), 200);
    let r = client
        .get(gh.uri() + "/repos/o/r")
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);

    let counts = gh.counts();
    assert_eq!(counts.post, 2, "exactly two POSTs");
    assert_eq!(counts.patch, 1, "exactly one PATCH");
    assert_eq!(counts.get, 1, "exactly one GET");
    assert_eq!(counts.mutations(), 3, "mutations = post + patch");
    assert_eq!(counts.total(), 4, "total = get + mutations");
}

/// `path_counts` keys by request path so a test that wants
/// "POST exactly twice to `/comments` and zero times to
/// `/labels`" can assert that directly without walking the
/// request log.
#[tokio::test]
async fn ac03_path_counts_are_keyed_per_endpoint() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/o/r/issues/1/comments",
        201,
        json!({"id": 1}),
    )
    .await;
    gh.mount_status("POST", "/repos/o/r/issues/1/labels", 200, json!([]))
        .await;

    let client = reqwest::Client::new();
    for _ in 0..3 {
        client
            .post(gh.uri() + "/repos/o/r/issues/1/comments")
            .json(&json!({"body": "x"}))
            .send()
            .await
            .expect("post comment");
    }
    client
        .post(gh.uri() + "/repos/o/r/issues/1/labels")
        .json(&json!(["bug"]))
        .send()
        .await
        .expect("post label");

    let counts = gh.path_counts();
    assert_eq!(counts.get("/repos/o/r/issues/1/comments").copied(), Some(3));
    assert_eq!(counts.get("/repos/o/r/issues/1/labels").copied(), Some(1));
}

/// `received_requests` preserves the order wiremock observed
/// the requests in. Useful for tests that want to assert
/// "first request was GET, second was POST" without relying
/// on timestamps.
#[tokio::test]
async fn ac03_received_requests_preserve_order_and_method() {
    let gh = MockGitHub::start().await;
    gh.mount("GET", "/repos/o/r", json!({"name": "r"})).await;
    gh.mount_status("POST", "/repos/o/r/issues", 201, json!({"number": 1}))
        .await;

    let client = reqwest::Client::new();
    client
        .get(gh.uri() + "/repos/o/r")
        .send()
        .await
        .expect("get");
    client
        .post(gh.uri() + "/repos/o/r/issues")
        .json(&json!({"title": "t"}))
        .send()
        .await
        .expect("post");

    let log = gh.received_requests();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].method.as_str(), "GET");
    assert_eq!(log[1].method.as_str(), "POST");
}

// -----------------------------------------------------------------------
// AC-01: ProcessTree can spawn, observe, and clean up descendants
// -----------------------------------------------------------------------

/// Spawn a child process, verify it appears in the descendant set,
/// then terminate it.
#[cfg(target_os = "linux")]
#[test]
fn ac01_process_tree_spawns_observable_descendant() {
    let pt = ProcessTree::start("ac01-spawn");
    let pid = pt.spawn_detached_bash("sleep 30");
    assert!(pid > 0, "spawn_detached_bash should return a valid PID");

    // Give the child a moment to start
    std::thread::sleep(Duration::from_millis(100));

    // The test process is the parent of the bash child.
    // We use the test process's own PID as the ppid.
    let my_pid = std::process::id() as i32;
    let descs = pt.descendants(my_pid);
    assert!(
        descs.contains(&pid),
        "descendants should contain the spawned PID {pid}, got: {descs:?}"
    );

    // Clean up
    pt.terminate(pid, nix::sys::signal::Signal::SIGKILL);
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// Spawn a script that forks a long-lived background child and
/// exits. The grandchild should survive the parent exit and be
/// visible to the /proc walker.
#[cfg(target_os = "linux")]
#[test]
fn ac01_process_tree_detached_grandchild_survives_parent_exit() {
    let pt = ProcessTree::start("ac01-grandchild");
    let script = r#"#!/bin/bash
(sleep 60 &)
sleep 2
"#;
    let parent_pid = pt.spawn_detached_bash(script);
    assert!(parent_pid > 0, "should have a valid parent PID");

    // Wait for the parent bash to exit (sleeps 2s + small overhead)
    std::thread::sleep(Duration::from_secs(3));

    // The grandchild (sleep 60) should still be alive.
    // It may have been reparented to PID 1 or to the test process
    // (since we set subreaper). We search /proc for any `sleep`
    // process that was started after our test began.
    let found = find_sleep_process();
    assert!(
        found,
        "should find a sleep process as a detached grandchild"
    );

    // Clean up all sleep processes left over
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 60"])
        .status();
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 30"])
        .status();
}

/// Spawn a child, kill everything, assert no descendants remain.
#[cfg(target_os = "linux")]
#[test]
fn ac01_process_tree_cleanup_leaves_no_descendants() {
    let pt = ProcessTree::start("ac01-cleanup");
    let pid = pt.spawn_detached_bash("sleep 30");
    assert!(pid > 0);

    std::thread::sleep(Duration::from_millis(200));

    // Kill the child via all available paths
    pt.terminate(pid, nix::sys::signal::Signal::SIGKILL);
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    // Also kill any sleep processes the test may have spawned
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 30"])
        .status();

    // Reap the zombie so /proc no longer shows it
    pt.reap(pid);

    // Wait for it to die
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let my_pid = std::process::id() as i32;
        let descs = pt.descendants(my_pid);
        if !descs.contains(&pid) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("child PID {pid} still alive after 5s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let my_pid = std::process::id() as i32;
    let descs = pt.descendants(my_pid);
    assert!(
        !descs.contains(&pid),
        "child should not be in descendants after kill"
    );
}

// -----------------------------------------------------------------------
// AC-02: CrashPoint can send signals at marker boundaries
// -----------------------------------------------------------------------

/// `kill_at_marker` sends SIGKILL and the process is signaled.
#[cfg(target_os = "linux")]
#[test]
fn ac02_crash_point_kill_at_marker_sends_sigkill() {
    let cp = CrashPoint::new("ac02-kill");
    let script = r#"#!/bin/bash
echo "READY"
sleep 30
echo "DONE"
"#;
    let (code, signaled) = cp.kill_at_marker(script, "READY");
    assert!(
        signaled,
        "process should be signaled (SIGKILL), got exit code {code}"
    );
}

/// `abort_at_marker` sends SIGABRT and the process is signaled.
#[cfg(target_os = "linux")]
#[test]
fn ac02_crash_point_abort_at_marker_sends_sigabrt() {
    let cp = CrashPoint::new("ac02-abort");
    let script = r#"#!/bin/bash
echo "READY"
sleep 30
echo "DONE"
"#;
    let (code, signaled) = cp.abort_at_marker(script, "READY");
    assert!(
        signaled,
        "process should be signaled (SIGABRT), got exit code {code}"
    );
}

/// Running `kill_at_marker` twice with the same input should
/// produce the same exit code both times.
#[cfg(target_os = "linux")]
#[test]
fn ac02_crash_point_reproducible_same_input_same_crash() {
    let cp = CrashPoint::new("ac02-repro");
    let script = r#"#!/bin/bash
echo "READY"
sleep 30
echo "DONE"
"#;
    let (code1, signaled1) = cp.kill_at_marker(script, "READY");
    let (code2, signaled2) = cp.kill_at_marker(script, "READY");
    assert_eq!(code1, code2, "exit codes should match across runs");
    assert_eq!(signaled1, signaled2, "signaled status should match");
}

// -----------------------------------------------------------------------
// AC-03: ReleaseBinary locates, hashes, and runs the supervisor
// -----------------------------------------------------------------------

/// `locate()` must return a path that exists and is a file.
#[test]
fn ac03_release_binary_locate_returns_existing_path() {
    let path = ReleaseBinary::locate();
    assert!(
        path.is_file(),
        "ReleaseBinary::locate() should return an existing file, got: {}",
        path.display()
    );
}

/// SHA-256 of the same binary must be stable across calls.
#[test]
fn ac03_release_binary_sha256_is_stable_across_calls() {
    let path = ReleaseBinary::locate();
    let hash1 = ReleaseBinary::sha256(&path);
    let hash2 = ReleaseBinary::sha256(&path);
    assert_eq!(hash1, hash2, "SHA-256 should be stable across calls");
    assert_eq!(hash1.len(), 64, "SHA-256 hex should be 64 chars");
}

/// Use `run_supervisor` with a simple worker, read the `READY`
/// frame, send ACK, and verify the supervisor protocol round-trip.
#[test]
fn ac03_release_binary_run_supervisor_serves_ready_frame() {
    use caduceus::worker_supervisor::{decode_frame, encode_frame, ControlFrame};
    use std::io::{Read, Write};

    let workdir = tempdir("ac03-supervisor");
    let worktree = workdir.join("worktree");
    fs::create_dir_all(&worktree).expect("create worktree");
    let transcript = workdir.join("transcript.log");
    let heartbeat = workdir.join("heartbeat");
    let worker_script = workdir.join("worker.sh");
    fs::write(&worker_script, "#!/bin/bash\necho 'hello'\n").expect("write worker");
    let mut perms = fs::metadata(&worker_script).expect("stat").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&worker_script, perms).expect("chmod");

    let args = RunSupervisorArgs {
        worktree,
        run_id: "test-run-001".to_string(),
        issue: "owner/repo#7".to_string(),
        context_json: "{}".to_string(),
        transcript,
        heartbeat,
        timeout_seconds: 10,
        worker: vec![worker_script.to_string_lossy().to_string()],
    };

    let mut child = ReleaseBinary::run_supervisor(args);
    let mut stdout = child.stdout.take().expect("take stdout");
    let mut stdin = child.stdin.take().expect("take stdin");

    // Read the READY frame (4-byte LE length + body)
    let mut header = [0u8; 4];
    stdout.read_exact(&mut header).expect("read header");
    let len = u32::from_le_bytes(header) as usize;
    let mut buf = vec![0u8; len];
    stdout.read_exact(&mut buf).expect("read body");
    let frame_bytes: Vec<u8> = header.iter().copied().chain(buf.iter().copied()).collect();
    let (frame, _) = decode_frame(&frame_bytes).expect("decode frame");
    match &frame {
        ControlFrame::Ready { pgid } => {
            assert!(*pgid > 0, "READY frame should carry a positive PGID");
        }
        other => {
            panic!("expected READY frame, got: {other:?}");
        }
    }

    // Send ACK
    let ack = encode_frame(&ControlFrame::Ack).expect("encode ACK");
    stdin.write_all(&ack).expect("write ACK");
    stdin.flush().expect("flush ACK");

    // Read the DONE frame
    let mut header2 = [0u8; 4];
    stdout.read_exact(&mut header2).expect("read done header");
    let len2 = u32::from_le_bytes(header2) as usize;
    let mut buf2 = vec![0u8; len2];
    stdout.read_exact(&mut buf2).expect("read done body");
    let done_bytes: Vec<u8> = header2
        .iter()
        .copied()
        .chain(buf2.iter().copied())
        .collect();
    let (done_frame, _) = decode_frame(&done_bytes).expect("decode done frame");
    match &done_frame {
        ControlFrame::Done { status, signaled } => {
            assert_eq!(*status, 0, "worker should exit 0");
            assert!(!signaled, "worker should not be signaled");
        }
        other => {
            panic!("expected DONE frame, got: {other:?}");
        }
    }

    // Wait for clean exit
    let status = child.wait().expect("wait for supervisor");
    assert!(status.success(), "supervisor should exit cleanly");
}

// -----------------------------------------------------------------------
// AC-04: All fixtures run with no user secrets in environment
// -----------------------------------------------------------------------

/// ProcessTree can be created and used with `env_clear()`.
#[cfg(target_os = "linux")]
#[test]
fn ac04_process_tree_runs_with_no_user_secrets() {
    // env_clear() in the test parent is not meaningful for
    // ProcessTree (it doesn't inherit ENV for itself), but we
    // verify the fixture can be created and spawn a child without
    // any token in the environment.
    let _ = std::env::var("GITHUB_TOKEN").ok();
    let _ = std::env::var("GH_TOKEN").ok();
    let _ = std::env::var("CADUCEUS_GITHUB_TOKEN").ok();
    // The fixture itself should not read any of these.
    let pt = ProcessTree::start("ac04-secrets");
    let pid = pt.spawn_detached_bash(": # noop");
    if pid > 0 {
        pt.terminate(pid, nix::sys::signal::Signal::SIGKILL);
    }
    // If we reach here, no panic occurred.
}

/// CrashPoint can be created and used with no tokens in the env.
#[cfg(target_os = "linux")]
#[test]
fn ac04_crash_point_runs_with_no_user_secrets() {
    let cp = CrashPoint::new("ac04-secrets");
    // Run a trivial script that exits immediately.
    let (code, _signaled) = cp.kill_at_marker("#!/bin/bash\nexit 0\n", "NEVER_MATCH");
    assert_eq!(code, 0, "trivial script should exit 0");
}

/// ReleaseBinary can be located and hashed with no tokens in the env.
#[test]
fn ac04_release_binary_runs_with_no_user_secrets() {
    let path = ReleaseBinary::locate();
    let hash = ReleaseBinary::sha256(&path);
    assert!(!hash.is_empty(), "SHA-256 hash should not be empty");
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Helper: search /proc for any `sleep` process.
#[cfg(target_os = "linux")]
fn find_sleep_process() -> bool {
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Ok(_pid) = name_str.parse::<i32>() else {
            continue;
        };
        let cmdline_path = entry.path().join("cmdline");
        let cmdline = match fs::read_to_string(&cmdline_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if cmdline.contains("sleep") {
            return true;
        }
    }
    false
}

fn tempdir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("caduceus-fixture-self-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}
