//! Review-side router tests (issue #339, DAR §8.1): the fourth-route
//! quiet-skip fan-out of `handle_review_infra_or_retry` on a
//! `ReviewRunGuard`, mirroring `tests/daemon/awaiting_review_test.rs`
//! for the issue guard.
//!
//! Coverage:
//!
//! - `HeadShaUnavailable` → quiet skip (Skipped, attempts unchanged,
//!   `review_skipped_head_sha_unavailable` event);
//! - `ReviewGone` (`pr_not_found` / `closed_unmerged`) → quiet skip
//!   (`review_skipped_pr_gone`);
//! - `ReviewSourceMutation` → Terminal NeedsAttention (attempts
//!   unchanged, no retry);
//! - Worker error → retry-budget (Queued + attempts+1);
//! - Infrastructure → backoff requeue (Queued, attempts unchanged,
//!   `next_attempt_at` set);
//! - the ISSUE-side fourth route (AC2): `HeadShaUnavailable` arriving
//!   at `handle_infra_or_retry` skips the issue entry instead of
//!   routing to NeedsAttention.

use std::path::PathBuf;
use std::sync::Arc;

use caduceus::config::Config;
use caduceus::daemon::orchestration::ReviewRunGuard;
use caduceus::daemon::tick::per_review::handle_review_infra_or_retry_for_tests;
use caduceus::error::CaduceusError;
use caduceus::meta::TickOutcome;
use caduceus::orchestration::{classify_error, FailureClass};
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::state::review::{ReviewPhase, ReviewQueueEntry, ReviewStore};
use caduceus::worktree::GitRunner;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

fn cfg(tmp: &std::path::Path) -> Config {
    Config::test_defaults(tmp)
}

fn repo_id() -> RepositoryId {
    RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    }
}

fn target() -> ReviewTarget {
    ReviewTarget {
        repository: repo_id(),
        pull_request: 7,
        head_sha: "b".repeat(40),
        base_sha: "a".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "a".repeat(40),
    }
}

fn seed_review_guard(state_dir: &std::path::Path) -> (Arc<ReviewStore>, ReviewRunGuard) {
    let store = Arc::new(ReviewStore::open(state_dir).expect("review store opens"));
    store
        .enqueue_review(&target())
        .expect("enqueue review target");
    let claimed = store
        .acquire_next_review("RUN-1", std::process::id(), chrono::Utc::now())
        .expect("acquire")
        .expect("eligible review entry");
    let runner = GitRunner::new(&cfg(state_dir));
    let guard = ReviewRunGuard::new(
        claimed.claim,
        Arc::clone(&store),
        PathBuf::from("/dev/null"),
        claimed.entry.target.clone(),
        runner,
    );
    (store, guard)
}

fn queue_entry(store: &ReviewStore) -> ReviewQueueEntry {
    store
        .review_queue_snapshot()
        .expect("review queue snapshot")
        .entries
        .values()
        .find(|e| e.target.pull_request == 7)
        .expect("review entry for PR 7")
        .clone()
}

// ---------------------------------------------------------------------------
// Fourth route: HeadShaUnavailable / ReviewGone → quiet skip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn head_sha_unavailable_is_quiet_skip() {
    let state_dir = tempdir("review-skip-head-sha");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::HeadShaUnavailable {
        sha: "b".repeat(40),
    };
    let class = classify_error(&err);
    assert_eq!(
        class,
        FailureClass::Infrastructure,
        "self-resolving infra, never fatal"
    );

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0, "skip must not burn the retry budget");
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("head SHA unavailable"),
        "skip reason must record the structured error"
    );
}

#[tokio::test]
async fn review_gone_pr_not_found_is_quiet_skip() {
    let state_dir = tempdir("review-skip-gone");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::ReviewGone {
        reason: "pr_not_found".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Infrastructure);

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0);
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("pr_not_found"),
        "skip reason must carry the gone reason"
    );
}

#[tokio::test]
async fn review_gone_closed_unmerged_is_quiet_skip() {
    let state_dir = tempdir("review-skip-closed");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::ReviewGone {
        reason: "closed_unmerged".to_string(),
    };
    let class = classify_error(&err);
    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0);
}

