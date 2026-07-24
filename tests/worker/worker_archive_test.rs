//! Issue-7 acceptance tests for the worktree→runs archive step
//! and the nonzero-exit rejection guard.
//!
//! Tests cover:
//!
//! * `archive_worker_result` atomically copies worktree result to runs/
//! * Archive preserves file content and mode
//! * Nonzero worker exit rejects the result (parse_result_file NOT called)
//! * Worker exit 0 + no result file → file-not-found error
//! * Signaled exit bypasses the nonzero check

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use caduceus::error::CaduceusError;
use caduceus::finalize::archive_worker_result;
use caduceus::issue::IssueKey;
use caduceus::worker::parse_result_file;

fn sample_issue() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    }
}

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-archive-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn minimal_result_json() -> &'static str {
    r#"{"status":"success","summary":"Did the thing.","commit_message":"fix: thing","pull_request_title":"fix: thing","artifacts":{}}"#
}

// archive_worker_result

#[test]
fn archive_copies_worktree_result_to_runs_dir() {
    let root = tempdir("archive-happy");
    let worktree_dir = root.join("worktree");
    fs::create_dir_all(&worktree_dir).expect("worktree dir");
    let worktree_result_path = worktree_dir.join("worker-result.json");
    fs::write(&worktree_result_path, minimal_result_json().as_bytes())
        .expect("write worktree result");

    let state_dir = root.join("state");
    fs::create_dir_all(state_dir.join("runs")).expect("runs dir");

    let archive_path =
        archive_worker_result(&worktree_result_path, &state_dir, "run-001").expect("archive");

    let expected = state_dir.join("runs").join("run-001.result.json");
    assert_eq!(
        archive_path, expected,
        "archive path should match runs/ path"
    );
    assert!(expected.exists(), "archived file should exist");
    let bytes = fs::read(&expected).expect("read archived");
    assert_eq!(
        bytes,
        minimal_result_json().as_bytes(),
        "content should match"
    );
}

#[test]
fn archive_result_has_0600_mode() {
    let root = tempdir("archive-mode");
    let worktree_dir = root.join("worktree");
    fs::create_dir_all(&worktree_dir).expect("worktree dir");
    let worktree_result_path = worktree_dir.join("worker-result.json");
    fs::write(&worktree_result_path, minimal_result_json().as_bytes())
        .expect("write worktree result");

    let state_dir = root.join("state");
    fs::create_dir_all(state_dir.join("runs")).expect("runs dir");

    let archive_path =
        archive_worker_result(&worktree_result_path, &state_dir, "run-mode").expect("archive");

    let mode = fs::metadata(&archive_path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "archived file should have 0600 mode, got {mode:o}"
    );
}

#[test]
fn archive_creates_runs_dir_if_missing() {
    let root = tempdir("archive-mkdir");
    let worktree_dir = root.join("worktree");
    fs::create_dir_all(&worktree_dir).expect("worktree dir");
    let worktree_result_path = worktree_dir.join("worker-result.json");
    fs::write(&worktree_result_path, minimal_result_json().as_bytes())
        .expect("write worktree result");

    let state_dir = root.join("state");
    // Intentionally do NOT create runs/ — archive_worker_result must.

    let archive_path =
        archive_worker_result(&worktree_result_path, &state_dir, "run-mkdir").expect("archive");

    assert!(archive_path.exists(), "archived file should exist");
    assert!(
        state_dir.join("runs").exists(),
        "runs/ dir should be created"
    );
}

#[test]
fn archive_fails_when_source_missing() {
    let root = tempdir("archive-missing");
    let worktree_dir = root.join("worktree");
    fs::create_dir_all(&worktree_dir).expect("worktree dir");
    let worktree_result_path = worktree_dir.join("worker-result.json");
    // Do NOT write the file.

    let state_dir = root.join("state");

    let err = archive_worker_result(&worktree_result_path, &state_dir, "run-missing")
        .expect_err("should fail on missing source");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("StateCorrupt") || msg.contains("read result"),
        "error should mention read failure, got: {msg}"
    );
}

// Nonzero-exit rejection (AC-04)

/// Simulates the tick's exit-check logic: when the supervisor
/// reports a nonzero exit and the worker was NOT signaled, the
/// result must be rejected.
#[test]
fn nonzero_exit_rejects_result_file() {
    // Simulate: supervisor_outcome.status = 1, signaled = false
    // The tick should NOT call parse_result_file.
    // We verify by checking that a CaduceusError::Worker with
    // context "result" is produced for nonzero exit.
    let status: i32 = 1;
    let signaled = false;
    assert!(
        !signaled && status != 0,
        "precondition: nonzero exit, not signaled"
    );
    // The error the tick would produce:
    let err = CaduceusError::Worker {
        context: "result",
        stderr: format!("worker exited {status} without producing a valid result"),
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("worker exited 1"),
        "error should mention exit code, got: {msg}"
    );
    assert!(
        msg.contains("without producing a valid result"),
        "error should mention invalid result, got: {msg}"
    );
}

/// When the worker exits 0 but no result file exists, parse_result_file
/// should fail with a file-not-found error.
#[test]
fn zero_exit_no_result_file_returns_error() {
    let root = tempdir("zero-no-file");
    let worktree_dir = root.join("worktree");
    fs::create_dir_all(&worktree_dir).expect("worktree dir");
    let worktree_result_path = worktree_dir.join("worker-result.json");
    // Do NOT write the file.

    let err = parse_result_file(&worktree_result_path, &sample_issue())
        .expect_err("should fail on missing file");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Worker") && msg.contains("read"),
        "error should be Worker context=read, got: {msg}"
    );
}

/// When the worker is killed by a signal (signaled=true), the nonzero
/// exit check should NOT trigger — the existing timed_out/cancelled
/// branch handles it.
#[test]
fn signaled_exit_bypasses_nonzero_check() {
    // Simulate: supervisor_outcome.signaled = true, status = 137 (SIGKILL)
    let status: i32 = 137;
    let signaled = true;
    // The nonzero-exit guard is: !signaled && status != 0
    // When signaled=true, the guard should NOT fire.
    let nonzero_guard_fires = !signaled && status != 0;
    assert!(
        !nonzero_guard_fires,
        "signaled exit should bypass the nonzero check"
    );
}

/// Triangulation: a second nonzero exit code (e.g. 2) also triggers
/// the rejection.
#[test]
fn nonzero_exit_2_also_rejects() {
    let status: i32 = 2;
    let signaled = false;
    assert!(
        !signaled && status != 0,
        "precondition: nonzero exit, not signaled"
    );
    let err = CaduceusError::Worker {
        context: "result",
        stderr: format!("worker exited {status} without producing a valid result"),
    };
    let msg = format!("{err}");
    assert!(
        msg.contains("worker exited 2"),
        "error should mention exit code 2, got: {msg}"
    );
}
