//! Tests for `enqueue_summaries` error propagation.
//!
//! Verifies that a StateStore returning an error from `enqueue`
//! causes `enqueue_summaries` to propagate that error rather than
//! silently swallowing it.

use std::path::PathBuf;

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-awaiting-review-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

#[tokio::test]
async fn enqueue_summaries_propagates_state_error() {
    let state_dir = tempdir("enqueue-error");
    let store = caduceus::state::queue::StateStore::open(&state_dir).expect("store opens");

    // Write a corrupt state.json so that enqueue fails on load.
    std::fs::write(state_dir.join("state.json"), b"not valid json").expect("write corrupt state");

    let summary = caduceus::poll::IssueSummary {
        key: caduceus::issue::IssueKey::parse("owner/repo#1").unwrap(),
        title: "Test".to_string(),
        labels: vec!["bug".to_string()],
        ticket_type: caduceus::queue::TicketType::Code,
        updated_at: chrono::Utc::now(),
    };

    let result =
        caduceus::daemon::tick::awaiting_review::enqueue_summaries(&store, &[summary], false);
    assert!(result.is_err(), "expected Err from corrupt state, got Ok");
}

#[test]
fn enqueue_summaries_empty_returns_ok_none() {
    let state_dir = tempdir("enqueue-empty");
    let store = caduceus::state::queue::StateStore::open(&state_dir).expect("store opens");

    let result = caduceus::daemon::tick::awaiting_review::enqueue_summaries(&store, &[], false);
    assert!(result.is_ok(), "expected Ok for empty summaries");
    assert!(result.unwrap().is_none(), "expected None for empty input");
}

// The awaiting-review tick's outcome-mapping helpers. Moved out of the
// inline `#[cfg(test)]` module per AGENTS.md; the public `*_for_tests`
// seams in `src/daemon/tick/awaiting_review.rs` mirror the private
// functions exactly.

use caduceus::config::Config;
use caduceus::daemon::tick::awaiting_review::{
    exit_code_for_tests, extract_http_status_for_tests, handle_infra_or_retry_for_tests,
    map_phase_to_outcome_for_tests, outcome_for_class_for_tests,
};
use caduceus::issue::IssueKey;
use caduceus::orchestration::{classify_error, ActiveRunGuard, FailureClass};
use caduceus::queue::Phase;
use caduceus::state::meta::TickOutcome;
use caduceus::{queue::TicketType, CaduceusError};
use std::sync::Arc;

#[test]
fn exit_code_for_outcome_table() {
    assert_eq!(exit_code_for_tests(&TickOutcome::Processed), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Idle304), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::IdleEmpty), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedConcurrent), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedCadence), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::RateLimited), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Cancelled), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Failed), 1);
}

#[test]
fn outcome_for_class_maps_each_failure_class() {
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::RateLimit { reset_at: 0 }),
        TickOutcome::RateLimited
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Cancellation),
        TickOutcome::Cancelled
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Worker),
        TickOutcome::Failed
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Infrastructure),
        TickOutcome::Failed
    ));
}

#[test]
fn map_phase_to_outcome_agrees_with_phase_taxonomy() {
    assert!(matches!(
        map_phase_to_outcome_for_tests(Phase::Queued),
        TickOutcome::Processed
    ));
    assert!(matches!(
        map_phase_to_outcome_for_tests(Phase::Failed),
        TickOutcome::Failed
    ));
    assert!(matches!(
        map_phase_to_outcome_for_tests(Phase::Done),
        TickOutcome::Processed
    ));
    assert!(matches!(
        map_phase_to_outcome_for_tests(Phase::Skipped),
        TickOutcome::Processed
    ));
}

#[test]
fn extract_http_status_only_matches_github_api_variant() {
    let err = CaduceusError::GitHubApi {
        status: 422,
        message: "x".to_string(),
    };
    assert_eq!(extract_http_status_for_tests(&err), Some(422));
    let err = CaduceusError::Worker {
        context: "result",
        stderr: "x".to_string(),
    };
    assert_eq!(extract_http_status_for_tests(&err), None);
}

// ---------------------------------------------------------------------------
// Terminal / Infrastructure / Retry dispatch (issue #167)
// ---------------------------------------------------------------------------

