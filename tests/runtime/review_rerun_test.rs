//! Pure matcher + trust-check tests for the trusted-comment re-review
//! listener (issue #335, DAR §17). No I/O — the async step is tested
//! separately in `review_rerun_step_test.rs` (Task 4).

use caduceus::daemon::tick::review_rerun::{
    comment_matches_rerun_command, emit_rerun_requested_for_tests,
    emit_rerun_skipped_in_progress_for_tests, emit_rerun_skipped_untrusted_for_tests,
    is_trusted_author, RERUN_REQUESTED_EVENT, RERUN_SKIPPED_IN_PROGRESS_EVENT,
    RERUN_SKIPPED_UNTRUSTED_EVENT,
};

const DEFAULT_COMMAND: &str = "/caduceus review";

// ---------------------------------------------------------------------------
// Matcher
// ---------------------------------------------------------------------------

#[test]
fn matches_exact_command_case_insensitive() {
    assert!(comment_matches_rerun_command(
        "/caduceus Review",
        DEFAULT_COMMAND
    ));
    assert!(comment_matches_rerun_command(
        "/CADUCEUS REVIEW",
        DEFAULT_COMMAND
    ));
    assert!(comment_matches_rerun_command(
        "/caduceus review",
        DEFAULT_COMMAND
    ));
}

#[test]
fn matches_after_whitespace_normalization() {
    // Leading/trailing whitespace and collapsed internal runs must not
    // break the match.
    assert!(comment_matches_rerun_command(
        "  /caduceus   review  ",
        DEFAULT_COMMAND
    ));
    assert!(comment_matches_rerun_command(
        "\t/caduceus\t review\t",
        DEFAULT_COMMAND
    ));
}

#[test]
fn matches_on_its_own_line_among_other_text() {
    assert!(comment_matches_rerun_command(
        "looks good\n/caduceus review\nthanks",
        DEFAULT_COMMAND
    ));
    // Multi-line body where the command is the ONLY content.
    assert!(comment_matches_rerun_command(
        "/caduceus review\n",
        DEFAULT_COMMAND
    ));
}

#[test]
fn does_not_match_substring_in_longer_sentence() {
    // Regression for the substring-classification bug class (DAR §17
    // decision): the command inside a sentence must NOT match.
    assert!(!comment_matches_rerun_command(
        "please /caduceus review this PR",
        DEFAULT_COMMAND
    ));
    assert!(!comment_matches_rerun_command(
        "/caduceus review please",
        DEFAULT_COMMAND
    ));
    assert!(!comment_matches_rerun_command(
        "run /caduceus review and /caduceus review",
        DEFAULT_COMMAND
    ));
}

#[test]
fn does_not_match_empty_or_blank_body() {
    assert!(!comment_matches_rerun_command("", DEFAULT_COMMAND));
    assert!(!comment_matches_rerun_command("   \n\t  ", DEFAULT_COMMAND));
}

#[test]
fn does_not_match_inside_fenced_code_block() {
    // Review feedback (#335): an allowlisted author quoting the
    // command in a code sample must not false-trigger. Fences are
    // toggled the way GitHub's renderer toggles them.
    assert!(!comment_matches_rerun_command(
        "```\n/caduceus review\n```",
        DEFAULT_COMMAND
    ));
    assert!(!comment_matches_rerun_command(
        "```rust\n/caduceus review\n```",
        DEFAULT_COMMAND
    ));
    assert!(!comment_matches_rerun_command(
        "~~~\n/caduceus review\n~~~",
        DEFAULT_COMMAND
    ));
    // A command BEFORE a fence still matches (the fence only guards
    // the lines inside it).
    assert!(comment_matches_rerun_command(
        "/caduceus review\n```\n/caduceus review\n```",
        DEFAULT_COMMAND
    ));
    // A command AFTER a closing fence matches too.
    assert!(comment_matches_rerun_command(
        "```\nnot the trigger\n```\n/caduceus review",
        DEFAULT_COMMAND
    ));
}

#[test]
fn does_not_match_empty_command() {
    // An empty / whitespace-only configured command matches nothing.
    assert!(!comment_matches_rerun_command("/caduceus review", ""));
    assert!(!comment_matches_rerun_command("/caduceus review", "   "));
}

// ---------------------------------------------------------------------------
// Trust check
// ---------------------------------------------------------------------------

#[test]
fn trusted_author_exact_match() {
    let allowlist = vec!["bob".to_string(), "carol".to_string()];
    assert!(is_trusted_author("bob", &allowlist));
    assert!(is_trusted_author("carol", &allowlist));
    assert!(!is_trusted_author("alice", &allowlist));
    // Exact match — no case folding, no prefix matching.
    assert!(!is_trusted_author("BOB", &allowlist));
    assert!(!is_trusted_author("b", &allowlist));
}

#[test]
fn untrusted_author_ignored_when_allowlist_empty() {
    // Fail-closed: an empty allowlist trusts nobody, so no trigger
    // comment can ever be enqueued (AC2 security posture).
    assert!(!is_trusted_author("bob", &[]));
    assert!(!is_trusted_author("alice", &[]));
}

#[test]
fn event_names_match_dar_13_catalog() {
    assert_eq!(RERUN_REQUESTED_EVENT, "review_rerun_requested");
    assert_eq!(
        RERUN_SKIPPED_UNTRUSTED_EVENT,
        "review_rerun_skipped_untrusted"
    );
    assert_eq!(
        RERUN_SKIPPED_IN_PROGRESS_EVENT,
        "review_rerun_skipped_in_progress"
    );
}

// ---------------------------------------------------------------------------
// Structured emission (serial: `tracing_core` caches callsite interest
// process-wide — the #167 finding)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn rerun_requested_event_emits_structured_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let log_path = root.path().join("rerun.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = caduceus::infra::logging::build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_rerun_requested_for_tests("o/r", 7, "alice", "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{RERUN_REQUESTED_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"author\":\"alice\""), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

#[test]
#[serial_test::serial]
fn rerun_skipped_untrusted_event_emits_structured_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let log_path = root.path().join("rerun-skip.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = caduceus::infra::logging::build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_rerun_skipped_untrusted_for_tests("o/r", 7, "mallory", "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{RERUN_SKIPPED_UNTRUSTED_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"author\":\"mallory\""), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}

#[test]
#[serial_test::serial]
fn rerun_skipped_in_progress_event_emits_structured_line() {
    let root = tempfile::tempdir().expect("tempdir");
    let log_path = root.path().join("rerun-in-progress.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = caduceus::infra::logging::build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_rerun_skipped_in_progress_for_tests("o/r", 7, "alice", "abc");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{RERUN_SKIPPED_IN_PROGRESS_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(body.contains("\"repo\":\"o/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(body.contains("\"author\":\"alice\""), "got: {body}");
    assert!(body.contains("\"head_sha\":\"abc\""), "got: {body}");
}
