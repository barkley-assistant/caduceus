//! E2E PR lifecycle integration test (issue #322, DAR §15
//! "End-to-end" row).
//!
//! ONE full-lifecycle test, NO STUBS: discovery → queue → review
//! worker → FAIL published to the sticky comment → head moves → new
//! revision → PASS replaces the SAME sticky comment — including
//! out-of-order completion and the two negative paths. Runs on BOTH
//! state backends (JSON + SQLite).
//!
//! Harness (see `lifecycle_harness.rs` and `docs/testing/e2e-lifecycle.md`):
//! wiremock fake GitHub + real local bare git remote + the REAL daemon
//! dispatch/finalizer/store code + the REAL `review_harness.py` worker
//! running as a real subprocess under the REAL `caduceus
//! __worker-supervisor` supervision protocol (the executor re-execs
//! the real `caduceus` binary via `ReleaseBinary::locate()`; the plan's
//! `self_exe = current_exe()` assumption cannot hold inside a test
//! binary — see the harness module docs for the deviation).
//!
//! Acceptance criteria (issue #322):
//! 1. Lifecycle green end-to-end with no stubs, both backends.
//! 2. Exactly one sticky comment across both revisions; two history
//!    rows.
//! 3. Same-SHA re-poll produces zero new admissions; publish-failure
//!    resume does not re-run the worker.

use std::fs;

use caduceus::infra::logging::build_test_subscriber;
use caduceus::meta::TickOutcome;
use caduceus::review::{
    finalize_review, DueFinalization, FinalizeOutcome, PublicationState, Verdict,
};
use caduceus::state::review::{ReviewPhase, ReviewQueueEntry};

#[path = "../fixtures/mod.rs"]
mod fixtures;
#[path = "lifecycle_harness.rs"]
mod harness;

use harness::{Backend, LifecycleHarness, STICKY_COMMENT_ID};

// ---------------------------------------------------------------------------
// Shared assertion helpers
// ---------------------------------------------------------------------------

fn state(h: &LifecycleHarness) -> caduceus::review::ReviewState {
    h.store
        .review_state(&h.repository(), harness::PR)
        .expect("state read")
        .expect("state row exists")
}

fn queue_entry(h: &LifecycleHarness, head_sha: &str) -> ReviewQueueEntry {
    h.store
        .review_queue_snapshot()
        .expect("queue snapshot")
        .entries
        .values()
        .find(|e| e.target.head_sha == head_sha)
        .expect("queue entry for head sha")
        .clone()
}

fn queue_len(h: &LifecycleHarness) -> usize {
    h.store
        .review_queue_snapshot()
        .expect("queue snapshot")
        .entries
        .len()
}

fn history_len(h: &LifecycleHarness) -> usize {
    h.store
        .history_for_pull_request(&h.repository(), harness::PR)
        .expect("history read")
        .len()
}

/// Assert the queue entry for `head_sha` is in `phase` with the given
/// attempts count.
fn assert_entry_phase(h: &LifecycleHarness, head_sha: &str, phase: ReviewPhase, attempts: u32) {
    let entry = queue_entry(h, head_sha);
    assert_eq!(entry.phase, phase, "phase for head {head_sha}");
    assert_eq!(entry.attempts, attempts, "attempts for head {head_sha}");
}

// ---------------------------------------------------------------------------
// The full lifecycle (AC1 + AC2), parametrized by backend
// ---------------------------------------------------------------------------

