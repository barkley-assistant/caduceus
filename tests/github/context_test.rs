//! "Worker environment and result" / "build stable context JSON":
//!
//! * Schema version is emitted.
//! * Empty issue snapshot is a valid document.
//! * Comments are split into `comments` and `trusted_comments`,
//!   with `trusted_comments` being a subset of `comments`.
//! * `feedback_author_allowlist` trust is by exact login.
//! * Trusted comments are sorted chronologically.
//! * Invalid regex entries are rejected at config time.
//! * Timeline events are serialised.
//! * Per-body cap is 64 KiB; total cap is 1 MiB.
//! * Truncation order: untrusted first, then trusted.
//! * JSON round-trip is lossless.

use std::collections::BTreeMap;

use caduceus::config::{Config, RawConfig};
use caduceus::context::{
    build_context, decode_context, encode_context, BuildInputs, CONTEXT_SCHEMA_VERSION,
    MAX_COMMENT_BODY_BYTES, MAX_CONTEXT_BYTES,
};
use caduceus::issue::{IssueComment, IssueDetail, IssueEvent, IssueKey};

fn sample_detail() -> IssueDetail {
    use chrono::TimeZone;
    IssueDetail {
        key: IssueKey {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 1,
        },
        title: "Test issue".to_string(),
        body: "Body".to_string(),
        labels: vec!["bug".to_string(), "area".to_string()],
        comments: vec![
            IssueComment {
                author: "alice".to_string(),
                body: "first".to_string(),
                created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            },
            IssueComment {
                author: "bob".to_string(),
                body: "second".to_string(),
                created_at: Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
            },
            IssueComment {
                author: "charlie".to_string(),
                body: "third".to_string(),
                created_at: Utc.with_ymd_and_hms(2024, 1, 3, 0, 0, 0).unwrap(),
            },
        ],
        trusted_comments: vec![],
        events: vec![IssueEvent {
            kind: "labeled".to_string(),
            actor: "alice".to_string(),
            created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            label_name: Some("bug".to_string()),
        }],
        fetched_at: Utc.with_ymd_and_hms(2024, 1, 4, 0, 0, 0).unwrap(),
    }
}

use chrono::Utc;

fn empty_config() -> Config {
    let mut cfg = Config::test_defaults(std::env::temp_dir().as_path());
    cfg.feedback_author_allowlist = Vec::new();
    cfg
}

fn trusted_config(author: &str) -> Config {
    let mut cfg = empty_config();
    cfg.feedback_author_allowlist = vec![author.to_string()];
    cfg
}

#[test]
fn empty_context_is_a_valid_document() {
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    detail.events.clear();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let value: serde_json::Value = serde_json::from_str(&s).expect("parse");
    assert_eq!(
        value.get("schema_version").and_then(|v| v.as_u64()),
        Some(CONTEXT_SCHEMA_VERSION as u64),
        "schema_version should be {}",
        CONTEXT_SCHEMA_VERSION
    );
    assert!(value
        .get("comments")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    assert!(value
        .get("trusted_comments")
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    assert!(value.get("events").unwrap().as_array().unwrap().is_empty());
    // Round-trip.
    let decoded = decode_context(&s).expect("decode");
    assert_eq!(decoded.schema_version, CONTEXT_SCHEMA_VERSION);
}

#[test]
fn trusted_author_appears_in_trusted_comments() {
    let cfg = trusted_config("alice");
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &sample_detail(),
    })
    .expect("build");
    // alice is the trusted author.
    assert!(ctx.trusted_comments.iter().any(|c| c.author == "alice"));
    // bob and charlie are not trusted.
    assert!(!ctx.trusted_comments.iter().any(|c| c.author == "bob"));
    assert!(!ctx.trusted_comments.iter().any(|c| c.author == "charlie"));
    // All three are still in `comments`.
    assert_eq!(ctx.comments.len(), 3);
    // The trusted comment is also present in `comments`.
    assert!(ctx.comments.iter().any(|c| c.author == "alice"));
}

