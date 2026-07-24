//! Tests drive [`parse_result_file`] and [`validate_worker_result`]
//! against temp files and in-memory structs. Symlink, oversized,
//! missing, and non-Unicode cases use the filesystem; the rest
//! construct bytes in memory so the schema assertions stay
//! fast and deterministic.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::worker::{
    parse_result_file, validate_worker_result, WorkerResult, WorkerStatus, MAX_ARTIFACTS,
    MAX_ARTIFACT_KEY_LEN, MAX_RESULT_FILE_BYTES, MAX_SUMMARY_BYTES, MAX_TITLE_BYTES,
};

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
    dir.push(format!("caduceus-worker-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write_file(path: &PathBuf, body: &[u8]) {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("create file");
    f.write_all(body).expect("write body");
    f.sync_all().ok();
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

fn make_result_json(body: &str) -> String {
    format!(
        r#"{{"status":"success","summary":"{body}","commit_message":"fix: thing","pull_request_title":"fix: thing","artifacts":{{}}}}"#
    )
}

// File-system success path

#[test]
fn parse_minimal_result_file_succeeds() {
    let root = tempdir("minimal");
    let path = root.join("worker-result.json");
    let body = make_result_json("Did the thing.");
    write_file(&path, body.as_bytes());

    let result = parse_result_file(&path, &sample_issue()).expect("parses");
    assert_eq!(result.status, WorkerStatus::Success);
    assert_eq!(result.summary, "Did the thing.");
    assert_eq!(result.commit_message, "fix: thing");
    assert_eq!(result.pull_request_title, "fix: thing");
    assert!(result.artifacts.is_empty());
    assert!(!result.investigation);
}

#[test]
fn parse_nested_artifact_value_preserves_structure() {
    let root = tempdir("nested");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "success",
        "summary": "ok",
        "commit_message": "fix: thing",
        "pull_request_title": "fix: thing",
        "artifacts": {
            "files_changed": [1, 2, 3],
            "metadata": {"step": 7, "tags": ["a", "b"]},
            "scalar": "hello"
        }
    }"#;
    write_file(&path, body.as_bytes());

    let result = parse_result_file(&path, &sample_issue()).expect("parses");
    let arts = &result.artifacts;
    assert_eq!(arts["files_changed"], serde_json::json!([1, 2, 3]));
    assert_eq!(
        arts["metadata"],
        serde_json::json!({"step": 7, "tags": ["a", "b"]})
    );
    assert_eq!(arts["scalar"], serde_json::json!("hello"));
}

#[test]
fn parse_investigation_ticket_succeeds() {
    let root = tempdir("investigation");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "success",
        "summary": "Investigation only.",
        "commit_message": "investigation: findings",
        "pull_request_title": "investigation: findings",
        "investigation": true
    }"#;
    write_file(&path, body.as_bytes());

    let result = parse_result_file(&path, &sample_issue()).expect("parses");
    assert!(result.investigation);
}

// Malformed containers / unknown fields / missing fields

#[test]
fn parse_rejects_malformed_artifacts_container() {
    let root = tempdir("malformed-artifacts");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "success",
        "summary": "ok",
        "commit_message": "fix: thing",
        "pull_request_title": "fix: thing",
        "artifacts": "not-a-map"
    }"#;
    write_file(&path, body.as_bytes());

    let err = parse_result_file(&path, &sample_issue()).expect_err("malformed");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
    assert!(msg.contains("parse"), "got: {msg}");
}

#[test]
fn parse_rejects_unknown_top_level_field() {
    let root = tempdir("unknown-field");
    let path = root.join("worker-result.json");
    let body = format!("{},\n    \"rogue\": true", make_result_json("ok"));
    write_file(&path, body.as_bytes());

    let err = parse_result_file(&path, &sample_issue()).expect_err("deny_unknown_fields");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
    assert!(
        msg.contains("rogue") || msg.contains("unknown"),
        "got: {msg}"
    );
}

#[test]
fn parse_rejects_missing_required_field() {
    let root = tempdir("missing-field");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "success",
        "summary": "ok",
        "commit_message": "fix: thing"
    }"#;
    write_file(&path, body.as_bytes());

    let err = parse_result_file(&path, &sample_issue()).expect_err("missing pull_request_title");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

#[test]
fn parse_rejects_wrong_status() {
    let root = tempdir("wrong-status");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "queued",
        "summary": "ok",
        "commit_message": "fix: thing",
        "pull_request_title": "fix: thing"
    }"#;
    write_file(&path, body.as_bytes());

    let err = parse_result_file(&path, &sample_issue()).expect_err("unknown status");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

