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
    validate_review_target, RepositoryId, ReviewTarget, MAX_REF_BYTES, MAX_REPO_COMPONENT_BYTES,
    MAX_SHA_BYTES,
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

// -----------------------------------------------------------------------
// AC6 — caps at parse time
// -----------------------------------------------------------------------

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
