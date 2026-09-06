//! ReviewResult validation tests (issue #305, DAR §8, §10.3, §15).
//!
//! Acceptance coverage (AC numbers are #305's, verbatim):
//!
//! - AC1 — PASS + zero blocking accepted.
//! - AC2 — FAIL + blocking findings accepted.
//! - AC3 — FAIL + zero blocking rejected as execution failure.
//! - AC4 — PASS + blocking findings rejected as execution failure.
//! - AC5 — unknown schema_version rejected as execution failure.
//! - AC6 — malformed severity/path/line rejected; field caps
//!   enforced (per-field caps are pinned in review_domain_test;
//!   this file pins the single-finding comment-budget invariant
//!   and the malformed-field matrix).
//!
//! Plus the status↔review presence rule (DAR §3 "present iff
//! status == Success") and, in the ingress section, Worker
//! classification of every rejection (DAR §8).

use caduceus::review::{
    parse_review_result, validate_review_result, ExecutionStatus, Finding, Review, ReviewResult,
    Severity, Verdict, MAX_FINDING_BODY_BYTES, MAX_FINDING_PATH_BYTES,
    MAX_FINDING_REMEDIATION_BYTES, MAX_FINDING_TITLE_BYTES, REVIEW_SCHEMA_VERSION,
};
use caduceus::worker::{parse_review_result_file, MAX_REVIEW_RESULT_FILE_BYTES};
use serde_json::json;

// -----------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------

fn finding(severity: Severity) -> Finding {
    Finding {
        severity,
        title: "title".to_string(),
        body: "body".to_string(),
        path: Some("src/lib.rs".to_string()),
        line: Some(7),
        remediation: Some("fix it".to_string()),
    }
}

fn review(verdict: Verdict, findings: Vec<Finding>) -> Review {
    Review {
        verdict,
        summary: "summary".to_string(),
        findings,
    }
}

fn success_result(review: Option<Review>) -> ReviewResult {
    ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: ExecutionStatus::Success,
        review,
    }
}

fn doc(result: &ReviewResult) -> String {
    serde_json::to_string(result).unwrap()
}

// -----------------------------------------------------------------------
// AC1 / AC2 — consistent combinations accepted
// -----------------------------------------------------------------------

#[test]
fn ac1_pass_with_zero_blocking_accepted() {
    let result = success_result(Some(review(
        Verdict::Pass,
        vec![finding(Severity::Warning), finding(Severity::Suggestion)],
    )));
    parse_review_result(&doc(&result)).expect("PASS + zero blocking accepted");
    validate_review_result(&result).expect("validator agrees");
}

#[test]
fn ac1_pass_with_zero_findings_accepted() {
    // A clean review: PASS with no findings at all.
    let result = success_result(Some(review(Verdict::Pass, vec![])));
    parse_review_result(&doc(&result)).expect("PASS + no findings accepted");
}

#[test]
fn ac2_fail_with_blocking_accepted() {
    let result = success_result(Some(review(
        Verdict::Fail,
        vec![finding(Severity::Blocking), finding(Severity::Warning)],
    )));
    parse_review_result(&doc(&result)).expect("FAIL + blocking accepted");
}

// -----------------------------------------------------------------------
// AC3 / AC4 — inconsistent combinations rejected
// -----------------------------------------------------------------------

#[test]
fn ac3_fail_with_zero_blocking_rejected() {
    let with_warnings = success_result(Some(review(
        Verdict::Fail,
        vec![finding(Severity::Warning), finding(Severity::Suggestion)],
    )));
    let err = parse_review_result(&doc(&with_warnings)).unwrap_err();
    assert!(err.to_string().contains("blocking"), "got: {err}");

    let no_findings = success_result(Some(review(Verdict::Fail, vec![])));
    assert!(parse_review_result(&doc(&no_findings)).is_err());
}

#[test]
fn ac4_pass_with_blocking_rejected() {
    let result = success_result(Some(review(
        Verdict::Pass,
        vec![finding(Severity::Suggestion), finding(Severity::Blocking)],
    )));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(err.to_string().contains("blocking"), "got: {err}");
}

// -----------------------------------------------------------------------
// AC5 — schema_version (typed variant, existing behavior pinned)
// -----------------------------------------------------------------------

