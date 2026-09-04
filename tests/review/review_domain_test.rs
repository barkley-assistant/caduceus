//! Auto Review domain types (issue #292).
//!
//! Acceptance coverage:
//!
//! - AC1 — round-trips with `deny_unknown_fields` on every type.
//! - AC2 — `schema_version` present, daemon const = 1, enforced.
//! - AC3 — `merge_base` on target; generation/publication fields on
//!   state; `attempt_count` is an unknown field.
//! - AC4 — findings ordering preserved verbatim.
//! - AC5 — enums serialize snake_case, reject unknown values.
//! - AC6 — caps declared and enforced at parse time.

use caduceus::review::{
    parse_review_result, validate_review_state, validate_review_target, ExecutionStatus, Finding,
    PublicationState, RepositoryId, Review, ReviewResult, ReviewState, ReviewTarget, Severity,
    Verdict, MAX_FINDINGS, MAX_FINDING_BODY_BYTES, MAX_FINDING_PATH_BYTES,
    MAX_FINDING_REMEDIATION_BYTES, MAX_FINDING_TITLE_BYTES, MAX_PUBLISH_ERROR_BYTES, MAX_REF_BYTES,
    MAX_REPO_COMPONENT_BYTES, MAX_REVIEW_SUMMARY_BYTES, MAX_RUN_ID_BYTES, MAX_SHA_BYTES,
    REVIEW_SCHEMA_VERSION,
};
use serde_json::json;

// -----------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------

fn sample_repository() -> RepositoryId {
    RepositoryId {
        owner: "barkley-assistant".to_string(),
        repo: "caduceus".to_string(),
    }
}

fn sample_target() -> ReviewTarget {
    ReviewTarget {
        repository: sample_repository(),
        pull_request: 42,
        head_sha: "a".repeat(40),
        base_sha: "b".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "c".repeat(40),
    }
}

fn finding(severity: Severity, title: &str) -> Finding {
    Finding {
        severity,
        title: title.to_string(),
        body: "body".to_string(),
        path: Some("src/lib.rs".to_string()),
        line: Some(7),
        remediation: Some("fix it".to_string()),
    }
}

fn result_with_findings(findings: Vec<Finding>) -> ReviewResult {
    ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: ExecutionStatus::Success,
        review: Some(Review {
            verdict: Verdict::Pass,
            summary: "ok".to_string(),
            findings,
        }),
    }
}

fn sample_state_json() -> serde_json::Value {
    json!({
        "repository": {"owner": "barkley-assistant", "repo": "caduceus"},
        "pull_request": 42,
        "last_reviewed_head_sha": null,
        "last_verdict": null,
        "last_reviewed_at": null,
        "sticky_comment_id": null,
        "last_run_id": null,
        "review_generation": 7,
        "publication_state": "pending",
        "publication_attempt_count": 0,
        "next_publish_at": null,
        "last_publish_error": null
    })
}

// -----------------------------------------------------------------------
// AC1 — round-trips + deny_unknown_fields
// -----------------------------------------------------------------------

#[test]
fn repository_id_round_trip_and_full_name() {
    let repo = sample_repository();
    let json = serde_json::to_string(&repo).unwrap();
    assert_eq!(json, r#"{"owner":"barkley-assistant","repo":"caduceus"}"#);
    let back: RepositoryId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, repo);
    assert_eq!(repo.full_name(), "barkley-assistant/caduceus");
}

#[test]
fn repository_id_rejects_unknown_fields() {
    let doc = json!({"owner": "o", "repo": "r", "number": 1});
    assert!(serde_json::from_str::<RepositoryId>(&doc.to_string()).is_err());
}

