//! Phase-1 fork-gate predicate and skip-event tests (issue #316).
//!
//! Coverage:
//!
//! - Predicate truth table over wire-shaped JSON fixtures: same-repo,
//!   fork, and every `Option` degenerate row (null/absent head repo —
//!   the deleted-head-branch fixture — plus base-side degenerates).
//! - Fail-closed property: only a proven `SameRepo` row passes the
//!   gate (AC3 by construction until #312 wires the gate into
//!   discovery).
//! - Skip-event emission: the DAR §13
//!   `review_skipped_fork_unsupported` event carries repo + PR +
//!   head-repo identity, and the nullable-head-repo row emits the same
//!   event with a `"null"` identity sentinel.
//! - Purity guard: `classify_fork` emits no events under a capture
//!   subscriber.
//!
//! Fixtures are built as `serde_json::json!` values and deserialised
//! through [`PullRequestDetail`], pinning the wire→`Option` decode
//! (explicit `null` and absent keys both map to `None`) together with
//! the classification, following the `pr_wire_test.rs` fixture shape.
//!
//! The event-capture tests are `#[serial_test::serial]`: `tracing_core`
//! caches callsite interest process-wide, so a sibling test exercising
//! the same callsite without a subscriber installed would silently
//! drop the captured event (the #167 finding).

use caduceus::github::fork_gate::{
    classify_fork, emit_fork_gate_skip, ForkStatus, FORK_SKIP_EVENT,
};
use caduceus::github::PullRequestDetail;
use caduceus::logging::build_test_subscriber;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

/// Decode a wire row into the typed model. Fixtures must round-trip:
/// a panic here means the fixture is not a valid `/pulls` row.
fn decode(row: serde_json::Value) -> PullRequestDetail {
    serde_json::from_value(row).expect("wire row decodes into PullRequestDetail")
}

/// A `/pulls` row with the given base/head repo identities. `None`
/// renders `"repo": null` (the deleted-head-branch / absent-repo
/// wire shape); `Some(full_name)` renders a populated repo object.
fn pr_row(base_repo: Option<&str>, head_repo: Option<&str>) -> serde_json::Value {
    let mut row = serde_json::json!({
        "number": 7,
        "title": "Fork gate row",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa"},
        "head": {"ref": "feature-x", "sha": "bbbb"}
    });
    row["base"]["repo"] = base_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row["head"]["repo"] = head_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row
}

/// A row with no `head` key at all.
fn row_without_head() -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": "No head",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa", "repo": {"full_name": "octocat/hello-world"}}
    })
}

/// A row with a `head` object but no `repo` key inside it.
fn row_without_head_repo_key() -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": "Head without repo key",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa", "repo": {"full_name": "octocat/hello-world"}},
        "head": {"ref": "feature-x", "sha": "bbbb"}
    })
}

/// A row with a `head.repo` object but no `full_name` key inside it.
fn row_without_head_full_name_key() -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": "Head repo without full_name",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa", "repo": {"full_name": "octocat/hello-world"}},
        "head": {"ref": "feature-x", "sha": "bbbb", "repo": {}}
    })
}

/// A row with no `base` key at all.
fn row_without_base() -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": "No base",
        "state": "open",
        "head": {"ref": "feature-x", "sha": "bbbb", "repo": {"full_name": "octocat/hello-world"}}
    })
}

/// A row with a `base.repo` object but no `full_name` key inside it.
fn row_without_base_full_name_key() -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": "Base repo without full_name",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa", "repo": {}},
        "head": {"ref": "feature-x", "sha": "bbbb", "repo": {"full_name": "octocat/hello-world"}}
    })
}

// ---------------------------------------------------------------------------
// Predicate truth table (pure tests — no subscriber, no async)
// ---------------------------------------------------------------------------

#[test]
fn same_repo_row_passes_gate() {
    let pr = decode(pr_row(
        Some("octocat/hello-world"),
        Some("octocat/hello-world"),
    ));
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::SameRepo);
    assert!(status.passes(), "proven same-repo must pass the gate");
}

#[test]
fn fork_row_classifies_fork_with_identity() {
    let pr = decode(pr_row(Some("octocat/hello-world"), Some("octocat/other")));
    let status = classify_fork(&pr);
    assert_eq!(
        status,
        ForkStatus::Fork {
            head_repo: "octocat/other".to_string(),
        }
    );
    assert!(!status.passes(), "proven fork must fail the gate");
    assert_eq!(status.head_repo_identity(), Some("octocat/other"));
}

// The nullable-head-repo fixture: `"head": {..., "repo": null}` (a
// deleted head branch) decodes to `repo: None` and classifies as
// `HeadRepoMissing` — its own verdict, never a panic, never a fork.
#[test]
fn null_head_repo_classifies_head_repo_missing() {
    let pr = decode(pr_row(Some("octocat/hello-world"), None));
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::HeadRepoMissing);
    assert!(!status.passes());
    assert_eq!(status.head_repo_identity(), None);
}

#[test]
fn absent_head_repo_classifies_head_repo_missing() {
    let pr = decode(row_without_head_repo_key());
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::HeadRepoMissing);
    assert!(!status.passes());
}

