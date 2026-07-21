//! Task 5.5 acceptance tests for audit redaction and credential
//! leak prevention in `validate_api_base` error messages.
//!
//! Test cases:
//! 1. `validate_api_base` error for `https://user:ghp_token@host/api/v3`
//!    does NOT contain `ghp_token`.
//! 2. `validate_api_base` error for `https://user:github_pat_token@host/api/v3`
//!    does NOT contain `github_pat_token`.
//! 3. `validate_api_base` error for malformed URL does NOT contain
//!    credentials.
//! 4. `scrub` removes `GITHUB_TOKEN=ghp_...` from input.
//! 5. `scrub` removes `CADUCEUS_GITHUB_TOKEN=...` from input.
//! 6. `scrub` removes `GH_TOKEN=...` from input.
//! 7. `scrub` removes `GITHUB_TOKEN="ghp_..."` (quoted value) from input.

use caduceus::config::validate_api_base;
use caduceus::error::scrub;

// ---------------------------------------------------------------------------
// validate_api_base error messages do not leak credentials
// ---------------------------------------------------------------------------

#[test]
fn validate_api_base_error_does_not_contain_ghp_token() {
    // The URL contains a ghp_ token in the userinfo section and
    // has a bad path so validation fails. The error must not
    // contain the raw ghp_ value because validate_api_base never
    // includes the raw input in its error messages.
    let err = validate_api_base("https://user:ghp_abc123@example.com/bad-path").unwrap_err();
    assert!(
        !err.contains("ghp_abc123"),
        "error message leaks ghp_ token: {err}"
    );
    assert!(
        !err.contains("ghp_"),
        "error message leaks ghp_ prefix: {err}"
    );
}

#[test]
fn validate_api_base_error_does_not_contain_github_pat_token() {
    let err = validate_api_base("https://user:github_pat_abc@example.com/bad-path").unwrap_err();
    assert!(
        !err.contains("github_pat_abc"),
        "error message leaks github_pat_ token: {err}"
    );
}

#[test]
fn validate_api_base_error_for_malformed_url_does_not_leak_credentials() {
    // A malformed URL that happens to contain credential-shaped
    // text must not leak it.
    let err = validate_api_base("ghp_token@bad").unwrap_err();
    assert!(
        !err.contains("ghp_token"),
        "error message leaks credential from malformed URL: {err}"
    );
}

// ---------------------------------------------------------------------------
// scrub helper tests
// ---------------------------------------------------------------------------

#[test]
fn scrub_removes_ghp_token_after_github_token_assignment() {
    // The scrub function recognises `GITHUB_TOKEN=...` and redacts
    // the value including ghp_ prefixed tokens.
    let input = "GITHUB_TOKEN=ghp_abc123";
    let result = scrub(input);
    assert!(
        !result.contains("ghp_abc123"),
        "scrub did not remove ghp_ token: {result}"
    );
    assert!(
        result.contains("<redacted>"),
        "scrub did not add redaction marker: {result}"
    );
}

#[test]
fn scrub_removes_github_pat_after_caduceus_github_token_assignment() {
    let input = "CADUCEUS_GITHUB_TOKEN=github_pat_abc";
    let result = scrub(input);
    assert!(
        !result.contains("github_pat_abc"),
        "scrub did not remove github_pat_ token: {result}"
    );
    assert!(
        result.contains("<redacted>"),
        "scrub did not add redaction marker: {result}"
    );
}

#[test]
fn scrub_removes_token_after_gh_token_assignment() {
    let input = "GH_TOKEN=some_value";
    let result = scrub(input);
    assert!(
        result.contains("<redacted>"),
        "scrub did not redact GH_TOKEN value: {result}"
    );
}

#[test]
fn scrub_removes_quoted_github_token_value() {
    let input = "GITHUB_TOKEN=\"ghp_xyz\"";
    let result = scrub(input);
    assert!(
        !result.contains("ghp_xyz"),
        "scrub did not remove quoted ghp_ token: {result}"
    );
    assert!(
        result.contains("<redacted>"),
        "scrub did not add redaction marker: {result}"
    );
}