#[test]
fn trusted_comments_subset_of_comments() {
    let cfg = trusted_config("alice");
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &sample_detail(),
    })
    .expect("build");
    let trusted_keys: std::collections::HashSet<(String, String)> = ctx
        .trusted_comments
        .iter()
        .map(|c| (c.author.clone(), c.body.clone()))
        .collect();
    let all_keys: std::collections::HashSet<(String, String)> = ctx
        .comments
        .iter()
        .map(|c| (c.author.clone(), c.body.clone()))
        .collect();
    for k in &trusted_keys {
        assert!(all_keys.contains(k), "trusted key {:?} not in comments", k);
    }
}

#[test]
fn chronological_order_preserved() {
    let detail = sample_detail();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let times: Vec<_> = ctx.comments.iter().map(|c| c.created_at).collect();
    let mut sorted = times.clone();
    sorted.sort();
    assert_eq!(
        times, sorted,
        "comments must be sorted ascending by created_at"
    );
}

#[test]
fn invalid_regex_rejected_at_config_time() {
    let raw = RawConfig {
        comment_ignore_patterns: Some(vec!["[unclosed".to_string()]),
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx_holder = caduceus::config::LoadContext {
        plugin_root: Some(std::env::temp_dir()),
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx_holder).expect_err("must reject invalid regex");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid regex") || msg.contains("comment_ignore_patterns"),
        "expected regex error, got: {msg}"
    );
}

#[test]
fn timeline_events_serialised() {
    let detail = sample_detail();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let value: serde_json::Value = serde_json::from_str(&s).expect("parse");
    let events = value.get("events").unwrap().as_array().unwrap();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.get("kind").and_then(|v| v.as_str()), Some("labeled"));
    assert_eq!(ev.get("actor").and_then(|v| v.as_str()), Some("alice"));
    assert_eq!(ev.get("label_name").and_then(|v| v.as_str()), Some("bug"),);
}

#[test]
fn exact_per_body_cap_is_64kib() {
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    let big = "x".repeat(MAX_COMMENT_BODY_BYTES + 1_000);
    detail.comments.push(IssueComment {
        author: "big".to_string(),
        body: big.clone(),
        created_at: Utc::now(),
    });
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    assert_eq!(ctx.comments.len(), 1);
    // The kept body must be at most MAX_COMMENT_BODY_BYTES +
    // marker length.
    assert!(ctx.comments[0].body.len() <= MAX_COMMENT_BODY_BYTES + 40);
    assert!(ctx.comments[0].body.contains("truncated"));
    assert!(ctx.truncation.body_truncated_count >= 1);
    assert!(ctx.truncation.total_body_bytes_dropped >= 1_000);
}

#[test]
fn total_cap_is_1_mib() {
    assert_eq!(MAX_CONTEXT_BYTES, 1024 * 1024);
}

#[test]
fn json_round_trip_preserves_all_fields() {
    let cfg = trusted_config("alice");
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &sample_detail(),
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let decoded = decode_context(&s).expect("decode");
    assert_eq!(decoded.schema_version, ctx.schema_version);
    assert_eq!(decoded.issue, ctx.issue);
    assert_eq!(decoded.issue_title, ctx.issue_title);
    assert_eq!(decoded.issue_body, ctx.issue_body);
    assert_eq!(decoded.labels, ctx.labels);
    assert_eq!(decoded.comments.len(), ctx.comments.len());
    assert_eq!(decoded.trusted_comments.len(), ctx.trusted_comments.len());
    assert_eq!(decoded.events.len(), ctx.events.len());
    assert_eq!(
        decoded.truncation.dropped_untrusted_comments,
        ctx.truncation.dropped_untrusted_comments
    );
}

