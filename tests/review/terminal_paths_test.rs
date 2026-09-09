//! AC5 — terminal paths survive restart without retry-budget burn
//! (issue #314, DAR §8.1, §14, §15).
//!
//! The four non-retry end states of the review router
//! (`src/daemon/tick/per_review.rs::handle_review_infra_or_retry`):
//!
//! | Condition | Route | Budget |
//! |---|---|---|
//! | Mutation violation (`ReviewSourceMutation`) | `NeedsAttention` (worktree KEPT) | NOT burned |
//! | Head SHA gone (`HeadShaUnavailable`) | `Skipped` (quiet skip) | NOT burned |
//! | PR gone (`ReviewGone`) | `Skipped` (quiet skip) | NOT burned |
//! | Oversized diff | direct `finish_skip` (prompt-builder seam) | NOT burned |
//!
//! Each test drives the REAL router seam
//! (`handle_review_infra_or_retry_for_tests`) with a freshly-claimed
//! entry and asserts: correct terminal phase, `attempts` unchanged,
//! the right structured event, and — via a store re-open — that the
//! terminal state survives restart (so a restart never re-burns the
//! budget).

use std::sync::Arc;

use caduceus::config::Config;
use caduceus::daemon::orchestration::{classify_error, ReviewRunGuard};
use caduceus::daemon::tick::per_review::handle_review_infra_or_retry_for_tests;
use caduceus::infra::error::CaduceusError;
use caduceus::infra::logging::build_test_subscriber;
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::state::meta::TickOutcome;
use caduceus::state::review::{
    review_claim_digest, review_queue_key, ClaimedReview, ReviewPhase, ReviewStore,
};
use caduceus::worktree::GitRunner;

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::tempdir;

const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR: u64 = 42;

fn repo() -> RepositoryId {
    RepositoryId {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    }
}

fn target(sha: &str) -> ReviewTarget {
    ReviewTarget {
        repository: repo(),
        pull_request: PR,
        head_sha: sha.to_string(),
        base_sha: "b".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "m".repeat(40),
    }
}

/// Seed a fresh store with one claimed `InProgress` entry (attempts 0)
/// and return the store + the claim.
fn seed_claimed(dir: &std::path::Path) -> (ReviewStore, ClaimedReview) {
    let store = ReviewStore::open(dir).expect("open store");
    store
        .enqueue_review(&target(&"a".repeat(40)))
        .expect("seed");
    let claimed = store
        .acquire_next_review("run-terminal", std::process::id(), chrono::Utc::now())
        .expect("acquire succeeds")
        .expect("entry claimable");
    assert_eq!(claimed.entry.attempts, 0);
    (store, claimed)
}

fn guard_for(cfg: &Config, store: Arc<ReviewStore>, claimed: &ClaimedReview) -> ReviewRunGuard {
    ReviewRunGuard::new(
        claimed.claim.clone(),
        store,
        cfg.state_dir.join("processor.log"),
        claimed.entry.target.clone(),
        GitRunner::new(cfg),
    )
}

