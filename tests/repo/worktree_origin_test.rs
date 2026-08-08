//! Tests for the worktree git-runner text contracts and the
//! workdir-base clone path derivation (moved from
//! `src/worktree/testing.rs`). The origin-parsing and host-validation
//! tests from that module live in `repository_discovery_test.rs`.

use std::path::PathBuf;

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::worktree::{cap_text_for_tests, clone_path, redact_and_cap_for_tests};

/// Minimal config for the pure-logic tests in this file. Only the
/// workdir base matters for `clone_path`.
fn minimal_config() -> Config {
    let mut cfg = Config::test_defaults(std::path::Path::new("/tmp"));
    cfg.workdir_base = PathBuf::from("/srv/workdirs");
    cfg
}

#[test]
fn cap_text_truncates_with_marker() {
    let huge = "x".repeat(caduceus::worktree::GIT_OUTPUT_BYTE_CAP + 100);
    let capped = cap_text_for_tests(huge.as_bytes());
    assert!(capped.contains("truncated"));
    assert!(capped.len() <= caduceus::worktree::GIT_OUTPUT_BYTE_CAP + 64);
}

#[test]
fn redact_and_cap_strips_token_shaped_substrings() {
    let raw = b"some output\nGITHUB_TOKEN=ghp_should_not_leak\nrest";
    let redacted = redact_and_cap_for_tests(raw);
    assert!(redacted.contains("<redacted>"));
    assert!(!redacted.contains("ghp_should_not_leak"));
}

#[test]
fn clone_path_is_workdir_base_plus_owner_plus_repo() {
    let cfg = minimal_config();
    let key = IssueKey {
        owner: "octocat".to_string(),
        repo: "Hello-World".to_string(),
        number: 1,
    };
    assert_eq!(
        clone_path(&cfg, &key),
        PathBuf::from("/srv/workdirs/octocat/Hello-World")
    );
}