#[test]
fn review_target_round_trip_pins_wire_shape() {
    let target = sample_target();
    let json = serde_json::to_string(&target).unwrap();
    // Golden: field order + snake_case + merge_base present (AC3).
    assert_eq!(
        json,
        concat!(
            r#"{"repository":{"owner":"barkley-assistant","repo":"caduceus"},"#,
            r#""pull_request":42,"head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","#,
            r#""base_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","#,
            r#""base_ref":"main","#,
            r#""merge_base":"cccccccccccccccccccccccccccccccccccccccc"}"#
        )
    );
    let back: ReviewTarget = serde_json::from_str(&json).unwrap();
    assert_eq!(back, target);
}

#[test]
fn review_target_rejects_unknown_fields() {
    let mut doc = serde_json::to_value(sample_target()).unwrap();
    doc["diff_range"] = json!("base..head");
    assert!(serde_json::from_str::<ReviewTarget>(&doc.to_string()).is_err());
}

#[test]
fn review_result_round_trip_pins_wire_shape() {
    let result = ReviewResult {
        schema_version: 1,
        status: ExecutionStatus::Failure,
        review: None,
    };
    let json = serde_json::to_string(&result).unwrap();
    assert_eq!(
        json,
        r#"{"schema_version":1,"status":"failure","review":null}"#
    );
    let back: ReviewResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back, result);
}

#[test]
fn review_result_rejects_unknown_fields() {
    let doc = json!({
        "schema_version": 1,
        "status": "success",
        "review": null,
        "attempt_count": 3
    });
    assert!(serde_json::from_str::<ReviewResult>(&doc.to_string()).is_err());
}

#[test]
fn finding_and_review_reject_unknown_fields() {
    let mut finding_doc = serde_json::to_value(finding(Severity::Warning, "t")).unwrap();
    finding_doc["column"] = json!(3);
    assert!(serde_json::from_str::<Finding>(&finding_doc.to_string()).is_err());

    let mut review_doc = json!({
        "verdict": "pass",
        "summary": "s",
        "findings": []
    });
    review_doc["score"] = json!(0.9);
    assert!(serde_json::from_str::<Review>(&review_doc.to_string()).is_err());
}

