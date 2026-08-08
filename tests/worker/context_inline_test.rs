//! Integration tests for the worker context builder
//! (`src/worker/context.rs`). Moved out of the inline `#[cfg(test)]`
//! module per AGENTS.md.

use caduceus::context::{
    build_context, decode_context, encode_context, truncate_body_for_tests, BuildInputs,
    CONTEXT_SCHEMA_VERSION, MAX_COMMENT_BODY_BYTES, TRUNCATION_MARKER,
};
use caduceus::github::issue::{IssueComment, IssueEvent};
use caduceus::infra::config::Config;
use caduceus::{IssueDetail, IssueKey};
use chrono::Utc;
use regex::Regex;

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
        labels: vec!["bug".to_string()],
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
        ],
        trusted_comments: vec![IssueComment {
            author: "alice".to_string(),
            body: "first".to_string(),
            created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        }],
        events: vec![IssueEvent {
            kind: "labeled".to_string(),
            actor: "alice".to_string(),
            created_at: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            label_name: Some("bug".to_string()),
        }],
        fetched_at: Utc::now(),
    }
}

fn empty_config() -> Config {
    let mut cfg = Config::test_defaults(std::env::temp_dir().as_path());
    // The trust partition is empty by default — no
    // author is trusted unless explicitly listed. The
    // tests that exercise trusted_comments must add the
    // relevant author to the allowlist.
    cfg.feedback_author_allowlist = Vec::new();
    cfg
}

fn trusted_config(author: &str) -> Config {
    let mut cfg = empty_config();
    cfg.feedback_author_allowlist = vec![author.to_string()];
    cfg
}

#[test]
fn schema_version_is_one() {
    assert_eq!(CONTEXT_SCHEMA_VERSION, 1);
}

#[test]
fn build_empty_context() {
    let mut detail = sample_detail();
    detail.comments.clear();
    detail.trusted_comments.clear();
    detail.events.clear();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    assert_eq!(ctx.schema_version, 1);
    assert_eq!(ctx.comments.len(), 0);
    assert_eq!(ctx.trusted_comments.len(), 0);
    assert_eq!(ctx.events.len(), 0);
    assert!(!ctx.truncation.comments_truncated);
    assert!(!ctx.truncation.trusted_comments_truncated);
    assert!(!ctx.truncation.events_truncated);
}

#[test]
fn build_includes_trusted_comment_in_both_arrays() {
    let detail = sample_detail();
    let ctx = build_context(BuildInputs {
        config: &trusted_config("alice"),
        detail: &detail,
    })
    .expect("build");
    // alice's comment appears in both lists; bob's only
    // in `comments`.
    assert_eq!(ctx.comments.len(), 2);
    assert_eq!(ctx.trusted_comments.len(), 1);
    assert_eq!(ctx.trusted_comments[0].author, "alice");
}

#[test]
fn comments_sorted_chronologically() {
    let mut detail = sample_detail();
    detail.comments = vec![
        IssueComment {
            author: "later".to_string(),
            body: "second".to_string(),
            created_at: chrono::Utc::now(),
        },
        IssueComment {
            author: "earlier".to_string(),
            body: "first".to_string(),
            created_at: chrono::Utc::now() - chrono::Duration::days(1),
        },
    ];
    detail.trusted_comments.clear();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    assert_eq!(ctx.comments[0].author, "earlier");
    assert_eq!(ctx.comments[1].author, "later");
}

#[test]
fn truncate_body_caps_at_max() {
    let big = "x".repeat(MAX_COMMENT_BODY_BYTES + 100);
    let (kept, dropped) = truncate_body_for_tests(&big);
    // The kept prefix is at most MAX_COMMENT_BODY_BYTES
    // bytes; the marker is appended and its length
    // depends on the dropped byte count. The marker is
    // bounded by ~30 chars, so we leave a generous
    // margin here.
    let max_with_marker = MAX_COMMENT_BODY_BYTES + TRUNCATION_MARKER.len() + 30;
    assert!(
        kept.len() <= max_with_marker,
        "kept {} exceeds {}",
        kept.len(),
        max_with_marker
    );
    assert!(dropped >= 100);
    assert!(kept.contains("truncated"));
}

#[test]
fn truncate_body_preserves_short_bodies() {
    let (kept, dropped) = truncate_body_for_tests("hello");
    assert_eq!(kept, "hello");
    assert_eq!(dropped, 0);
}

#[test]
fn truncate_body_handles_unicode() {
    let body: String = "héllo".repeat(MAX_COMMENT_BODY_BYTES / 2 + 10);
    let (kept, dropped) = truncate_body_for_tests(&body);
    assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
    assert!(dropped > 0);
}

#[test]
fn encode_decode_round_trip() {
    let detail = sample_detail();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    let s = encode_context(&ctx).expect("encode");
    let decoded = decode_context(&s).expect("decode");
    assert_eq!(decoded.schema_version, ctx.schema_version);
    assert_eq!(decoded.issue, ctx.issue);
    assert_eq!(decoded.comments.len(), ctx.comments.len());
    assert_eq!(decoded.trusted_comments.len(), ctx.trusted_comments.len());
}