/// Drive the complete FAIL → PASS lifecycle on one backend and return
/// the finished harness so negative-path tests can continue from it.
async fn run_lifecycle(backend: Backend) -> LifecycleHarness {
    let h = LifecycleHarness::start("lifecycle", backend).await;
    let repo = h.repository();
    h.mount_pr_fetch_and_discussion().await;
    h.mount_comment_create(201, STICKY_COMMENT_ID).await;
    h.mount_comment_get(STICKY_COMMENT_ID).await;
    h.mount_comment_patch(STICKY_COMMENT_ID).await;

    // Phase 1 — discovery admits gen 1 (head = mid).
    h.mount_pulls(&h.mid_sha).await;
    let stats = h.drive_discovery().await;
    assert_eq!(stats.admitted, 1, "gen 1 admitted on the first poll");
    assert_eq!(stats.stale_observed, 0);
    let entry = queue_entry(&h, &h.mid_sha);
    assert_eq!(entry.review_generation, 1, "first generation");
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(
        entry.target.merge_base, h.base_sha,
        "DAR §2.2 merge base persisted at admission"
    );

    // Phase 2 — claim + real worker run → FAIL verdict.
    let outcome = h.drive_claim("run-1", "fail").await;
    assert_eq!(outcome, TickOutcome::Processed, "FAIL run processes");
    harness::assert_history(&h.store, &repo, &[("run-1", 1, &h.mid_sha, "fail")]);
    assert_eq!(state(&h).review_generation, 1);
    assert_eq!(
        state(&h).publication_state,
        PublicationState::Pending,
        "history appended; not yet published"
    );
    assert_entry_phase(&h, &h.mid_sha, ReviewPhase::Done, 0);

    // Phase 3 — finalizer publishes the FAIL sticky comment.
    let fstats = h.drive_finalize().await;
    assert_eq!(fstats.published, 1, "FAIL publication succeeds");
    let s = state(&h);
    assert_eq!(s.publication_state, PublicationState::Published);
    assert_eq!(s.sticky_comment_id, Some(STICKY_COMMENT_ID));
    assert_eq!(s.last_verdict, Some(Verdict::Fail));
    assert_eq!(
        s.last_reviewed_head_sha.as_deref(),
        Some(h.mid_sha.as_str())
    );
    assert_eq!(h.gh.counts().post, 1, "one comment created for FAIL");
    assert_eq!(h.gh.counts().patch, 0, "no update yet");

    // Phase 4 — head moves → re-discovery admits gen 2 (head = tip).
    h.mount_pulls_revision(&h.tip_sha).await;
    let stats = h.drive_discovery().await;
    assert_eq!(stats.admitted, 1, "new SHA re-admits");
    assert_eq!(stats.stale_observed, 1, "B → C stale observation");
    assert_eq!(queue_len(&h), 2, "both revisions present in the queue");
    let fresh = queue_entry(&h, &h.tip_sha);
    assert_eq!(fresh.review_generation, 2, "generation advanced");
    assert_eq!(fresh.phase, ReviewPhase::Queued);
    assert_entry_phase(&h, &h.mid_sha, ReviewPhase::Done, 0);

    // Phase 5 — re-review → PASS verdict.
    let outcome = h.drive_claim("run-2", "pass").await;
    assert_eq!(outcome, TickOutcome::Processed, "PASS run processes");
    harness::assert_history(
        &h.store,
        &repo,
        &[
            ("run-1", 1, &h.mid_sha, "fail"),
            ("run-2", 2, &h.tip_sha, "pass"),
        ],
    );
    assert_eq!(state(&h).review_generation, 2);

    // Phase 6 — finalizer updates the SAME sticky comment with PASS.
    let fstats = h.drive_finalize().await;
    assert_eq!(fstats.published, 1, "PASS publication succeeds");
    let s = state(&h);
    assert_eq!(s.publication_state, PublicationState::Published);
    assert_eq!(s.last_verdict, Some(Verdict::Pass));
    assert_eq!(
        s.last_reviewed_head_sha.as_deref(),
        Some(h.tip_sha.as_str())
    );

    // AC2 — exactly one sticky comment across both revisions; two
    // history rows with distinct run ids.
    assert_eq!(
        state(&h).sticky_comment_id,
        Some(STICKY_COMMENT_ID),
        "PASS updates the same comment — id unchanged"
    );
    assert_eq!(h.gh.counts().post, 1, "exactly one comment created");
    assert_eq!(h.gh.counts().patch, 1, "PASS replaced via one PATCH");
    assert_eq!(history_len(&h), 2, "two history rows");

    h
}

#[tokio::test]
#[serial_test::serial]
async fn lifecycle_json() {
    let h = run_lifecycle(Backend::Json).await;
    drop(h);
}

#[tokio::test]
#[serial_test::serial]
async fn lifecycle_sqlite() {
    let h = run_lifecycle(Backend::Sqlite).await;
    drop(h);
}

// ---------------------------------------------------------------------------
// Out-of-order completion (DAR §9.4): B completes before A
// ---------------------------------------------------------------------------