#[test]
fn review_state_round_trip_and_rejects_unknown_fields() {
    let state: ReviewState = serde_json::from_str(&sample_state_json().to_string()).unwrap();
    assert_eq!(state.review_generation, 7);
    assert_eq!(state.publication_state, PublicationState::Pending);
    let json = serde_json::to_string(&state).unwrap();
    // snake_case on the wire (AC5 surface for PublicationState).
    assert!(json.contains(r#""publication_state":"pending""#));
    let back: ReviewState = serde_json::from_str(&json).unwrap();
    assert_eq!(back, state);

    let mut bad = sample_state_json();
    bad["unexpected"] = json!(true);
    assert!(serde_json::from_str::<ReviewState>(&bad.to_string()).is_err());
}

// -----------------------------------------------------------------------
// AC3 — no attempt_count; field set
// -----------------------------------------------------------------------

#[test]
fn review_state_rejects_attempt_count_field() {
    // The queue owns execution attempts; a serialized `attempt_count`
    // on ReviewState must be an UNKNOWN field, not a tolerated one.
    let mut doc = sample_state_json();
    doc["attempt_count"] = json!(3);
    assert!(serde_json::from_str::<ReviewState>(&doc.to_string()).is_err());
}

#[test]
fn review_state_option_fields_default_to_none_when_missing() {
    // Pin serde's Option-missing semantics so a future required-Option
    // regression is caught.
    let mut doc = sample_state_json();
    doc.as_object_mut().unwrap().remove("last_publish_error");
    doc.as_object_mut().unwrap().remove("next_publish_at");
    let state: ReviewState = serde_json::from_str(&doc.to_string()).unwrap();
    assert!(state.last_publish_error.is_none());
    assert!(state.next_publish_at.is_none());
}

#[test]
fn review_state_new_starts_pending_with_zero_counters() {
    let state = ReviewState::new(sample_repository(), 42, 3);
    assert_eq!(state.publication_state, PublicationState::Pending);
    assert_eq!(state.publication_attempt_count, 0);
    assert_eq!(state.review_generation, 3);
    assert!(state.last_run_id.is_none());
    assert!(state.sticky_comment_id.is_none());
    assert!(state.last_reviewed_head_sha.is_none());
    assert!(state.last_publish_error.is_none());
}

// -----------------------------------------------------------------------
// AC2 — schema_version
// -----------------------------------------------------------------------

#[test]
fn parse_review_result_accepts_v1() {
    let doc = json!({
        "schema_version": 1,
        "status": "success",
        "review": {
            "verdict": "fail",
            "summary": "two blocking findings",
            "findings": [
                {"severity": "blocking", "title": "t1", "body": "b",
                 "path": "src/a.rs", "line": 1, "remediation": "r"},
                {"severity": "suggestion", "title": "t2", "body": "b"}
            ]
        }
    });
    let result = parse_review_result(&doc.to_string()).unwrap();
    assert_eq!(result.schema_version, REVIEW_SCHEMA_VERSION);
    assert_eq!(result.review.as_ref().unwrap().findings.len(), 2);
    assert!(result.review.as_ref().unwrap().findings[1].path.is_none());
    assert_eq!(REVIEW_SCHEMA_VERSION, 1);
}

#[test]
fn parse_review_result_rejects_wrong_schema_version() {
    for bad in [0u32, 2, 999] {
        let doc = json!({"schema_version": bad, "status": "success", "review": null});
        let err = parse_review_result(&doc.to_string()).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "v{bad}: {err}");
    }
}

#[test]
fn parse_review_result_rejects_missing_schema_version() {
    let doc = json!({"status": "success", "review": null});
    assert!(parse_review_result(&doc.to_string()).is_err());
}

#[test]
fn parse_review_result_rejects_malformed_documents() {
    assert!(parse_review_result("not json").is_err());
    assert!(parse_review_result("{}").is_err());
}

// -----------------------------------------------------------------------
// AC4 — findings ordering
// -----------------------------------------------------------------------

#[test]
fn findings_order_preserved_verbatim_through_round_trip() {
    // Deliberately NOT severity-grouped and NOT alphabetical: any
    // future "helpful" sort at parse time breaks this test.
    let result = result_with_findings(vec![
        finding(Severity::Warning, "b-warning"),
        finding(Severity::Blocking, "a-blocking"),
        finding(Severity::Suggestion, "c-suggestion"),
        finding(Severity::Blocking, "d-blocking"),
    ]);
    let json = serde_json::to_string(&result).unwrap();
    let back: ReviewResult = serde_json::from_str(&json).unwrap();
    let review = back.review.unwrap();
    let titles: Vec<&str> = review.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["b-warning", "a-blocking", "c-suggestion", "d-blocking"]
    );
}

#[test]
fn review_result_serialization_is_deterministic() {
    // Byte-identical re-publish (DAR §9.2) starts here: serialize →
    // parse → serialize must be a fixed point.
    let result = result_with_findings(vec![
        finding(Severity::Blocking, "x"),
        finding(Severity::Warning, "y"),
    ]);
    let first = serde_json::to_string(&result).unwrap();
    let reparsed: ReviewResult = serde_json::from_str(&first).unwrap();
    let second = serde_json::to_string(&reparsed).unwrap();
    assert_eq!(first, second);
}

// -----------------------------------------------------------------------
// AC5 — enums
// -----------------------------------------------------------------------

