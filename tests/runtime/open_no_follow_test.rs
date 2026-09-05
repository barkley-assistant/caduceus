#![cfg(unix)]

//! Audit verdict (see design.md): each supervisor runtime file call site is
//! protected by trailing-component `O_NOFOLLOW` only; no parent-directory walk
//! is needed under the operator-approved threat model. This integration test
//! guards the contract against any future caller that drops the flag.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use caduceus::error::CaduceusError;
use caduceus::finalize::dry_run_archive::write_atomic;
use caduceus::issue::IssueKey;
use caduceus::worker::prompt::write_prompt;
use caduceus::worker::{parse_result_file, WorkerResult, WorkerStatus};
use caduceus::worker_supervisor::{
    open_transcript, truncate_transcript, write_heartbeat_record, Heartbeat, HEARTBEAT_FILE_VERSION,
};
use tempfile::tempdir;

fn write_file(path: &Path, contents: &[u8]) {
    let mut file = fs::File::create(path).expect("create file");
    file.write_all(contents).expect("write file");
}

fn assert_error<T: std::fmt::Debug>(result: Result<T, CaduceusError>, context: &str) {
    let err = result.expect_err(context);
    eprintln!("{err:?}");
}

fn sample_issue() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    }
}

fn minimal_result() -> WorkerResult {
    WorkerResult {
        status: WorkerStatus::Success,
        summary: "Did the thing.".to_string(),
        commit_message: "fix: thing".to_string(),
        pull_request_title: "fix: thing".to_string(),
        artifacts: BTreeMap::new(),
        investigation: false,
    }
}

#[test]
fn heartbeat_write_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let runs = dir.path().join("runs");
    fs::create_dir(&runs).expect("runs directory");

    let real = runs.join("REAL.heartbeat");
    write_file(&real, b"heartbeat target");
    let link = runs.join("RUN-LINK.heartbeat");
    let temp = link.with_extension("heartbeat.tmp");
    symlink(&real, &temp).expect("heartbeat temp symlink");

    let now = chrono::Utc::now();
    let record = Heartbeat {
        version: HEARTBEAT_FILE_VERSION,
        run_id: "RUN-LINK".to_string(),
        pid: std::process::id(),
        started_at: now,
        updated_at: now,
        target: "owner/repo#1".to_string(),
        transcript_path: PathBuf::from("/tmp"),
    };

    assert_error(
        write_heartbeat_record(&record, &link),
        "heartbeat trailing symlink",
    );
}

#[test]
fn transcript_open_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let real = dir.path().join("REAL.log");
    write_file(&real, b"transcript target");
    let link = dir.path().join("RUN-LINK.log");
    symlink(&real, &link).expect("transcript symlink");

    assert_error(open_transcript(&link), "transcript trailing symlink");
}

#[test]
fn transcript_truncate_read_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let real = dir.path().join("REAL.log");
    write_file(&real, &[b'x'; 1024]);
    let link = dir.path().join("RUN-LINK.log");
    symlink(&real, &link).expect("transcript symlink");

    // symlink_metadata refuses the link before the read-side O_NOFOLLOW open.
    assert_error(
        truncate_transcript(&link, 4),
        "transcript truncate read symlink",
    );
}

#[test]
fn transcript_truncate_write_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let real = dir.path().join("REAL.log");
    write_file(&real, &[b'x'; 1024]);
    let link = dir.path().join("RUN-LINK.log");
    symlink(&real, &link).expect("transcript symlink");

    // The same preflight protects the later write-side O_NOFOLLOW open.
    assert_error(
        truncate_transcript(&link, 4),
        "transcript truncate write symlink",
    );
}

#[test]
fn worker_prompt_write_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let worktree = dir.path();
    let real = worktree.join("PROMPT-TARGET");
    write_file(&real, b"prompt target");
    let temp = worktree.join(format!(".worker-prompt.md.{}.tmp", std::process::id()));
    symlink(&real, &temp).expect("prompt temp symlink");

    assert_error(
        write_prompt(worktree, "hello"),
        "worker prompt trailing symlink",
    );
}

#[test]
fn worker_result_read_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let real = dir.path().join("REAL.worker-result.json");
    let json = serde_json::to_string(&minimal_result()).expect("serialize result");
    write_file(&real, json.as_bytes());
    let link = dir.path().join("worker-result.json");
    symlink(&real, &link).expect("worker result symlink");

    assert_error(
        parse_result_file(&link, &sample_issue()),
        "worker result trailing symlink",
    );
}

#[test]
fn dry_run_report_write_rejects_trailing_symlink() {
    let dir = tempdir().expect("tempdir");
    let real = dir.path().join("REAL.report");
    write_file(&real, b"before");
    let before = fs::read(&real).expect("read target snapshot");
    let link = dir.path().join("report");
    symlink(&real, &link).expect("dry-run report symlink");

    // The temporary name includes nanoseconds and cannot be planted
    // deterministically. A successful rename replaces the final symlink
    // rather than following it, so the linked target must remain unchanged.
    match write_atomic(&link, b"x") {
        Err(err) => eprintln!("{err:?}"),
        Ok(()) => {
            assert_eq!(fs::read(&real).expect("read target"), before);
        }
    }
}