#[test]
fn ac5_unknown_schema_version_rejected() {
    for bad in [0u32, 2, 999] {
        let raw = json!({
            "schema_version": bad,
            "status": "success",
            "review": null
        });
        // The version check fires BEFORE the presence rule, so the
        // typed variant survives (order pinned by the implementation).
        let err = parse_review_result(&raw.to_string()).unwrap_err();
        assert!(
            matches!(err, caduceus::CaduceusError::ReviewSchemaVersion { found, supported }
                     if found == bad && supported == REVIEW_SCHEMA_VERSION),
            "v{bad}: {err:?}"
        );
    }
}

// -----------------------------------------------------------------------
// Presence rule (DAR §3: review present iff status == Success)
// -----------------------------------------------------------------------

#[test]
fn success_without_review_rejected() {
    let result = success_result(None);
    let err = validate_review_result(&result).unwrap_err();
    assert!(err.to_string().contains("must be present"), "got: {err}");
}

#[test]
fn failure_with_review_rejected() {
    let result = ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: ExecutionStatus::Failure,
        review: Some(review(Verdict::Pass, vec![])),
    };
    let err = validate_review_result(&result).unwrap_err();
    assert!(err.to_string().contains("must be absent"), "got: {err}");
}

#[test]
fn failure_without_review_accepted() {
    // A non-executing run is a VALID document; retry is the tick's
    // decision (#312), never the validator's.
    let result = ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: ExecutionStatus::Failure,
        review: None,
    };
    parse_review_result(&doc(&result)).expect("failure status is valid");
}

// -----------------------------------------------------------------------
// AC6 — malformed severity / path / line
// -----------------------------------------------------------------------

#[test]
fn malformed_severity_rejected_at_parse() {
    let raw = json!({
        "schema_version": 1,
        "status": "success",
        "review": {
            "verdict": "pass",
            "summary": "s",
            "findings": [
                {"severity": "critical", "title": "t", "body": "b"}
            ]
        }
    });
    assert!(parse_review_result(&raw.to_string()).is_err());
}

#[test]
fn malformed_line_zero_rejected() {
    let mut f = finding(Severity::Warning);
    f.line = Some(0);
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(err.to_string().contains("1-based"), "got: {err}");
}

#[test]
fn malformed_line_without_path_rejected() {
    let mut f = finding(Severity::Warning);
    f.path = None;
    // line still Some(7) — a line number with no file is malformed.
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(
        err.to_string().contains("line requires a path"),
        "got: {err}"
    );
}

#[test]
fn path_without_line_accepted() {
    // File-level findings are legitimate.
    let mut f = finding(Severity::Warning);
    f.line = None;
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    parse_review_result(&doc(&result)).expect("path without line accepted");
}

#[test]
fn malformed_absolute_path_rejected() {
    let mut f = finding(Severity::Warning);
    f.path = Some("/etc/passwd".to_string());
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(err.to_string().contains("repo-relative"), "got: {err}");
}

#[test]
fn malformed_parent_component_rejected() {
    let mut f = finding(Severity::Warning);
    f.path = Some("src/../../etc/passwd".to_string());
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(err.to_string().contains("'..'"), "got: {err}");
}

#[test]
fn malformed_control_char_path_rejected() {
    let mut f = finding(Severity::Warning);
    f.path = Some("src/lib.rs\n".to_string());
    let result = success_result(Some(review(Verdict::Pass, vec![f])));
    let err = parse_review_result(&doc(&result)).unwrap_err();
    assert!(err.to_string().contains("control"), "got: {err}");
}

// -----------------------------------------------------------------------
// AC6 — caps: the DAR §10.3 invariant, made executable
// -----------------------------------------------------------------------

#[test]
fn single_finding_cannot_exceed_comment_budget_alone() {
    // DAR §10.3 / §9.2: no single adversarial finding can exceed the
    // 64 KiB (65,536-byte) sticky-comment budget by itself. The worst
    // fully-loaded finding is title + body + path + remediation.
    let worst = MAX_FINDING_TITLE_BYTES
        + MAX_FINDING_BODY_BYTES
        + MAX_FINDING_PATH_BYTES
        + MAX_FINDING_REMEDIATION_BYTES;
    assert!(worst < 64 * 1024, "DAR §10.3 invariant broken: {worst}");
}

// -----------------------------------------------------------------------
// File ingress (worker_contract::parse_review_result_file) + Worker
// classification of every rejection (DAR §8)
// -----------------------------------------------------------------------

use caduceus::orchestration::{classify_error, FailureClass};
use caduceus::CaduceusError;
use std::fs;
use std::path::Path;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

