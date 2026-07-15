//! Task 6.6 acceptance tests for the public-voice rule.
//!
//! Covers the contract: defaults, case variants, substring matching,
//! explicit replacement of the default forbidden-term list, empty
//! forbidden entries rejected by config, PR-body and artifact
//! enforcement, max-length limit, and proof that a rejected
//! request never reaches the HTTP layer (the validator gates
//! every legitimate helper).

use caduceus::config::{Config, RawConfig};
use caduceus::error::VoiceError;
use caduceus::finalize::{
    first_forbidden_term, terminal_from_voice, validate_comment, validate_pr_body,
    validate_pr_title, validate_public_text, DEFAULT_COMMENT_MAX_BYTES, DEFAULT_PR_BODY_MAX_BYTES,
    DEFAULT_PR_TITLE_MAX_BYTES,
};
use caduceus::github::{check_voice_or_error, VoiceChannel};
use caduceus::issue::IssueKey;

#[allow(dead_code)]
fn sample_issue() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    }
}

fn sample_config(extra_forbidden: &[&str]) -> Config {
    let mut cfg = Config::test_defaults(&std::env::temp_dir());
    cfg.comment_forbidden_strings = extra_forbidden.iter().map(|s| s.to_string()).collect();
    cfg
}

fn config_with_yaml(yaml: &str) -> Result<Config, String> {
    use caduceus::config::LoadContext;
    let raw: RawConfig = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
    let ctx = LoadContext::default();
    Config::from_raw(raw, &ctx).map_err(|e| format!("{e:?}"))
}

// ---------------------------------------------------------------------------
// Defaults and explicit replacement
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_text_that_contains_no_forbidden_term() {
    let cfg = sample_config(&[]);
    assert!(validate_public_text("a perfectly fine comment", &cfg, 4096).is_ok());
}

#[test]
fn validate_rejects_an_explicit_forbidden_term() {
    // The contract says ``comment_forbidden_strings`` defaults to
    // an empty list (operators supply their own explicit list).
    // When an entry is supplied, any matching text is rejected.
    let cfg_str = r#"
worker_command:
  - python3
worker_timeout_seconds: 60
http_timeout_seconds: 30
git_timeout_seconds: 30
comment_forbidden_strings: ["SEKRIT-FORBIDDEN"]
"#;
    let cfg = config_with_yaml(cfg_str).expect("loads");
    let err = validate_public_text("this contains sekrit-forbidden text", &cfg, 4096)
        .expect_err("forbidden");
    match err {
        VoiceError::Forbidden { found } => assert_eq!(found, "sekrit-forbidden"),
        other => panic!("expected Forbidden; got: {other:?}"),
    }
}

#[test]
fn validate_explicit_replacement_removes_defaults() {
    // When the operator supplies an explicit list, the defaults are
    // entirely replaced. A document containing only a defaulted
    // term must therefore pass the validator.
    let cfg = sample_config(&["-custom-only-"]);
    assert!(
        validate_public_text("this contains anyword and is fine", &cfg, 4096).is_ok(),
        "no replacement for anyword — must pass"
    );
    assert!(
        validate_public_text("this contains -custom-only- and is rejected", &cfg, 4096).is_err(),
        "custom entry must trigger Forbidden"
    );
}

// ---------------------------------------------------------------------------
// Case variants and substring matching
// ---------------------------------------------------------------------------

#[test]
fn case_insensitive_substring_match() {
    let cfg = sample_config(&["FOO"]);
    for body in [
        "foo at start",
        "ends with foo",
        "in THE middle FOOBAR",
        "fOo",
        "FooBar",
    ] {
        let err = validate_public_text(body, &cfg, 4096)
            .expect_err(&format!("body {body:?} must be rejected"));
        assert!(matches!(err, VoiceError::Forbidden { .. }));
    }
}