#[test]
fn severity_serializes_snake_case_and_rejects_unknown() {
    assert_eq!(
        serde_json::to_string(&Severity::Blocking).unwrap(),
        "\"blocking\""
    );
    assert_eq!(
        serde_json::to_string(&Severity::Warning).unwrap(),
        "\"warning\""
    );
    assert_eq!(
        serde_json::to_string(&Severity::Suggestion).unwrap(),
        "\"suggestion\""
    );
    for bad in ["BLOCKING", "block", "critical", ""] {
        assert!(
            serde_json::from_str::<Severity>(&format!("\"{bad}\"")).is_err(),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn verdict_serializes_snake_case_and_rejects_unknown() {
    assert_eq!(serde_json::to_string(&Verdict::Pass).unwrap(), "\"pass\"");
    assert_eq!(serde_json::to_string(&Verdict::Fail).unwrap(), "\"fail\"");
    for bad in ["passed", "failed", "PASS", ""] {
        assert!(
            serde_json::from_str::<Verdict>(&format!("\"{bad}\"")).is_err(),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn execution_status_serializes_snake_case_and_rejects_unknown() {
    assert_eq!(
        serde_json::to_string(&ExecutionStatus::Success).unwrap(),
        "\"success\""
    );
    assert_eq!(
        serde_json::to_string(&ExecutionStatus::Failure).unwrap(),
        "\"failure\""
    );
    for bad in ["failed", "ok", "Success", ""] {
        assert!(
            serde_json::from_str::<ExecutionStatus>(&format!("\"{bad}\"")).is_err(),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn publication_state_serializes_snake_case_and_rejects_unknown() {
    assert_eq!(
        serde_json::to_string(&PublicationState::Pending).unwrap(),
        "\"pending\""
    );
    assert_eq!(
        serde_json::to_string(&PublicationState::Publishing).unwrap(),
        "\"publishing\""
    );
    assert_eq!(
        serde_json::to_string(&PublicationState::Published).unwrap(),
        "\"published\""
    );
    assert_eq!(
        serde_json::to_string(&PublicationState::FailedRetryable).unwrap(),
        "\"failed_retryable\""
    );
    for bad in ["failed", "FailedRetryable", "failed-retryable", "retryable"] {
        assert!(
            serde_json::from_str::<PublicationState>(&format!("\"{bad}\"")).is_err(),
            "accepted {bad:?}"
        );
    }
}

// -----------------------------------------------------------------------
// AC6 — caps at parse time
// -----------------------------------------------------------------------

#[test]
fn review_summary_cap_enforced() {
    let mut at = result_with_findings(vec![]);
    at.review.as_mut().unwrap().summary = "x".repeat(MAX_REVIEW_SUMMARY_BYTES);
    parse_review_result(&serde_json::to_string(&at).unwrap()).unwrap();

    let mut over = at;
    over.review.as_mut().unwrap().summary = "x".repeat(MAX_REVIEW_SUMMARY_BYTES + 1);
    assert!(parse_review_result(&serde_json::to_string(&over).unwrap()).is_err());
}

#[test]
fn findings_count_cap_enforced() {
    let at = result_with_findings(
        (0..MAX_FINDINGS)
            .map(|i| finding(Severity::Suggestion, &format!("f{i}")))
            .collect(),
    );
    parse_review_result(&serde_json::to_string(&at).unwrap()).unwrap();

    let over = result_with_findings(
        (0..MAX_FINDINGS + 1)
            .map(|i| finding(Severity::Suggestion, &format!("f{i}")))
            .collect(),
    );
    assert!(parse_review_result(&serde_json::to_string(&over).unwrap()).is_err());
}

#[test]
fn finding_field_caps_enforced() {
    let cases: [(&str, usize); 4] = [
        ("title", MAX_FINDING_TITLE_BYTES),
        ("body", MAX_FINDING_BODY_BYTES),
        ("remediation", MAX_FINDING_REMEDIATION_BYTES),
        ("path", MAX_FINDING_PATH_BYTES),
    ];
    for (field, max) in cases {
        // Exactly at the limit passes.
        let mut f = finding(Severity::Warning, "t");
        match field {
            "title" => f.title = "x".repeat(max),
            "body" => f.body = "x".repeat(max),
            "remediation" => f.remediation = Some("x".repeat(max)),
            "path" => f.path = Some("x".repeat(max)),
            _ => unreachable!(),
        }
        let at = result_with_findings(vec![f]);
        parse_review_result(&serde_json::to_string(&at).unwrap())
            .unwrap_or_else(|e| panic!("{field} at limit rejected: {e}"));

        // One over fails.
        let mut f = finding(Severity::Warning, "t");
        match field {
            "title" => f.title = "x".repeat(max + 1),
            "body" => f.body = "x".repeat(max + 1),
            "remediation" => f.remediation = Some("x".repeat(max + 1)),
            "path" => f.path = Some("x".repeat(max + 1)),
            _ => unreachable!(),
        }
        let over = result_with_findings(vec![f]);
        assert!(
            parse_review_result(&serde_json::to_string(&over).unwrap()).is_err(),
            "{field} over limit accepted"
        );
    }
}

#[test]
fn caps_count_bytes_not_chars() {
    // 'é' is two UTF-8 bytes: 128 chars = 256 bytes (at the title
    // limit), 129 chars = 258 bytes (over it). Pins byte semantics.
    let at = result_with_findings(vec![finding(Severity::Warning, &"é".repeat(128))]);
    parse_review_result(&serde_json::to_string(&at).unwrap()).unwrap();

    let over = result_with_findings(vec![finding(Severity::Warning, &"é".repeat(129))]);
    assert!(parse_review_result(&serde_json::to_string(&over).unwrap()).is_err());
}

#[test]
fn parse_rejects_empty_required_strings() {
    let mut f = finding(Severity::Warning, "");
    f.body = "".to_string();
    let bad = result_with_findings(vec![f]);
    assert!(parse_review_result(&serde_json::to_string(&bad).unwrap()).is_err());

    let mut empty_summary = result_with_findings(vec![]);
    empty_summary.review.as_mut().unwrap().summary = String::new();
    assert!(parse_review_result(&serde_json::to_string(&empty_summary).unwrap()).is_err());
}

#[test]
fn review_target_caps_enforced() {
    // At limit passes.
    validate_review_target(&sample_target()).unwrap();

    let cases: [(&str, usize); 4] = [
        ("head_sha", MAX_SHA_BYTES + 1),
        ("base_ref", MAX_REF_BYTES + 1),
        ("owner", MAX_REPO_COMPONENT_BYTES + 1),
        ("repo", MAX_REPO_COMPONENT_BYTES + 1),
    ];
    for (field, over_len) in cases {
        let mut target = sample_target();
        match field {
            "head_sha" => target.head_sha = "a".repeat(over_len),
            "base_ref" => target.base_ref = "b".repeat(over_len),
            "owner" => target.repository.owner = "o".repeat(over_len),
            "repo" => target.repository.repo = "r".repeat(over_len),
            _ => unreachable!(),
        }
        assert!(
            validate_review_target(&target).is_err(),
            "{field} over limit accepted"
        );
    }

    // Empty identity strings are malformed.
    let mut empty = sample_target();
    empty.head_sha = String::new();
    assert!(validate_review_target(&empty).is_err());
}

#[test]
fn review_state_caps_enforced() {
    let mut base: ReviewState = serde_json::from_str(&sample_state_json().to_string()).unwrap();

    base.last_run_id = Some("r".repeat(MAX_RUN_ID_BYTES));
    base.last_publish_error = Some("e".repeat(MAX_PUBLISH_ERROR_BYTES));
    validate_review_state(&base).unwrap();

    base.last_run_id = Some("r".repeat(MAX_RUN_ID_BYTES + 1));
    assert!(validate_review_state(&base).is_err());

    base.last_run_id = Some("r".repeat(MAX_RUN_ID_BYTES));
    base.last_publish_error = Some("e".repeat(MAX_PUBLISH_ERROR_BYTES + 1));
    assert!(validate_review_state(&base).is_err());

    base.last_publish_error = Some(String::new());
    assert!(validate_review_state(&base).is_err());
}