#[test]
fn comments_drops_untrusted_before_trusted_on_size_cap() {
    let mut detail = sample_detail();
    // Each comment body is large enough that 600
    // comments overwhelm the 1 MiB cap (600 × 2 KiB ≈
    // 1.2 MiB raw + JSON overhead). The trusted comment
    // is the *latest* in time, so oldest-first truncation
    // must drop untrusted comments first.
    let big = "u".repeat(2048);
    let mut comments = Vec::new();
    for i in 0..600u32 {
        comments.push(IssueComment {
            author: format!("u{i}"),
            body: big.clone(),
            created_at: chrono::Utc::now() + chrono::Duration::seconds(i as i64),
        });
    }
    // One trusted comment, also large.
    comments.push(IssueComment {
        author: "trusted1".to_string(),
        body: big.clone(),
        created_at: chrono::Utc::now() + chrono::Duration::seconds(1_000_000),
    });
    detail.comments = comments;
    detail.trusted_comments = vec![IssueComment {
        author: "trusted1".to_string(),
        body: big,
        created_at: chrono::Utc::now() + chrono::Duration::seconds(1_000_000),
    }];
    let cfg = trusted_config("trusted1");
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &detail,
    })
    .expect("build");
    // Trusted comment must still be present.
    assert!(
        ctx.trusted_comments.iter().any(|c| c.author == "trusted1"),
        "trusted comment should be preserved when untrusted are dropped first"
    );
    assert!(
        ctx.truncation.dropped_untrusted_comments > 0,
        "expected untrusted comments to be dropped; got {:?}",
        ctx.truncation
    );
}

#[test]
fn events_truncated_when_oversized() {
    // Force events to be huge by writing a very large
    // event payload (the events are reduced oldest-first;
    // we create enough events that even with truncation the
    // test ends up with a flagged truncation metadata).
    let mut detail = sample_detail();
    let big = "e".repeat(8192);
    let mut events = Vec::new();
    for i in 0..600u64 {
        events.push(IssueEvent {
            kind: big.clone(),
            actor: "a".to_string(),
            created_at: chrono::Utc::now() + chrono::Duration::seconds(i as i64),
            label_name: Some(big.clone()),
        });
    }
    detail.events = events;
    detail.comments.clear();
    detail.trusted_comments.clear();
    let ctx = build_context(BuildInputs {
        config: &empty_config(),
        detail: &detail,
    })
    .expect("build");
    assert!(
        ctx.truncation.events_truncated,
        "events_truncated should be true with 600 oversized events"
    );
}

#[test]
fn ignore_pattern_excludes_allowlisted_author() {
    // The contract is: a comment is trusted only if the
    // author is in `feedback_author_allowlist` AND not
    // matched by any ignore regex. An author in the
    // allowlist but also matched by an ignore regex must
    // not appear in `trusted_comments`.
    let mut detail = sample_detail();
    detail.comments.push(IssueComment {
        author: "bot-account".to_string(),
        body: "spammy".to_string(),
        created_at: chrono::Utc::now() + chrono::Duration::seconds(2),
    });
    // Compile a regex that matches `bot-account`.
    let mut cfg = trusted_config("bot-account");
    let re = Regex::new("bot-.*").expect("valid regex");
    cfg.compiled_ignore_patterns = vec![re];
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &detail,
    })
    .expect("build");
    // bot-account's comment must not appear in
    // trusted_comments because the ignore regex matched it.
    assert!(
        !ctx.trusted_comments
            .iter()
            .any(|c| c.author == "bot-account"),
        "ignore regex must exclude author from trusted_comments"
    );
    // But it still appears in `comments` (filter, not partition).
    assert!(ctx.comments.iter().any(|c| c.author == "bot-account"));
}

#[test]
fn invalid_regex_is_rejected_at_config_time() {
    // Use the full config-from-raw path with a Hermes
    // context that pre-resolves the worker command so the
    // standalone-install check does not fire.
    let raw_config = caduceus::infra::config::RawConfig {
        comment_ignore_patterns: Some(vec!["[invalid".to_string()]),
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        ..Default::default()
    };
    let ctx_holder = caduceus::infra::config::LoadContext {
        plugin_root: Some(std::env::temp_dir()),
        ..Default::default()
    };
    let err = caduceus::infra::config::Config::from_raw(raw_config, &ctx_holder)
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("invalid regex") || msg.contains("comment_ignore_patterns"),
        "expected regex error, got: {msg}"
    );
}

#[test]
fn comment_ignore_regex_in_json_documents_correct_field() {
    // End-to-end: a regex that compiles correctly is
    // reflected in the JSON document; the contract says
    // the *compiled* regexes are used at fetch time, and
    // here at build time we apply them again to filter
    // `trusted_comments`. An invalid regex is a config
    // error, never silently dropped.
    let detail = sample_detail();
    let mut cfg = trusted_config("alice");
    // Manually attach a compiled regex that matches alice.
    let re = Regex::new("^alice$").unwrap();
    cfg.compiled_ignore_patterns = vec![re];
    let ctx = build_context(BuildInputs {
        config: &cfg,
        detail: &detail,
    })
    .expect("build");
    // alice's comment is matched by `^alice$`, so it must
    // not appear in trusted_comments.
    assert!(!ctx.trusted_comments.iter().any(|c| c.author == "alice"));
}