// String field rules

#[test]
fn validate_rejects_empty_summary() {
    let mut result = minimal_result();
    result.summary = "   ".to_string();
    let err = validate_worker_result(&result, &sample_issue()).expect_err("empty");
    let msg = format!("{err:?}");
    assert!(msg.contains("summary"), "got: {msg}");
    assert!(msg.contains("empty"), "got: {msg}");
}

#[test]
fn validate_rejects_summary_oversized() {
    let mut result = minimal_result();
    result.summary = "a".repeat(MAX_SUMMARY_BYTES + 1);
    let err = validate_worker_result(&result, &sample_issue()).expect_err("too big");
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds limit"), "got: {msg}");
    assert!(msg.contains("65536"), "got: {msg}");
}

#[test]
fn validate_rejects_pr_title_with_newline() {
    let mut result = minimal_result();
    result.pull_request_title = "first\nsecond".to_string();
    let err = validate_worker_result(&result, &sample_issue()).expect_err("multi-line title");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("single line") || msg.contains("control"),
        "got: {msg}"
    );
}

#[test]
fn validate_rejects_pr_title_with_control_character() {
    let mut result = minimal_result();
    result.pull_request_title = "title\u{0007}with-bell".to_string();
    let err = validate_worker_result(&result, &sample_issue()).expect_err("control");
    let msg = format!("{err:?}");
    assert!(msg.contains("control"), "got: {msg}");
}

#[test]
fn validate_rejects_commit_message_with_control_character() {
    let mut result = minimal_result();
    result.commit_message = "fix: thing\u{0007}beep".to_string();
    let err = validate_worker_result(&result, &sample_issue()).expect_err("control");
    let msg = format!("{err:?}");
    assert!(msg.contains("control"), "got: {msg}");
}

#[test]
fn validate_accepts_multiline_commit_message() {
    let mut result = minimal_result();
    result.commit_message = "fix: thing\n\nA longer description\nwith several lines.".to_string();
    assert_eq!(
        result.commit_message.len(),
        "fix: thing\n\nA longer description\nwith several lines.".len()
    );
    validate_worker_result(&result, &sample_issue()).expect("multi-line commit message is OK");
}

#[test]
fn validate_rejects_nul_in_string() {
    let mut result = minimal_result();
    result.summary = "before\0after".to_string();
    let err = validate_worker_result(&result, &sample_issue()).expect_err("NUL");
    let msg = format!("{err:?}");
    assert!(msg.contains("NUL"), "got: {msg}");
}

#[test]
fn validate_rejects_oversized_commit_message() {
    let mut result = minimal_result();
    result.commit_message = "a".repeat(MAX_TITLE_BYTES + 1);
    let err = validate_worker_result(&result, &sample_issue()).expect_err("too long");
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds limit"), "got: {msg}");
    assert!(msg.contains("256"), "got: {msg}");
}

// Artifact rules

#[test]
fn validate_rejects_empty_artifact_key() {
    let mut result = minimal_result();
    result
        .artifacts
        .insert("".to_string(), serde_json::json!("x"));
    let err = validate_worker_result(&result, &sample_issue()).expect_err("empty key");
    let msg = format!("{err:?}");
    assert!(msg.contains("artifact key"), "got: {msg}");
}

#[test]
fn validate_rejects_oversized_artifact_key() {
    let mut result = minimal_result();
    result
        .artifacts
        .insert("a".repeat(MAX_ARTIFACT_KEY_LEN + 1), serde_json::json!("x"));
    let err = validate_worker_result(&result, &sample_issue()).expect_err("key too long");
    let msg = format!("{err:?}");
    assert!(msg.contains("artifact key"), "got: {msg}");
    assert!(msg.contains("exceeds limit"), "got: {msg}");
    assert!(msg.contains("128"), "got: {msg}");
}

#[test]
fn validate_rejects_control_in_artifact_key() {
    let mut result = minimal_result();
    result
        .artifacts
        .insert("bad\u{0007}key".to_string(), serde_json::json!("x"));
    let err = validate_worker_result(&result, &sample_issue()).expect_err("control key");
    let msg = format!("{err:?}");
    assert!(msg.contains("control"), "got: {msg}");
}

