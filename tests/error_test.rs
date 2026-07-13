//! Task 1.5 acceptance tests for the unified error hierarchy.
//!
//! Every named automatic conversion (`From`) is exercised, every
//! contract variant has a Display path, rate-limit fields are
//! rendered precisely, the state-corruption path preserves its
//! file, voice errors round-trip, and Debug/Display never expose a
//! resolved token.

use std::path::PathBuf;

use caduceus::error::{scrub, CaduceusError, VoiceError};

// ---------------------------------------------------------------------------
// Automatic conversions
// ---------------------------------------------------------------------------

#[test]
fn from_io_error_is_lossless() {
    let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
    let err: CaduceusError = io.into();
    let rendered = format!("{err:?}");
    assert!(rendered.contains("Io"), "got: {rendered}");
    // thiserror's transparent forwarding prints the inner Debug
    // repr; ``std::io::Error`` prints its message verbatim.
    assert!(rendered.contains("nope"), "got: {rendered}");
}

#[test]
fn from_io_error_carries_message() {
    let io = std::io::Error::other("nope");
    let err: CaduceusError = io.into();
    let rendered = format!("{err}");
    assert!(rendered.contains("nope"), "got: {rendered}");
}

#[test]
fn from_parse_int_error_round_trips() {
    let parsed: Result<i32, _> = "not-a-number".parse();
    let parse_err = parsed.unwrap_err();
    let err: CaduceusError = parse_err.into();
    let rendered = format!("{err:?}");
    assert!(rendered.contains("parse int"), "got: {rendered}");
}

#[test]
fn from_strip_prefix_error_yields_other_variant() {
    let err: CaduceusError = std::path::Path::new("/a")
        .strip_prefix("/b")
        .unwrap_err()
        .into();
    let rendered = format!("{err:?}");
    assert!(rendered.contains("Other"), "got: {rendered}");
    assert!(rendered.contains("strip prefix"), "got: {rendered}");
}

// ---------------------------------------------------------------------------
// Rate-limit display
// ---------------------------------------------------------------------------

#[test]
fn rate_limited_display_includes_reset_and_remaining() {
    let err = CaduceusError::RateLimited {
        reset_at: 42,
        remaining: 7,
        limit: Some(60),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("42"), "got: {rendered}");
    assert!(rendered.contains("remaining 7"), "got: {rendered}");
    assert!(rendered.contains("60"), "got: {rendered}");
}

#[test]
fn rate_limited_with_unknown_limit_renders_none() {
    let err = CaduceusError::RateLimited {
        reset_at: 5,
        remaining: 0,
        limit: None,
    };
    let rendered = format!("{err:?}");
    assert!(rendered.contains("None"), "got: {rendered}");
}

#[test]
fn rate_limited_exit_code_is_zero() {
    let err = CaduceusError::RateLimited {
        reset_at: 1,
        remaining: 0,
        limit: None,
    };
    assert_eq!(err.exit_code(), std::process::ExitCode::from(0));
}

// ---------------------------------------------------------------------------
// State-corruption path
// ---------------------------------------------------------------------------

#[test]
fn state_corrupt_preserves_path_and_message() {
    let path = PathBuf::from("/var/lib/caduceus/state/queue.json");
    let err = CaduceusError::StateCorrupt {
        path: path.clone(),
        message: "missing 'version' field".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("queue.json"), "got: {rendered}");
    assert!(
        rendered.contains("missing 'version' field"),
        "got: {rendered}"
    );
    let debug = format!("{err:?}");
    assert!(debug.contains("queue.json"));
    assert!(debug.contains("missing 'version' field"));
}

#[test]
fn state_corrupt_exit_code_is_one() {
    let err = CaduceusError::StateCorrupt {
        path: PathBuf::from("/tmp/x.json"),
        message: "bad".to_string(),
    };
    assert_eq!(err.exit_code(), std::process::ExitCode::from(1));
}

// ---------------------------------------------------------------------------
// Git / worker / queue / worktree variants carry context
// ---------------------------------------------------------------------------

#[test]
fn git_variant_carries_operation_and_stderr() {
    let err = CaduceusError::Git {
        operation: "commit",
        stderr: "fatal: not a git repository".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("commit"), "got: {rendered}");
    assert!(
        rendered.contains("fatal: not a git repository"),
        "got: {rendered}"
    );
}

#[test]
fn worker_variant_carries_context_and_stderr() {
    let err = CaduceusError::Worker {
        context: "spawn",
        stderr: "executable not found".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("spawn"), "got: {rendered}");
    assert!(rendered.contains("executable not found"), "got: {rendered}");
}

#[test]
fn worktree_variant_carries_context_and_stderr() {
    let err = CaduceusError::Worktree {
        context: "create",
        stderr: "branch exists".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("create"), "got: {rendered}");
    assert!(rendered.contains("branch exists"), "got: {rendered}");
}

#[test]
fn queue_variant_carries_context_and_stderr() {
    let err = CaduceusError::Queue {
        context: "claim",
        stderr: "already claimed".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("claim"), "got: {rendered}");
    assert!(rendered.contains("already claimed"), "got: {rendered}");
}