#[test]
fn ingress_accepts_consistent_pass_document() {
    let dir = tempdir("rv-ingress-ok");
    let path = dir.join("worker-result.json");
    fs::write(
        &path,
        doc(&success_result(Some(review(
            Verdict::Pass,
            vec![finding(Severity::Warning)],
        )))),
    )
    .unwrap();
    let result = parse_review_result_file(&path).expect("consistent document accepted");
    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.review.as_ref().unwrap().verdict, Verdict::Pass);
}

#[test]
fn ingress_accepts_failure_status_document() {
    let dir = tempdir("rv-ingress-failure");
    let path = dir.join("worker-result.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"status":"failure","review":null}"#,
    )
    .unwrap();
    let result = parse_review_result_file(&path).expect("failure status is a valid document");
    assert_eq!(result.status, ExecutionStatus::Failure);
    assert!(result.review.is_none());
}

#[test]
fn ingress_missing_file_is_worker_read_error() {
    let err = parse_review_result_file(Path::new("/nonexistent/worker-result.json"))
        .expect_err("missing file rejected");
    assert!(
        matches!(
            err,
            CaduceusError::Worker {
                context: "read",
                ..
            }
        ),
        "got: {err:?}"
    );
    assert_eq!(classify_error(&err), FailureClass::Worker);
}

#[test]
fn ingress_rejects_symlinked_result_file() {
    let dir = tempdir("rv-ingress-symlink");
    let real = dir.join("worker-result.json");
    fs::write(
        &real,
        doc(&success_result(Some(review(Verdict::Pass, vec![])))),
    )
    .unwrap();
    let link = dir.join("link.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = parse_review_result_file(&link).expect_err("symlink rejected (O_NOFOLLOW)");
    assert!(
        matches!(
            err,
            CaduceusError::Worker {
                context: "read",
                ..
            }
        ),
        "got: {err:?}"
    );
}

#[test]
fn ingress_rejects_oversized_result_file() {
    let dir = tempdir("rv-ingress-oversize");
    let path = dir.join("worker-result.json");
    let mut blob = String::new();
    // Valid JSON shape, but padded past the file cap via the summary.
    blob.push_str(
        r#"{"schema_version":1,"status":"success","review":{"verdict":"pass","summary":""#,
    );
    blob.push_str(&"x".repeat(MAX_REVIEW_RESULT_FILE_BYTES as usize));
    blob.push_str(r#"","findings":[]}}"#);
    fs::write(&path, blob).unwrap();
    let err = parse_review_result_file(&path).expect_err("oversized file rejected");
    assert!(
        matches!(
            err,
            CaduceusError::Worker {
                context: "read",
                ..
            }
        ),
        "got: {err:?}"
    );
    assert!(format!("{err:?}").contains("exceeds cap"), "got: {err:?}");
}

#[test]
fn ingress_schema_version_mismatch_is_typed_and_worker_class() {
    let dir = tempdir("rv-ingress-version");
    let path = dir.join("worker-result.json");
    fs::write(
        &path,
        r#"{"schema_version":2,"status":"success","review":null}"#,
    )
    .unwrap();
    let err = parse_review_result_file(&path).expect_err("unknown version rejected");
    assert!(
        matches!(
            err,
            CaduceusError::ReviewSchemaVersion {
                found: 2,
                supported: 1
            }
        ),
        "got: {err:?}"
    );
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Worker);
    assert!(class.counts_against_retry_budget());
}

#[test]
fn ingress_verdict_rejection_is_worker_validate_and_burns_budget() {
    let dir = tempdir("rv-ingress-verdict");
    let path = dir.join("worker-result.json");
    fs::write(
        &path,
        doc(&success_result(Some(review(Verdict::Fail, vec![])))),
    )
    .unwrap();
    let err = parse_review_result_file(&path).expect_err("FAIL + zero blocking rejected");
    assert!(
        matches!(
            err,
            CaduceusError::Worker {
                context: "validate",
                ..
            }
        ),
        "got: {err:?}"
    );
    assert!(format!("{err:?}").contains("blocking"), "got: {err:?}");
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Worker);
    assert!(class.counts_against_retry_budget());
}

#[test]
fn ingress_malformed_json_is_worker_error() {
    let dir = tempdir("rv-ingress-malformed");
    let path = dir.join("worker-result.json");
    fs::write(&path, "not json").unwrap();
    let err = parse_review_result_file(&path).expect_err("malformed document rejected");
    assert!(matches!(err, CaduceusError::Worker { .. }), "got: {err:?}");
    assert_eq!(classify_error(&err), FailureClass::Worker);
}