fn cfg(tmp: &std::path::Path) -> Config {
    Config::test_defaults(tmp)
}

fn seed_guard(
    state_dir: &std::path::Path,
) -> (Arc<caduceus::queue::StateStore>, IssueKey, ActiveRunGuard) {
    let store = Arc::new(caduceus::queue::StateStore::open(state_dir).expect("open store"));
    let key = IssueKey::parse("owner/repo#1").unwrap();
    store
        .enqueue(&key, TicketType::Code, false)
        .expect("enqueue");
    let eligible = store
        .acquire_next("RUN-1", std::process::id(), chrono::Utc::now())
        .expect("acquire")
        .expect("eligible");
    let guard = ActiveRunGuard::new(
        eligible.claim,
        store.clone(),
        PathBuf::from("/dev/null"),
        key.clone(),
    );
    (store, key, guard)
}

#[tokio::test]
async fn terminal_dispatches_to_needs_attention() {
    let state_dir = tempdir("terminal-dispatch");
    let cfg = cfg(&state_dir);
    let (store, key, mut guard) = seed_guard(&state_dir);

    let err = CaduceusError::Worktree {
        context: "discover-dirty-main",
        stderr: "main checkout is dirty at /tmp/repo; run `caduceus queue reset ...`".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);

    let outcome = handle_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::NeedsAttention);
    assert!(entry
        .last_error
        .as_deref()
        .unwrap()
        .contains("main checkout is dirty"));
    assert_eq!(entry.blocked_source.as_deref(), Some("worktree/dirty_main"));
    assert!(entry
        .blocked_recovery_hint
        .as_deref()
        .unwrap()
        .contains("caduceus queue reset"));
    assert!(entry.last_run_id.is_none());
    assert!(entry.next_attempt_at.is_none());
}

#[tokio::test]
async fn infrastructure_still_requeues_with_backoff() {
    let state_dir = tempdir("infra-requeue");
    let cfg = cfg(&state_dir);
    let (store, key, mut guard) = seed_guard(&state_dir);

    let err = CaduceusError::GitHubApi {
        status: 500,
        message: "GitHub transport error".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Infrastructure);

    let outcome = handle_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::Queued);
    assert!(entry.next_attempt_at.is_some());
    assert!(entry.blocked_source.is_none());
    assert!(entry.blocked_recovery_hint.is_none());
}

#[tokio::test]
async fn terminal_path_collision_dispatches_to_needs_attention() {
    let state_dir = tempdir("terminal-path-collision");
    let cfg = cfg(&state_dir);
    let (store, key, mut guard) = seed_guard(&state_dir);

    let err = CaduceusError::Worktree {
        context: "create-path-collision",
        stderr: "path collision: /tmp/repo/.worktrees/foreign already exists under the worker's home (foreign run id). Run `caduceus worktree-gc --dry-run` to inspect, then `caduceus worktree-gc`. If that does not help: `rm -rf /tmp/repo/.worktrees/foreign && git worktree prune`.".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);

    let outcome = handle_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::NeedsAttention);
    assert!(entry
        .last_error
        .as_deref()
        .unwrap()
        .contains("path collision"));
    assert_eq!(
        entry.blocked_source.as_deref(),
        Some("worktree/path_collision")
    );
    assert!(entry
        .blocked_recovery_hint
        .as_deref()
        .unwrap()
        .contains("caduceus worktree-gc"));
    assert!(entry.last_run_id.is_none());
    assert!(entry.next_attempt_at.is_none());
}

#[tokio::test]
async fn needs_attention_maps_to_failed() {
    let state_dir = tempdir("needs-attention-maps");
    let cfg = cfg(&state_dir);
    let (store, key, mut guard) = seed_guard(&state_dir);

    let err = CaduceusError::Queue {
        context: "claim-terminal-mismatch",
        stderr: "claim does not match; run `caduceus queue reprocess ...`".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);

    let outcome = handle_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);
    assert_eq!(
        map_phase_to_outcome_for_tests(Phase::NeedsAttention),
        TickOutcome::Failed
    );

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::NeedsAttention);
    assert_eq!(
        entry.blocked_source.as_deref(),
        Some("queue/claim_mismatch")
    );
}