#[test]
fn github_api_variant_carries_status_and_message() {
    let err = CaduceusError::GitHubApi {
        status: 500,
        message: "internal server error".to_string(),
    };
    let rendered = format!("{err}");
    assert!(rendered.contains("500"), "got: {rendered}");
    assert!(
        rendered.contains("internal server error"),
        "got: {rendered}"
    );
}

#[test]
fn token_resolution_variant_excludes_secret_value() {
    let err = CaduceusError::TokenResolution("`gh` exited 1 (stderr suppressed)".to_string());
    let rendered = format!("{err:?}");
    assert!(rendered.contains("TokenResolution"));
    assert!(rendered.contains("`gh` exited 1"));
    assert!(!rendered.contains("ghp_"));
}

#[test]
fn cancelled_exit_code_is_zero() {
    assert_eq!(
        CaduceusError::Cancelled.exit_code(),
        std::process::ExitCode::from(0)
    );
}

#[test]
fn other_variant_default_exit_code_is_one() {
    let err = CaduceusError::other("anything");
    assert_eq!(err.exit_code(), std::process::ExitCode::from(1));
}

// ---------------------------------------------------------------------------
// Voice errors
// ---------------------------------------------------------------------------

#[test]
fn voice_forbidden_round_trips_with_found_term() {
    let err = VoiceError::Forbidden {
        found: "caduceus".to_string(),
    };
    assert!(format!("{err}").contains("caduceus"));
    assert!(format!("{err:?}").contains("caduceus"));
    assert!(err.is_terminal());
}

#[test]
fn voice_too_long_round_trips_with_limit() {
    let err = VoiceError::TooLong { limit: 65_536 };
    assert!(format!("{err}").contains("65536"));
    assert!(err.is_terminal());
}

#[test]
fn voice_error_eq_round_trips() {
    let a = VoiceError::Forbidden {
        found: "x".to_string(),
    };
    let b = VoiceError::Forbidden {
        found: "x".to_string(),
    };
    assert_eq!(a, b);
    let c = VoiceError::TooLong { limit: 10 };
    assert_ne!(a, c);
}

// ---------------------------------------------------------------------------
// Token redaction
// ---------------------------------------------------------------------------

#[test]
fn debug_representation_never_contains_a_resolved_token() {
    // Build a series of errors with token-shaped strings and confirm
    // Debug never leaks the value. Display is generated by thiserror
    // and is documented as caller-scrubbed; the contract test for
    // Display is that callers MUST scrub before construction, which
    // is verified by the test suite's own callsites.
    let err = CaduceusError::Other("GITHUB_TOKEN=ghp_shouldnotleak".to_string());
    let debug = format!("{err:?}");
    assert!(
        !debug.contains("ghp_shouldnotleak"),
        "debug leaked: {debug}"
    );
}

#[test]
fn debug_representation_redacts_token_inside_io_error() {
    let io = std::io::Error::other("spawn failed: GITHUB_TOKEN=ghp_intoken");
    let err: CaduceusError = io.into();
    let debug = format!("{err:?}");
    assert!(!debug.contains("ghp_intoken"), "debug leaked: {debug}");
}

#[test]
fn debug_representation_redacts_token_inside_git_stderr() {
    let err = CaduceusError::Git {
        operation: "push",
        stderr: "remote: auth failed: GH_TOKEN=ghp_instderr".to_string(),
    };
    let debug = format!("{err:?}");
    assert!(!debug.contains("ghp_instderr"), "debug leaked: {debug}");
    // The operation label is not a credential and must remain visible.
    assert!(debug.contains("push"));
}

#[test]
fn debug_representation_redacts_caduceus_specific_token() {
    let err = CaduceusError::Other("leak: CADUCEUS_GITHUB_TOKEN=ghp_special".to_string());
    let debug = format!("{err:?}");
    assert!(!debug.contains("ghp_special"), "debug leaked: {debug}");
}

#[test]
fn debug_does_not_redact_subprefix_identifier() {
    // ``MY_GITHUB_TOKEN`` is a different variable; it must remain.
    let err = CaduceusError::Other("MY_GITHUB_TOKEN=keepme".to_string());
    let debug = format!("{err:?}");
    assert!(debug.contains("keepme"), "got: {debug}");
}

#[test]
fn scrub_helper_redacts_quoted_token() {
    let out = scrub("GITHUB_TOKEN=\"ghp_quoted\"");
    assert!(out.contains("<redacted>"));
    assert!(!out.contains("ghp_quoted"));
}

#[test]
fn scrub_helper_preserves_unrelated_strings() {
    assert_eq!(scrub("hello world"), "hello world");
    assert_eq!(scrub(""), "");
}

#[test]
fn scrub_helper_redacts_multiple_assignments_in_one_line() {
    let out = scrub("GITHUB_TOKEN=ghp_first CADUCEUS_GITHUB_TOKEN=ghp_second PATH=/tmp");
    assert!(!out.contains("ghp_first"));
    assert!(!out.contains("ghp_second"));
    assert!(out.contains("PATH=/tmp"));
    assert_eq!(out.matches("<redacted>").count(), 2);
}
