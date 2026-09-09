//! AC4 — out-of-order completion + stale-generation suppression
//! (issue #314, DAR §9.4, §15).
//!
//! B admitted after A (higher `review_generation`), B finishes first →
//! the sticky comment shows B; A's result persists in history only,
//! and `review_publication_suppressed_stale_generation` is emitted for
//! A. The suppression survives a restart (the state is durable).
//!
//! The ordering is FABRICATED by persist-then-bump (plan D4), not live
//! timing: A completes and its result is in history at generation 1;
//! B is admitted (generation bumps to 2) and completes; B finalizes
//! first; A's finalization is driven with its generation-1
//! `DueFinalization` and the §9.4 guard suppresses it.

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::review::{
    finalize_review, DueFinalization, ExecutionStatus, FinalizeOutcome, PublicationState,
    RepositoryId, Review, ReviewResult, ReviewTarget, Verdict, REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{ReviewHistoryRow, ReviewStore};

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR: u64 = 42;
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

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

fn sample_review() -> Review {
    Review {
        verdict: Verdict::Pass,
        summary: "looks good".to_string(),
        findings: vec![],
    }
}

fn history_row(run_id: &str, generation: u64, head_sha: &str) -> ReviewHistoryRow {
    ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: repo(),
        pull_request: PR,
        head_sha: head_sha.to_string(),
        review_generation: generation,
        completed_at: chrono::Utc::now(),
        result_json: serde_json::to_string(&ReviewResult {
            schema_version: REVIEW_SCHEMA_VERSION,
            status: ExecutionStatus::Success,
            review: Some(sample_review()),
        })
        .expect("result serializes"),
    }
}

/// Seed the full A-then-B completion story and return the store.
/// A runs at generation 1 and completes (history row); B is admitted
/// (generation bumps to 2), runs and completes. Both queue entries end
/// `Done`; the state row is at generation 2, `Pending` (re-armed by
/// B's admission).
fn seeded_ooo_store(dir: &std::path::Path) -> ReviewStore {
    let store = ReviewStore::open(dir).expect("open store");

    // A: admit (gen 1) → claim → history → Done.
    store.enqueue_review(&target(SHA_A)).expect("admit A");
    let claimed_a = store
        .acquire_next_review("run-a", 1, chrono::Utc::now())
        .expect("acquire A")
        .expect("A claimable");
    assert_eq!(claimed_a.entry.review_generation, 1, "A runs at gen 1");
    store
        .append_history(history_row("run-a", 1, SHA_A))
        .expect("A result persisted");
    store.complete_review(claimed_a.claim).expect("A completes");

    // B: admit (bumps to gen 2) → claim → history → Done.
    store.enqueue_review(&target(SHA_B)).expect("admit B");
    let claimed_b = store
        .acquire_next_review("run-b", 2, chrono::Utc::now())
        .expect("acquire B")
        .expect("B claimable");
    assert_eq!(claimed_b.entry.review_generation, 2, "B runs at gen 2");
    store
        .append_history(history_row("run-b", 2, SHA_B))
        .expect("B result persisted");
    store.complete_review(claimed_b.claim).expect("B completes");

    let state = store
        .review_state(&repo(), PR)
        .expect("state read")
        .expect("state row exists");
    assert_eq!(state.review_generation, 2);
    assert_eq!(state.publication_state, PublicationState::Pending);
    store
}