/// B (gen 2) completes before A (gen 1): the sticky comment shows B;
/// A persists to history only, and A's publication is suppressed with
/// `review_publication_suppressed_stale_generation`. Both backends.
#[tokio::test]
#[serial_test::serial]
async fn lifecycle_out_of_order() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let h = LifecycleHarness::start("ooo", backend).await;
        let repo = h.repository();
        h.mount_pr_fetch_and_discussion().await;
        h.mount_comment_create(201, STICKY_COMMENT_ID).await;

        // Admit A (gen 1, head = mid) and B (gen 2, head = tip).
        h.store
            .enqueue_review(&harness::review_target(&repo, &h.mid_sha, &h.base_sha))
            .expect("admit A (gen 1)");
        h.store
            .enqueue_review(&harness::review_target(&repo, &h.tip_sha, &h.base_sha))
            .expect("admit B (gen 2)");
        assert_eq!(
            state(&h).review_generation,
            2,
            "state tracks the newest generation"
        );

        // Claim both (FIFO: A first, then B), then complete B BEFORE A
        // — the durable rows are the finalizer's only input (DAR §9.1).
        let claimed_a = h
            .store
            .acquire_next_review("run-a", std::process::id(), chrono::Utc::now())
            .expect("acquire A")
            .expect("A claimable");
        assert_eq!(claimed_a.entry.review_generation, 1);
        let claimed_b = h
            .store
            .acquire_next_review("run-b", std::process::id(), chrono::Utc::now())
            .expect("acquire B")
            .expect("B claimable");
        assert_eq!(claimed_b.entry.review_generation, 2);

        // B's PASS result lands first; A's FAIL result persists to
        // history afterwards.
        h.store
            .append_history(harness::fabricated_row(
                &repo, "run-b", &h.tip_sha, 2, "pass",
            ))
            .expect("B history row");
        h.store
            .complete_review(claimed_b.claim)
            .expect("B completes");
        h.store
            .append_history(harness::fabricated_row(
                &repo, "run-a", &h.mid_sha, 1, "fail",
            ))
            .expect("A history row");
        h.store
            .complete_review(claimed_a.claim)
            .expect("A completes");

        // The history-driven poll routes ONLY B: A's generation is
        // superseded at the query (state.rs due_finalizations).
        let fstats = h.drive_finalize().await;
        assert_eq!(fstats.published, 1, "B publishes");
        assert_eq!(fstats.suppressed, 0, "A is filtered before finalize");
        let s = state(&h);
        assert_eq!(s.publication_state, PublicationState::Published);
        assert_eq!(s.sticky_comment_id, Some(STICKY_COMMENT_ID));
        assert_eq!(s.last_verdict, Some(Verdict::Pass), "sticky shows B's PASS");
        assert_eq!(
            s.last_reviewed_head_sha.as_deref(),
            Some(h.tip_sha.as_str()),
            "current pointer shows B, not A"
        );
        assert_eq!(h.gh.counts().post, 1, "exactly one comment — for B");
        assert_eq!(h.gh.counts().patch, 0);

        // Direct guard test: a stale DueFinalization (gen 1 while the
        // state is gen 2) is suppressed and emits the §9.4 event.
        let capture = h.cfg.state_dir.join("events.log");
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&capture)
            .expect("open capture file");
        let (writer, appender_guard) = tracing_appender::non_blocking(file);
        let subscriber = build_test_subscriber(writer);
        let out_a = {
            let _guard = tracing::subscriber::set_default(subscriber);
            finalize_review(
                h.client.as_ref(),
                &h.cfg,
                h.store.as_ref(),
                &DueFinalization {
                    repository: repo.clone(),
                    pull_request: harness::PR,
                    run_generation: 1,
                    head_sha: h.mid_sha.clone(),
                },
                chrono::Utc::now(),
            )
            .await
            .expect("A finalizes")
        };
        drop(appender_guard);
        assert_eq!(
            out_a,
            FinalizeOutcome::SuppressedStaleGeneration,
            "A suppressed — got {out_a:?}"
        );
        let body = fs::read_to_string(&capture).expect("read capture file");
        assert!(
            body.contains("review_publication_suppressed_stale_generation"),
            "suppression event missing: {body}"
        );

        // A never touched the presentation; both rows are in history.
        assert_eq!(h.gh.counts().post, 1, "no second comment for A");
        assert_eq!(history_len(&h), 2, "A and B both in history");
        harness::assert_history(
            &h.store,
            &repo,
            &[
                ("run-b", 2, &h.tip_sha, "pass"),
                ("run-a", 1, &h.mid_sha, "fail"),
            ],
        );
        assert!(
            h.store.due_finalizations().expect("due scan").is_empty(),
            "nothing re-due after suppression"
        );
    }
}

