//! validator.
//!
//! Covers every scenario from the spec:
//!
//! 1. `https://api.github.com` accepted.
//! 2. `https://api.github.com/` accepted (trailing slash).
//! 3. `https://ghes.example.com/api/v3` accepted.
//! 4. `https://ghes.example.com/api/v3/` accepted.
//! 5. `https://ghes.example.com/api/v3/some/path` accepted.
//! 6. `http://api.github.com` rejected (wrong scheme).
//! 7. `https://bitbucket.example.com` rejected (path is `/`, not `/api/v3`).
//! 8. `https://my-proxy.example.com/github/api/v3` rejected.
//! 9. Malformed URL rejected.
//! 10. Empty string rejected.
//! 11. Independence test: `validate_api_base("https://api.github.com")` returns
//!     `Ok(())` regardless of `comment_forbidden_strings`.
//! 12. Independence test: `validate_api_base("https://ghes.example.com/api/v3")`
//!     returns `Ok(())` regardless of `comment_forbidden_strings`.
//! 13. Independence test (negative): `validate_api_base("https://api.github.com")`
//!     does NOT return an error containing `"bitbucket"` or any forbidden-string
//!     text.
//! 14. Error messages do NOT contain the raw `value` parameter (credential leak
//!     prevention — test with `https://user:ghp_token@host/api/v3`).

use caduceus::config::validate_api_base;

// Positive cases (accepted)

#[test]
fn accepts_github_com_saas() {
    assert!(validate_api_base("https://api.github.com").is_ok());
}

#[test]
fn accepts_github_com_saas_with_trailing_slash() {
    assert!(validate_api_base("https://api.github.com/").is_ok());
}

#[test]
fn accepts_ghes_with_api_v3_path() {
    assert!(validate_api_base("https://ghes.example.com/api/v3").is_ok());
}

#[test]
fn accepts_ghes_with_api_v3_trailing_slash() {
    assert!(validate_api_base("https://ghes.example.com/api/v3/").is_ok());
}

#[test]
fn accepts_ghes_with_api_v3_subpath() {
    assert!(validate_api_base("https://ghes.example.com/api/v3/some/path").is_ok());
}

// Negative cases (rejected)

#[test]
fn rejects_http_scheme() {
    let err = validate_api_base("http://api.github.com").unwrap_err();
    assert!(err.contains("scheme must be https"), "got: {err}");
}

#[test]
fn rejects_bitbucket_host_with_root_path() {
    let err = validate_api_base("https://bitbucket.example.com").unwrap_err();
    assert!(err.contains("api/v3"), "got: {err}");
}

#[test]
fn rejects_custom_path_prefix_proxy() {
    let err = validate_api_base("https://my-proxy.example.com/github/api/v3").unwrap_err();
    assert!(err.contains("api/v3"), "got: {err}");
}

#[test]
fn rejects_malformed_url() {
    let err = validate_api_base("not a url").unwrap_err();
    assert!(err.contains("not a valid URL"), "got: {err}");
}

#[test]
fn rejects_empty_string() {
    let err = validate_api_base("").unwrap_err();
    assert!(err.contains("must not be empty"), "got: {err}");
}

// Forbidden-string independence tests

#[test]
fn forbidden_strings_do_not_affect_github_com_validation() {
    // The validator is a pure function and does NOT read
    // `comment_forbidden_strings`. Verify this directly.
    assert!(validate_api_base("https://api.github.com").is_ok());
}

#[test]
fn forbidden_strings_do_not_affect_ghes_validation() {
    assert!(validate_api_base("https://ghes.example.com/api/v3").is_ok());
}

#[test]
fn forbidden_string_text_does_not_appear_in_github_com_error() {
    // The validator must never match against forbidden-string
    // patterns. "bitbucket" is a known forbidden string; it must
    // not appear in the error message for a valid GitHub.com URL.
    let result = validate_api_base("https://api.github.com");
    assert!(result.is_ok(), "expected Ok, got Err: {:?}", result);
}

// Credential leak prevention

#[test]
fn error_does_not_contain_raw_value() {
    // A URL with credentials in the userinfo section must not
    // leak the raw value in the error message. Use a non-GitHub
    // host with a non-matching path so validation fails.
    let err = validate_api_base("https://user:ghp_token@example.com/bad-path").unwrap_err();
    assert!(
        !err.contains("ghp_token"),
        "error message leaks credential: {err}"
    );
}