#[tokio::test]
#[serial_test::serial]
async fn b_before_a_leaves_sticky_b_and_suppresses_a() {
    let dir = tempdir("ooo");
    let gh = MockGitHub::start().await;
    // B's publication path: PR open, empty marker page, create → id 777.
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "open", "merged": false }),
    )
    .await;
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([])],
    )
    .await;
    gh.mount_status(
        "POST",
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        201,
        serde_json::json!({ "id": 777, "body": "" }),
    )
    .await;
    let mut cfg = Config::test_defaults(&dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let store = seeded_ooo_store(&dir);

    // Capture the suppression event around A's finalization (serial
    // discipline: the traced callsite interest is cached process-wide).
    let capture = dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    // Both finalizes run under the subscriber: B publishes first (the
    // due-scan routes only B — A's generation is already superseded),
    // then A's stale DueFinalization is suppressed by the §9.4 guard.
    let out_b;
    let out_a;
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let due = store.due_finalizations().expect("due scan");
        assert_eq!(due.len(), 1, "only B is due — A is superseded");
        assert_eq!(due[0].head_sha, SHA_B);
        out_b = finalize_review(&client, &cfg, &store, &due[0], chrono::Utc::now())
            .await
            .expect("B finalizes");
        assert_eq!(out_b, FinalizeOutcome::Published, "B publishes");
        assert_eq!(gh.counts().post, 1, "one comment — for B");

        out_a = finalize_review(
            &client,
            &cfg,
            &store,
            &DueFinalization {
                repository: repo(),
                pull_request: PR,
                run_generation: 1,
                head_sha: SHA_A.to_string(),
            },
            chrono::Utc::now(),
        )
        .await
        .expect("A finalizes");
    }
    drop(appender_guard);

    assert_eq!(
        out_a,
        FinalizeOutcome::SuppressedStaleGeneration,
        "A suppressed — got {out_a:?}"
    );
    assert_eq!(
        gh.counts().post,
        1,
        "no second comment for A — sticky still shows B"
    );

    // Event emitted for A.
    let body = std::fs::read_to_string(&capture).expect("read capture file");
    assert!(
        body.contains("review_publication_suppressed_stale_generation"),
        "suppression event missing: {body}"
    );

    // History has both; the current pointer shows B.
    let history = store
        .history_for_pull_request(&repo(), PR)
        .expect("history read");
    assert_eq!(history.len(), 2, "A and B both in history");
    let state = store
        .review_state(&repo(), PR)
        .expect("state read")
        .expect("state row exists");
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(
        state.last_reviewed_head_sha.as_deref(),
        Some(SHA_B),
        "sticky/current pointer shows B, not A"
    );
    assert_eq!(state.sticky_comment_id, Some(777));
}

#[tokio::test]
#[serial_test::serial]
async fn stale_generation_suppression_survives_restart() {
    let dir = tempdir("ooo-restart");
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "open", "merged": false }),
    )
    .await;
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([])],
    )
    .await;
    gh.mount_status(
        "POST",
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        201,
        serde_json::json!({ "id": 778, "body": "" }),
    )
    .await;
    let mut cfg = Config::test_defaults(&dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let store = seeded_ooo_store(&dir);

    // B publishes; A is suppressed.
    let due = store.due_finalizations().expect("due scan");
    assert_eq!(due.len(), 1);
    let out_b = finalize_review(&client, &cfg, &store, &due[0], chrono::Utc::now())
        .await
        .expect("B finalizes");
    assert_eq!(out_b, FinalizeOutcome::Published);
    let out_a = finalize_review(
        &client,
        &cfg,
        &store,
        &DueFinalization {
            repository: repo(),
            pull_request: PR,
            run_generation: 1,
            head_sha: SHA_A.to_string(),
        },
        chrono::Utc::now(),
    )
    .await
    .expect("A finalizes");
    assert_eq!(out_a, FinalizeOutcome::SuppressedStaleGeneration);

    // Restart: drop the handle and re-open. The suppression is durable
    // — A is NOT re-due, B is Published.
    drop(store);
    let reopened = ReviewStore::open(&dir).expect("re-open store");
    let due = reopened.due_finalizations().expect("due scan");
    assert!(
        due.is_empty(),
        "suppression persisted across restart — nothing due"
    );
    let state = reopened
        .review_state(&repo(), PR)
        .expect("state read")
        .expect("state row exists");
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.last_reviewed_head_sha.as_deref(), Some(SHA_B));
    assert_eq!(state.sticky_comment_id, Some(778));
    assert_eq!(gh.counts().post, 1, "exactly one comment across restart");
}