// ---------------------------------------------------------------------------
// Negative paths (AC3)
// ---------------------------------------------------------------------------

/// Same-SHA re-poll after the full lifecycle: zero new admissions,
/// the queue and history are untouched. Both backends.
#[tokio::test]
#[serial_test::serial]
async fn lifecycle_same_sha_repoll() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let h = run_lifecycle(backend).await;
        let queue_before = queue_len(&h);
        let history_before = history_len(&h);

        // Re-mount the SAME head (tip) and re-poll.
        h.mount_pulls_revision(&h.tip_sha).await;
        let stats = h.drive_discovery().await;
        assert_eq!(stats.admitted, 0, "AC3: no new admissions on same SHA");
        assert_eq!(stats.skipped_already_complete, 1);
        assert_eq!(queue_len(&h), queue_before, "queue unchanged");
        assert_eq!(history_len(&h), history_before, "no new history row");
        assert_eq!(h.gh.counts().post, 1, "no extra comment on the re-poll");
        assert_eq!(h.gh.counts().patch, 1);
    }
}

/// Publish-failure resume: a GitHub 500 during publication leaves the
/// row at `FailedRetryable` with persisted backoff; the resume
/// publishes WITHOUT re-running the worker. Both backends.
#[tokio::test]
#[serial_test::serial]
async fn lifecycle_publish_failure_resume() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let h = LifecycleHarness::start("publish-fail", backend).await;
        let repo = h.repository();
        h.mount_pr_fetch_and_discussion().await;

        // Seed one completed FAIL run (history row; nothing published).
        h.store
            .enqueue_review(&harness::review_target(&repo, &h.mid_sha, &h.base_sha))
            .expect("admit");
        let claimed = h
            .store
            .acquire_next_review("run-1", std::process::id(), chrono::Utc::now())
            .expect("acquire")
            .expect("claimable");
        assert_eq!(claimed.entry.review_generation, 1);
        h.store
            .append_history(harness::fabricated_row(
                &repo, "run-1", &h.mid_sha, 1, "fail",
            ))
            .expect("history row");
        h.store.complete_review(claimed.claim).expect("complete");

        // Publication fails retryably (POST → 500).
        h.mount_comment_create(500, STICKY_COMMENT_ID).await;
        let stats = h.drive_finalize().await;
        assert_eq!(stats.failed_retryable, 1, "AC3: retryable failure");
        let s = state(&h);
        assert_eq!(s.publication_state, PublicationState::FailedRetryable);
        assert_eq!(s.publication_attempt_count, 1);
        assert!(s.next_publish_at.is_some(), "backoff persisted");
        assert!(s.last_publish_error.is_some());
        assert_eq!(
            h.gh.counts().post,
            1,
            "the failed create was attempted once"
        );

        // Resume: the endpoint recovers, the backoff elapsed, and the
        // finalizer publishes the SAME durable result — the worker is
        // NEVER re-run (DAR §9.1: publication is presentation only).
        h.mount_comment_create_resume(STICKY_COMMENT_ID).await;
        let next_attempt_at = s.next_publish_at.expect("next publish at");
        let now = next_attempt_at + chrono::Duration::seconds(1);
        let due = h.store.due_finalizations().expect("due scan");
        assert_eq!(due.len(), 1, "the FailedRetryable row is due again");
        let outcome = finalize_review(h.client.as_ref(), &h.cfg, h.store.as_ref(), &due[0], now)
            .await
            .expect("resume finalizes");
        assert_eq!(outcome, FinalizeOutcome::Published);

        let s = state(&h);
        assert_eq!(s.publication_state, PublicationState::Published);
        assert_eq!(s.sticky_comment_id, Some(STICKY_COMMENT_ID));
        assert_eq!(s.publication_attempt_count, 2);
        assert_eq!(s.last_verdict, Some(Verdict::Fail));
        assert_eq!(h.gh.counts().post, 2, "failed create + successful create");

        // AC3 part 2 — the worker was NOT re-run: still exactly one
        // history row, the queue entry is Done with zero attempts.
        harness::assert_history(&h.store, &repo, &[("run-1", 1, &h.mid_sha, "fail")]);
        assert_entry_phase(&h, &h.mid_sha, ReviewPhase::Done, 0);
    }
}