/// Assert the terminal phase + no-burn + event, given the router
/// inputs. Re-opens the store afterwards to prove the terminal state
/// survives restart.
async fn assert_terminal_route(
    label: &str,
    err: CaduceusError,
    expected_phase: ReviewPhase,
    expected_event: &str,
) {
    let dir = tempdir(label);
    let cfg = Config::test_defaults(&dir);
    let (store, claimed) = seed_claimed(&dir);
    let store = Arc::new(store);
    let class = classify_error(&err);

    let capture = dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut guard = guard_for(&cfg, Arc::clone(&store), &claimed);
        handle_review_infra_or_retry_for_tests(cfg.clone(), &mut guard, &err, class)
            .await
            .expect("router succeeds")
    };
    drop(appender_guard);

    // Expected outcome class: terminal → Failed; quiet skip → Processed.
    let expected_outcome = if expected_phase == ReviewPhase::NeedsAttention {
        TickOutcome::Failed
    } else {
        TickOutcome::Processed
    };
    assert_eq!(outcome, expected_outcome, "tick outcome for {label}");

    let queue = store.review_queue_snapshot().expect("load queue");
    let entry = queue.entries.values().next().expect("entry exists").clone();
    assert_eq!(entry.phase, expected_phase, "terminal phase for {label}");
    assert_eq!(entry.attempts, 0, "no retry-budget burn for {label}");
    assert_eq!(
        entry.last_run_id, None,
        "claim released on the terminal route for {label}"
    );

    // The claim file is gone (the terminal transition unlinked it).
    let digest = review_claim_digest(&review_queue_key(&target(&"a".repeat(40))));
    assert!(
        !dir.join("review-claims")
            .join(format!("{digest}.claim"))
            .exists(),
        "no claim file after {label}"
    );

    // Structured event emitted.
    let body = std::fs::read_to_string(&capture).expect("read capture file");
    assert!(
        body.contains(expected_event),
        "expected event {expected_event} for {label}, got: {body}"
    );

    // Restart: terminal state is durable — the entry is NOT re-queued
    // and the budget is not re-burned by a restart.
    drop(store);
    let reopened = ReviewStore::open(&dir).expect("re-open store");
    let entry = reopened
        .review_queue_snapshot()
        .expect("load queue")
        .entries
        .values()
        .next()
        .expect("entry survives restart")
        .clone();
    assert_eq!(
        entry.phase, expected_phase,
        "terminal state persisted for {label}"
    );
    assert_eq!(
        entry.attempts, 0,
        "restart did not re-burn the budget for {label}"
    );
}

// ---------------------------------------------------------------------------
// Terminal route: mutation violation → NeedsAttention (worktree KEPT)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn mutation_violation_is_terminal_needs_attention_no_burn() {
    assert_terminal_route(
        "terminal-mutation",
        CaduceusError::ReviewSourceMutation {
            worktree_path: "/state/review-worktrees/run-terminal".into(),
            detail: "tracked files modified: src/main.rs".to_string(),
        },
        ReviewPhase::NeedsAttention,
        "review_mutation_violation",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Quiet-skip routes: head SHA gone / PR gone → Skipped
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn head_sha_unavailable_is_skip_no_burn() {
    assert_terminal_route(
        "terminal-headsha",
        CaduceusError::HeadShaUnavailable {
            sha: "a".repeat(40),
        },
        ReviewPhase::Skipped,
        "review_skipped_head_sha_unavailable",
    )
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn pr_gone_is_skip_no_burn() {
    assert_terminal_route(
        "terminal-prgone",
        CaduceusError::ReviewGone {
            reason: "pr_not_found".to_string(),
        },
        ReviewPhase::Skipped,
        "review_skipped_pr_gone",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Oversized diff: direct finish_skip (the prompt-builder seam; the
// route never enters handle_review_infra_or_retry — per_review.rs:442)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn oversized_diff_is_skip_no_burn() {
    let dir = tempdir("terminal-oversized");
    let cfg = Config::test_defaults(&dir);
    let (store, claimed) = seed_claimed(&dir);
    let store = Arc::new(store);

    // Mirror run_review_claim step 6: emit the DAR §13 event, then
    // finish_skip directly (no error, no retry path).
    let capture = dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        caduceus::worker::review_prompt::emit_oversized_pr_skip(
            &repo().full_name(),
            PR,
            &"a".repeat(40),
            2_000_000,
            1_000_000,
        );
        let mut guard = guard_for(&cfg, Arc::clone(&store), &claimed);
        guard
            .finish_skip("oversized review diff (deterministically unreviewable)")
            .await
            .expect("skip succeeds");
    }
    drop(appender_guard);

    let entry = store
        .review_queue_snapshot()
        .expect("load queue")
        .entries
        .values()
        .next()
        .expect("entry exists")
        .clone();
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0, "oversized skip never burns the budget");
    let body = std::fs::read_to_string(&capture).expect("read capture file");
    assert!(
        body.contains("review_skipped_oversized_pr"),
        "oversized event missing: {body}"
    );
}