#[test]
fn validate_rejects_too_many_artifacts() {
    let mut result = minimal_result();
    for i in 0..(MAX_ARTIFACTS + 1) {
        result
            .artifacts
            .insert(format!("key-{i}"), serde_json::json!(i));
    }
    let err = validate_worker_result(&result, &sample_issue()).expect_err("too many");
    let msg = format!("{err:?}");
    assert!(msg.contains("artifacts exceeds limit"), "got: {msg}");
    assert!(msg.contains("100"), "got: {msg}");
}

#[test]
fn validate_accepts_max_size_artifacts() {
    let mut result = minimal_result();
    for i in 0..MAX_ARTIFACTS {
        result
            .artifacts
            .insert(format!("key-{i}"), serde_json::json!(i));
    }
    validate_worker_result(&result, &sample_issue()).expect("max-size is OK");
}

// File-system failure paths

#[test]
fn parse_rejects_invalid_utf8() {
    let root = tempdir("invalid-utf8");
    let path = root.join("worker-result.json");
    let bytes: [u8; 4] = [0x7B, 0x80, 0x22, 0x7D];
    write_file(&path, &bytes);
    let err = parse_result_file(&path, &sample_issue()).expect_err("invalid utf8");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

#[test]
fn parse_rejects_oversized_file() {
    let root = tempdir("oversized");
    let path = root.join("worker-result.json");
    // Allocate slightly over the cap; the read cap rejects it.
    let cap = (MAX_RESULT_FILE_BYTES + 1) as usize;
    let mut big = vec![b' '; cap];
    big[0] = b'{';
    big[cap - 1] = b'}';
    // Skip the JSON validity requirement — the file-size check runs first.
    write_file(&path, &big);
    let err = parse_result_file(&path, &sample_issue()).expect_err("too large");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
    assert!(
        msg.contains("exceeds cap") || msg.contains("read"),
        "got: {msg}"
    );
}

#[test]
fn parse_rejects_symlink() {
    let root = tempdir("symlink");
    let real = root.join("real");
    write_file(&real, make_result_json("ok").as_bytes());
    let link = root.join("worker-result.json");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let err = parse_result_file(&link, &sample_issue()).expect_err("symlink");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

#[test]
fn parse_rejects_missing_file() {
    let root = tempdir("missing");
    let path = root.join("does-not-exist.json");
    let err = parse_result_file(&path, &sample_issue()).expect_err("missing");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

#[test]
fn parse_rejects_directory() {
    let root = tempdir("dir");
    let path = root.join("worker-result-dir");
    fs::create_dir(&path).expect("create dir");
    let err = parse_result_file(&path, &sample_issue()).expect_err("directory");
    let msg = format!("{err:?}");
    assert!(msg.contains("Worker"), "got: {msg}");
}

// Error variant consistency

#[test]
fn file_io_failure_is_worker_read_context() {
    let root = tempdir("read-context");
    let path = root.join("does-not-exist.json");
    let err = parse_result_file(&path, &sample_issue()).expect_err("missing");
    match err {
        CaduceusError::Worker { context, stderr } => {
            assert_eq!(context, "read", "got: {stderr}");
            assert!(stderr.contains("does-not-exist.json"), "got: {stderr}");
        }
        other => panic!("expected Worker; got: {other:?}"),
    }
}

#[test]
fn schema_failure_is_worker_parse_context() {
    let root = tempdir("parse-context");
    let path = root.join("worker-result.json");
    write_file(&path, b"not json");
    let err = parse_result_file(&path, &sample_issue()).expect_err("bad json");
    match err {
        CaduceusError::Worker { context, .. } => {
            assert_eq!(context, "parse");
        }
        other => panic!("expected Worker; got: {other:?}"),
    }
}

#[test]
fn validation_failure_is_worker_validate_context() {
    let root = tempdir("validate-context");
    let path = root.join("worker-result.json");
    let body = r#"{
        "status": "success",
        "summary": "ok",
        "commit_message": "fix: thing",
        "pull_request_title": "first\nsecond"
    }"#;
    write_file(&path, body.as_bytes());
    let err = parse_result_file(&path, &sample_issue()).expect_err("multi-line title");
    match err {
        CaduceusError::Worker { context, .. } => {
            assert_eq!(context, "validate");
        }
        other => panic!("expected Worker; got: {other:?}"),
    }
}

// Internals

#[test]
fn can_create_file_with_o_nofollow_explicitly() {
    // The parse function uses O_NOFOLLOW; this test makes sure the
    // dependency path is reachable. It does not need to fail.
    let root = tempdir("nofollow");
    let path = root.join("normal");
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .expect("open with O_NOFOLLOW");
    assert!(f.metadata().is_ok());
}