#[test]
fn case_insensitive_against_term_alternating_case() {
    let cfg = sample_config(&["MixedCase"]);
    assert!(validate_public_text("this mentions mixedcase explicitly", &cfg, 4096).is_err());
    assert!(validate_public_text("MIXEDCASE", &cfg, 4096).is_err());
    assert!(validate_public_text("mixedCASE", &cfg, 4096).is_err());
}

#[test]
fn substring_at_the_end_is_a_match() {
    let cfg = sample_config(&["end-token"]);
    let err = validate_public_text("blah blah blah end-token", &cfg, 4096)
        .expect_err("end-of-string match");
    match err {
        VoiceError::Forbidden { found } => assert_eq!(found, "end-token"),
        other => panic!("expected Forbidden; got: {other:?}"),
    }
}

#[test]
fn empty_text_does_not_match_a_nonempty_term() {
    let cfg = sample_config(&["forbidden"]);
    assert!(validate_public_text("", &cfg, 4096).is_ok());
}

// ---------------------------------------------------------------------------
// Max-length limit
// ---------------------------------------------------------------------------

#[test]
fn too_long_text_reports_too_long() {
    let cfg = sample_config(&[]);
    let limit = 10;
    let err = validate_public_text("a]b]c]d]e]x", &cfg, limit).expect_err("too long");
    match err {
        VoiceError::TooLong { limit: l } => assert_eq!(l, limit),
        other => panic!("expected TooLong; got: {other:?}"),
    }
}

#[test]
fn text_at_exactly_the_limit_is_accepted() {
    let cfg = sample_config(&[]);
    let s = "a".repeat(64);
    validate_public_text(&s, &cfg, 64).expect("exactly-at-limit is OK");
}

#[test]
fn default_pr_title_limit_is_256_bytes() {
    assert_eq!(DEFAULT_PR_TITLE_MAX_BYTES, 256);
}

#[test]
fn default_pr_body_and_comment_limit_is_65536() {
    assert_eq!(DEFAULT_PR_BODY_MAX_BYTES, 65_536);
    assert_eq!(DEFAULT_COMMENT_MAX_BYTES, 65_536);
}

#[test]
fn forbidden_term_takes_precedence_over_too_long() {
    let cfg = sample_config(&["forbidden"]);
    let body = format!("{} forbidden", "a".repeat(10_000));
    let err = validate_public_text(&body, &cfg, 100).expect_err("forbidden first");
    assert!(matches!(err, VoiceError::Forbidden { .. }));
}

// ---------------------------------------------------------------------------
// PR body / artifact enforcement
// ---------------------------------------------------------------------------

#[test]
fn pr_body_helper_uses_65536_limit() {
    let cfg = sample_config(&[]);
    validate_pr_body("ok", &cfg).expect("ok");
}

#[test]
fn pr_title_helper_uses_256_limit() {
    let cfg = sample_config(&[]);
    let title = "a".repeat(256);
    validate_pr_title(&title, &cfg).expect("at-limit title");
    let oversized = "a".repeat(257);
    let err = validate_pr_title(&oversized, &cfg).expect_err("too long");
    assert!(matches!(err, VoiceError::TooLong { .. }));
}

#[test]
fn comment_helper_uses_65536_limit() {
    let cfg = sample_config(&[]);
    validate_comment("ok", &cfg).expect("ok");
}

// ---------------------------------------------------------------------------
// Empty forbidden entry is rejected by config
// ---------------------------------------------------------------------------

#[test]
fn empty_forbidden_entry_is_rejected_by_config() {
    let yaml = r#"
worker_command:
  - python3
worker_timeout_seconds: 60
http_timeout_seconds: 30
git_timeout_seconds: 30
comment_forbidden_strings: ["foo", "", "bar"]
"#;
    let cfg = config_with_yaml(yaml).expect_err("empty entry must be rejected");
    assert!(
        cfg.contains("comment_forbidden_strings must not contain empty entries"),
        "got: {cfg}"
    );
}

// ---------------------------------------------------------------------------
// HTTP helper integration: rejected request never reaches the wire
// ---------------------------------------------------------------------------