// ---------------------------------------------------------------------------
// Terminal route: mutation violation → NeedsAttention, worktree kept
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutation_violation_is_terminal_needs_attention() {
    let state_dir = tempdir("review-mutation-terminal");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::ReviewSourceMutation {
        worktree_path: PathBuf::from("/tmp/repo"),
        detail: "worker-prompt.md control file modified by the worker".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
    assert_eq!(entry.attempts, 0, "terminal routes never burn the budget");
    assert_eq!(
        entry.blocked_source.as_deref(),
        Some("review/mutation_violation")
    );
}

#[tokio::test]
async fn unknown_terminal_routes_to_needs_attention() {
    let state_dir = tempdir("review-terminal-unknown");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::Worktree {
        context: "discover-dirty-main",
        stderr: "main checkout is dirty at /tmp/repo".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
    assert_eq!(entry.attempts, 0);
}

// ---------------------------------------------------------------------------
// Retry-budget route
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_error_retries_with_budget() {
    let state_dir = tempdir("review-worker-retry");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::Worker {
        context: "result",
        stderr: "result file missing".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Worker);

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(
        entry.attempts, 1,
        "worker-attributable failure burns the budget"
    );
    assert!(entry.next_attempt_at.is_some(), "backoff must be scheduled");
    assert!(entry
        .last_error
        .as_deref()
        .unwrap_or_default()
        .contains("result file missing"));
}

// ---------------------------------------------------------------------------
// Infrastructure route: backoff requeue, attempts untouched
// ---------------------------------------------------------------------------

#[tokio::test]
async fn infrastructure_requeues_with_backoff() {
    let state_dir = tempdir("review-infra-requeue");
    let cfg = cfg(&state_dir);
    let (store, mut guard) = seed_review_guard(&state_dir);

    let err = CaduceusError::GitHubApi {
        status: 500,
        message: "GitHub transport error".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Infrastructure);

    let outcome = handle_review_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = queue_entry(&store);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(
        entry.attempts, 0,
        "infrastructure never increments attempts"
    );
    assert!(entry.next_attempt_at.is_some());
    assert!(entry
        .last_error
        .as_deref()
        .unwrap_or_default()
        .contains("GitHub transport error"));
}

// ---------------------------------------------------------------------------
// AC2: the ISSUE-side fourth route also skips on HeadShaUnavailable
// ---------------------------------------------------------------------------

use caduceus::daemon::tick::awaiting_review::handle_infra_or_retry_for_tests;
use caduceus::issue::IssueKey;
use caduceus::queue::Phase;
use caduceus::state::queue::StateStore;

fn seed_issue_guard(
    state_dir: &std::path::Path,
) -> (
    Arc<StateStore>,
    IssueKey,
    caduceus::daemon::orchestration::ActiveRunGuard,
) {
    let store = Arc::new(StateStore::open(state_dir).expect("issue store opens"));
    let key = IssueKey::parse("owner/repo#1").unwrap();
    store
        .enqueue(&key, caduceus::queue::TicketType::Code, false)
        .expect("enqueue issue");
    let eligible = store
        .acquire_next("RUN-1", std::process::id(), chrono::Utc::now())
        .expect("acquire")
        .expect("eligible issue entry");
    let guard = caduceus::daemon::orchestration::ActiveRunGuard::new(
        eligible.claim,
        store.clone(),
        PathBuf::from("/dev/null"),
        key.clone(),
    );
    (store, key, guard)
}

#[tokio::test]
async fn issue_router_skips_on_head_sha_unavailable() {
    let state_dir = tempdir("issue-skip-head-sha");
    let cfg = cfg(&state_dir);
    let (store, key, mut guard) = seed_issue_guard(&state_dir);

    let err = CaduceusError::HeadShaUnavailable {
        sha: "b".repeat(40),
    };
    let class = classify_error(&err);

    let outcome = handle_infra_or_retry_for_tests(cfg, &mut guard, &err, class)
        .await
        .expect("dispatch");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::Skipped);
    assert_eq!(
        entry.last_error.as_deref().unwrap_or_default(),
        err.to_string(),
        "skip reason must record the structured error verbatim"
    );
    assert!(entry.last_run_id.is_none());
}