#[test]
fn null_head_full_name_classifies_head_repo_missing() {
    let row = serde_json::json!({
        "number": 7,
        "title": "Null head full_name",
        "state": "open",
        "base": {"ref": "main", "sha": "aaaa", "repo": {"full_name": "octocat/hello-world"}},
        "head": {"ref": "feature-x", "sha": "bbbb", "repo": {"full_name": null}}
    });
    let pr = decode(row);
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::HeadRepoMissing);
    assert!(!status.passes());
}

#[test]
fn absent_head_full_name_key_classifies_head_repo_missing() {
    let pr = decode(row_without_head_full_name_key());
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::HeadRepoMissing);
    assert!(!status.passes());
}

#[test]
fn absent_head_classifies_head_repo_missing() {
    let pr = decode(row_without_head());
    let status = classify_fork(&pr);
    assert_eq!(status, ForkStatus::HeadRepoMissing);
    assert!(!status.passes());
}

#[test]
fn null_base_repo_classifies_base_repo_missing() {
    let pr = decode(pr_row(None, Some("octocat/hello-world")));
    let status = classify_fork(&pr);
    assert_eq!(
        status,
        ForkStatus::BaseRepoMissing {
            head_repo: "octocat/hello-world".to_string(),
        }
    );
    assert!(!status.passes());
    assert_eq!(status.head_repo_identity(), Some("octocat/hello-world"));
}

#[test]
fn absent_base_classifies_base_repo_missing() {
    let pr = decode(row_without_base());
    let status = classify_fork(&pr);
    assert_eq!(
        status,
        ForkStatus::BaseRepoMissing {
            head_repo: "octocat/hello-world".to_string(),
        }
    );
    assert!(!status.passes());
    assert_eq!(status.head_repo_identity(), Some("octocat/hello-world"));
}

#[test]
fn null_base_full_name_classifies_base_repo_missing() {
    let pr = decode(row_without_base_full_name_key());
    let status = classify_fork(&pr);
    assert_eq!(
        status,
        ForkStatus::BaseRepoMissing {
            head_repo: "octocat/hello-world".to_string(),
        }
    );
    assert!(!status.passes());
}

#[test]
fn full_name_comparison_is_exact_not_case_folded() {
    let pr = decode(pr_row(Some("Octo/Repo"), Some("octo/repo")));
    let status = classify_fork(&pr);
    assert_eq!(
        status,
        ForkStatus::Fork {
            head_repo: "octo/repo".to_string(),
        }
    );
    assert!(!status.passes(), "strict inequality fails closed");
}

// The AC3-by-construction test: every non-SameRepo verdict (the full
// degenerate family plus a proven fork) fails the gate, so only a
// proven same-repo row can ever be admitted.
#[test]
fn only_proven_same_repo_passes_gate() {
    let rows = vec![
        pr_row(Some("octocat/hello-world"), Some("octocat/other")),
        pr_row(Some("octocat/hello-world"), None),
        row_without_head_repo_key(),
        row_without_head_full_name_key(),
        row_without_head(),
        pr_row(None, Some("octocat/hello-world")),
        row_without_base(),
        row_without_base_full_name_key(),
        pr_row(Some("Octo/Repo"), Some("octo/repo")),
    ];
    for row in rows {
        let pr = decode(row);
        let status = classify_fork(&pr);
        assert!(
            !status.passes(),
            "non-SameRepo verdict {:?} must fail the gate",
            status
        );
    }
}

// ---------------------------------------------------------------------------
// Skip-event emission (capture tests — serial: callsite interest caching)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn fork_skip_emits_structured_event_with_full_identity() {
    let root = tempdir("fork-gate-event");
    let log_path = root.join("fork.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_fork_gate_skip("octocat/hello-world", 42, Some("octocat/other"));
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{FORK_SKIP_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"octocat/hello-world\""),
        "got: {body}"
    );
    assert!(body.contains("\"pr\":42"), "got: {body}");
    assert!(
        body.contains("\"head_repo\":\"octocat/other\""),
        "got: {body}"
    );
}

#[test]
#[serial_test::serial]
fn head_repo_missing_skip_emits_null_identity() {
    let root = tempdir("fork-gate-null");
    let log_path = root.join("fork.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        emit_fork_gate_skip("octocat/hello-world", 43, None);
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{FORK_SKIP_EVENT}\"")),
        "event name missing: {body}"
    );
    assert!(
        body.contains("\"repo\":\"octocat/hello-world\""),
        "got: {body}"
    );
    assert!(body.contains("\"pr\":43"), "got: {body}");
    // The `None` identity renders as the literal `"null"` sentinel —
    // unambiguous because a real GitHub `full_name` always contains `/`.
    assert!(body.contains("\"head_repo\":\"null\""), "got: {body}");
}

#[test]
#[serial_test::serial]
fn classify_fork_emits_no_events() {
    let root = tempdir("fork-gate-pure");
    let log_path = root.join("classify.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    // One row per verdict family: same-repo, fork, head-missing,
    // base-missing. `classify_fork` must stay pure — no event, no
    // logging — so the capture file stays empty of the fork event.
    let rows = vec![
        pr_row(Some("octocat/hello-world"), Some("octocat/hello-world")),
        pr_row(Some("octocat/hello-world"), Some("octocat/other")),
        pr_row(Some("octocat/hello-world"), None),
        row_without_base(),
    ];
    tracing::subscriber::with_default(subscriber, || {
        for row in rows {
            let pr = decode(row);
            let _ = classify_fork(&pr);
        }
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        !body.contains(FORK_SKIP_EVENT),
        "classify_fork must not emit the fork event: {body}"
    );
}