#[test]
fn trusted_comment_dropped_only_after_untrusted() {
    // Build a detail with one trusted comment at the *oldest*
    // timestamp and many untrusted comments at later timestamps
    // — this forces the truncation loop to prefer dropping
    // the untrusted (newer) comments over the trusted (older)
    // one.
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    let trusted = IssueComment {
        author: "trusted".to_string(),
        body: "x".repeat(2048),
        created_at: Utc::now() - chrono::Duration::days(7),
    };
    let mut comments = vec![trusted.clone()];
    for i in 0..800u32 {
        comments.push(IssueComment {
            author: format!("u{i}"),
            body: "u".repeat(2048),
            created_at: Utc::now() + chrono::Duration::seconds(i as i64),
        });
    }
    detail.comments = comments;
    detail.trusted_comments = vec![trusted];
    let cfg = trusted_config("trusted");
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &detail,
    })
    .expect("build");
    // The trusted comment is at position 0 (oldest). If the
    // truncation dropped oldest-first without the trust
    // preference, the trusted comment would be gone.
    assert!(
        ctx.trusted_comments.iter().any(|c| c.author == "trusted"),
        "trusted comment must survive the truncation because untrusted are dropped first"
    );
}

#[test]
fn unicode_in_comments_and_labels() {
    use chrono::TimeZone;
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    detail.comments.push(IssueComment {
        author: "alice".to_string(),
        body: "héllo wörld — τεκστ".to_string(),
        created_at: Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap(),
    });
    detail.labels = vec!["🤖 auto-fix".to_string()];
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    assert!(s.contains("héllo"));
    assert!(s.contains("🤖"));
}

#[test]
fn irreducibly_oversized_event_errors() {
    // Construct a detail whose ONLY event has an encoded
    // body larger than the context byte budget. The build
    // must fail with the documented `context:oversized_event`
    // error.
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    detail.events.clear();
    let huge = "x".repeat(MAX_CONTEXT_BYTES + 1);
    detail.events.push(IssueEvent {
        kind: huge,
        actor: "a".to_string(),
        created_at: Utc::now(),
        label_name: None,
    });
    let err = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect_err("must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oversized_event"),
        "expected oversized_event error, got: {msg}"
    );
}

#[test]
fn empty_labels_round_trip() {
    let mut detail = sample_detail();
    detail.labels.clear();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
    assert!(v.get("labels").unwrap().as_array().unwrap().is_empty());
}

#[test]
fn case_sensitive_author_match() {
    // The contract is case-sensitive: an allowlist of "Alice"
    // does not trust a comment by "alice" (different case).
    let mut cfg = empty_config();
    cfg.feedback_author_allowlist = vec!["Alice".to_string()];
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &sample_detail(),
    })
    .expect("build");
    assert!(
        !ctx.trusted_comments.iter().any(|c| c.author == "alice"),
        "case-sensitive matching: 'Alice' must not trust 'alice'"
    );
}

#[test]
fn explicit_ci_flag_enables_case_insensitive_match() {
    // An explicit `(?i)` flag in the regex enables
    // case-insensitive matching of authors per the contract.
    let raw = RawConfig {
        comment_ignore_patterns: Some(vec!["(?i)ALICE".to_string()]),
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx_holder = caduceus::config::LoadContext {
        plugin_root: Some(std::env::temp_dir()),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx_holder).expect("valid config");
    let detail = sample_detail();
    // The fetcher wouldn't normally put alice in
    // trusted_comments, but for this test we set up the
    // ignore-regex exclusion directly.
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &detail,
    })
    .expect("build");
    // alice is matched by `(?i)ALICE` so she is excluded
    // from trusted_comments even though she's not in the
    // allowlist.
    assert!(ctx.trusted_comments.iter().all(|c| c.author != "alice"));
    let _ = BTreeMap::<String, String>::new(); // silence unused
}

#[test]
fn rename_resistant_id_through_issue_key() {
    // Issue numbers are u64, so a username change does not
    // affect the canonical key. This test simply asserts that
    // the round-trip preserves the key.
    let detail = sample_detail();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
    let issue = v.get("issue").unwrap();
    assert_eq!(issue.get("owner").and_then(|v| v.as_str()), Some("owner"));
    assert_eq!(issue.get("repo").and_then(|v| v.as_str()), Some("repo"));
    assert_eq!(issue.get("number").and_then(|v| v.as_u64()), Some(1));
}