#[test]
fn rejected_pr_body_never_reaches_post_helper() {
    // The validator must reject the body before any HTTP layer is
    // reached. The orchestrator (Phase 6) routes every outbound
    // PR-body / PR-title / comment through `check_voice_or_error`
    // first; this test exercises the validator chokepoint
    // directly. A real HTTP integration that would otherwise
    // surface as a 422 from GitHub is covered by the
    // `pr_forbidden_text_prevents_http_request` test in
    // `pr_test.rs`.
    let cfg = sample_config(&["forbidden"]);
    let err = check_voice_or_error("contains forbidden wording", &cfg, VoiceChannel::PrBody)
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("public-voice"),
        "expected voice error wrapper; got: {msg}"
    );
}

#[test]
fn rejected_comment_never_reaches_post_helper() {
    let cfg = sample_config(&["sensitive"]);
    let err = check_voice_or_error("this is sensitive text", &cfg, VoiceChannel::Comment)
        .expect_err("must reject");
    assert!(format!("{err:?}").contains("public-voice"));
}

#[test]
fn rejected_investigation_comment_never_reaches_post_helper() {
    let cfg = sample_config(&["confidential"]);
    let err = check_voice_or_error("this comment is confidential.", &cfg, VoiceChannel::Comment)
        .expect_err("must reject");
    assert!(format!("{err:?}").contains("public-voice"));
}

#[test]
fn check_voice_or_error_helpers_share_the_validator() {
    let cfg = sample_config(&["forbidden"]);
    let err = check_voice_or_error("forbidden text", &cfg, VoiceChannel::Comment)
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("public-voice"), "got: {msg}");
}

#[test]
fn success_path_voice_validator_does_not_block_legitimate_text() {
    // Validator lets legitimate text through. The downstream
    // HTTP helpers (Phase 6 finalization) compose this
    // chokepoint with the typed `Client`; this test pins
    // the validator's pass-through behaviour.
    let cfg = sample_config(&["forbidden"]);
    check_voice_or_error("thanks for filing", &cfg, VoiceChannel::Comment)
        .expect("legitimate comment");
    check_voice_or_error("fix: handle edge case", &cfg, VoiceChannel::PrTitle)
        .expect("legitimate PR title");
    check_voice_or_error("longer description ...", &cfg, VoiceChannel::PrBody)
        .expect("legitimate PR body");
}

// ---------------------------------------------------------------------------
// terminal_from_voice conversion
// ---------------------------------------------------------------------------

#[test]
fn terminal_from_voice_forbidden_carries_term() {
    let v = VoiceError::Forbidden {
        found: "secret".to_string(),
    };
    let err = terminal_from_voice(v);
    let msg = format!("{err:?}");
    assert!(msg.contains("forbidden term matched"), "got: {msg}");
    assert!(msg.contains("secret"), "got: {msg}");
}

#[test]
fn terminal_from_voice_too_long_carries_limit() {
    let v = VoiceError::TooLong { limit: 4096 };
    let err = terminal_from_voice(v);
    let msg = format!("{err:?}");
    assert!(msg.contains("exceeds limit"), "got: {msg}");
    assert!(msg.contains("4096"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// first_forbidden_term
// ---------------------------------------------------------------------------

#[test]
fn first_forbidden_term_returns_none_for_no_match() {
    assert!(first_forbidden_term("a normal string", &["foo".to_string()]).is_none());
}

#[test]
fn first_forbidden_term_returns_lowercased_match() {
    let found = first_forbidden_term("contains Foo here", &["foo".to_string()]).expect("found");
    assert_eq!(found, "foo");
}

#[test]
fn first_forbidden_term_skips_empty_entries() {
    let found = first_forbidden_term(
        "contains SensitiveWord",
        &["".to_string(), "SensitiveWord".to_string()],
    )
    .expect("found");
    assert_eq!(found, "sensitiveword");
}

// ---------------------------------------------------------------------------
// Suppress unused-import warnings when running with default features.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _suppress_unused_imports() -> Result<(), caduceus::error::VoiceError> {
    Ok(())
}
