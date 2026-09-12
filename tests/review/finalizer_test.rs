//! Review publication finalizer tests (issue #310, DAR §9.1–9.4, §15).
//!
//! Coverage:
//!
//! - AC1: FSM transitions persisted and resumed across restart; no
//!   model re-run on GitHub failure (retryable failure →
//!   `FailedRetryable` + persisted backoff, history row untouched).
//! - AC2: stale-generation publication suppressed (history only, no
//!   comment); monotonicity via the store's generation CAS.
//! - AC3: merged → publishes; closed-unmerged → quiet skip + event;
//!   PR-404 → quiet skip.
//! - AC4: persisted backoff survives a store re-open (restart);
//!   `publication_attempt_count` is separate from worker attempts.
//! - AC5: crash-after-publish-before-mark produces no duplicate
//!   comment (marker adoption / byte-identical idempotency).

use caduceus::config::{Config, PublicationMode};
use caduceus::github::{poll_pr_merge_status, Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::review::finalize::{
    EVENT_PUBLISHED, EVENT_PUBLISH_FAILED_RETRYABLE, EVENT_PUBLISH_STARTED,
};
use caduceus::review::sticky_comment::{
    marker_for_generation, render_sticky_comment, RenderInput, REVIEW_MARKER,
};
use caduceus::review::{
    backoff_delay, claim_for_publication, finalize_review, DueFinalization, ExecutionStatus,
    FinalizeOutcome, PublicationState, RepositoryId, Review, ReviewResult, ReviewState,
    ReviewTarget, Verdict, REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{ReviewHistoryRow, ReviewStore};
use chrono::{DateTime, TimeZone, Utc};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, Request, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR: u64 = 42;
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repository() -> RepositoryId {
    RepositoryId {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    }
}

fn mock_client(gh: &MockGitHub) -> (Client, Config) {
    let state_dir = tempdir("finalize");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

/// Matches requests that do NOT carry the named header. Disambiguates
/// the unconditional first GET (200 + ETag) from the conditional
/// re-GET (304) on the same path.
struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

fn sample_review() -> Review {
    Review {
        verdict: Verdict::Pass,
        summary: "looks good".to_string(),
        findings: vec![],
    }
}

fn result_json(review: Option<Review>) -> String {
    serde_json::to_string(&ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: match review {
            Some(_) => ExecutionStatus::Success,
            None => ExecutionStatus::Failure,
        },
        review,
    })
    .expect("result serializes")
}

fn history_row(run_id: &str, generation: u64, review: Option<Review>) -> ReviewHistoryRow {
    ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: repository(),
        pull_request: PR,
        head_sha: SHA.to_string(),
        review_generation: generation,
        completed_at: Utc.with_ymd_and_hms(2026, 9, 7, 12, 0, 0).unwrap(),
        result_json: result_json(review),
    }
}

/// Seed a fresh store: `ReviewState` at `generation` in `publication`,
/// plus one durable history row for the run. Returns the store path so
/// a test can re-open it (restart semantics).
fn seeded_store(
    label: &str,
    generation: u64,
    publication: PublicationState,
) -> (ReviewStore, std::path::PathBuf) {
    let dir = tempdir(label);
    let store = ReviewStore::open(&dir).expect("review store opens");
    let mut state = ReviewState::new(repository(), PR, generation);
    state.publication_state = publication;
    store.save_review_state(&state).expect("seed state");
    store
        .append_history(history_row("run-1", generation, Some(sample_review())))
        .expect("seed history");
    (store, dir)
}

fn due(generation: u64) -> DueFinalization {
    DueFinalization {
        repository: repository(),
        pull_request: PR,
        run_generation: generation,
        head_sha: SHA.to_string(),
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 7, 13, 0, 0).unwrap()
}

/// The exact rendered body the finalizer publishes for
/// [`sample_review`] (marker adoption tests replay it).
fn rendered_body() -> String {
    render_sticky_comment(&RenderInput {
        review: &sample_review(),
        reviewed_head_sha: SHA,
        current_head_sha: None,
        review_generation: 1,
        publication_mode: PublicationMode::Update,
    })
}

/// Mount the GitHub endpoints a happy-path publish consumes: the PR
/// lifecycle GET (still open), an empty marker-search page, and the
/// create endpoint returning `comment_id`.
async fn mount_publish_open(gh: &MockGitHub, comment_id: u64) {
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
        serde_json::json!({ "id": comment_id, "body": "" }),
    )
    .await;
}

fn load_state(store: &ReviewStore) -> ReviewState {
    store
        .review_state(&repository(), PR)
        .expect("state read")
        .expect("state row exists")
}

// ---------------------------------------------------------------------------
// AC1 + AC3: happy path — Pending → Publishing → Published
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_publishes_and_persists_terminal_state() {
    let gh = MockGitHub::start().await;
    mount_publish_open(&gh, 777).await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-happy", 1, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::Published);

    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, Some(777));
    assert_eq!(state.publication_attempt_count, 1);
    assert_eq!(state.next_publish_at, None);
    assert_eq!(state.last_publish_error, None);
    assert_eq!(state.last_reviewed_head_sha.as_deref(), Some(SHA));
    assert_eq!(state.last_verdict, Some(Verdict::Pass));

    // Exactly one comment was created — no duplicate.
    assert_eq!(gh.counts().post, 1, "one create");
}

// ---------------------------------------------------------------------------
// AC3 row D: merged-and-current publishes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merged_pr_publishes() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({
            "state": "closed",
            "merged": true,
            "merge_commit_sha": "ff00ff"
        }),
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
        serde_json::json!({ "id": 901, "body": "" }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-merged", 1, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::Published, "merged publishes");
    assert_eq!(load_state(&store).sticky_comment_id, Some(901));
}

// ---------------------------------------------------------------------------
// AC3 row C: closed-unmerged → quiet skip + event, terminal sentinel
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn closed_unmerged_is_quiet_skip_with_event() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "closed", "merged": false }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, dir) = seeded_store("fin-closed", 1, PublicationState::Pending);

    // Capture the skip event around the finalize (serial discipline:
    // callsite interest is cached process-wide). The subscriber is
    // installed for the current thread only; the #[tokio::test]
    // runtime is already driving this future on it.
    let capture = dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        finalize_review(&client, &cfg, &store, &due(1), now())
            .await
            .expect("finalize succeeds")
    };
    drop(guard);

    assert_eq!(outcome, FinalizeOutcome::SkippedClosedUnmerged);
    // Quiet skip + structured event (DAR §9.3 row C).
    let body = std::fs::read_to_string(&capture).expect("read capture file");
    assert!(
        body.contains("review_skipped_pr_closed_unmerged"),
        "skip event missing: {body}"
    );

    // Terminal: finalized without publication, no comment created.
    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, None);
    assert_eq!(state.last_publish_error.as_deref(), Some("closed_unmerged"));
    assert_eq!(gh.counts().mutations(), 0, "no comment created");
}

// ---------------------------------------------------------------------------
// AC3 row B: PR-404 → quiet skip, no comment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pr_not_found_is_quiet_skip() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        404,
        serde_json::json!({ "message": "Not Found" }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-404", 1, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::PrGone);

    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, None);
    assert_eq!(state.last_publish_error.as_deref(), Some("pr_not_found"));
    assert_eq!(gh.counts().mutations(), 0, "no comment created");
}

// ---------------------------------------------------------------------------
// AC1 + AC4: retryable GitHub failure → FailedRetryable + persisted
// backoff; the model is never re-run (history row unchanged)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn github_failure_lands_in_failed_retryable_with_backoff() {
    let gh = MockGitHub::start().await;
    // Lifecycle probe succeeds; the comment create 503s.
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
        503,
        serde_json::json!({ "message": "unavailable" }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, dir) = seeded_store("fin-retry", 1, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize reports the retryable outcome");
    let next_attempt_at = match outcome {
        FinalizeOutcome::FailedRetryable { next_attempt_at } => next_attempt_at,
        other => panic!("expected FailedRetryable, got {other:?}"),
    };
    assert_eq!(
        next_attempt_at,
        now() + backoff_delay(1),
        "backoff for the first attempt"
    );

    // Retry state persisted: FailedRetryable + backoff + error, and
    // the attempt counter is the PUBLICATION counter (AC4).
    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::FailedRetryable);
    assert_eq!(state.next_publish_at, Some(next_attempt_at));
    assert!(state.last_publish_error.is_some(), "error persisted");
    assert_eq!(state.publication_attempt_count, 1);

    // AC1: the durable result is untouched — no model re-run.
    let rows = store
        .history_for_head_sha(&repository(), PR, SHA)
        .expect("history read");
    assert_eq!(rows.len(), 1, "exactly one history row");
    assert!(
        rows[0].result_json.contains("looks good"),
        "durable result unchanged"
    );

    // AC4: restart — re-open the store from disk; the backoff state
    // survives.
    let reopened = ReviewStore::open(&dir).expect("re-open");
    let state = load_state(&reopened);
    assert_eq!(state.publication_state, PublicationState::FailedRetryable);
    assert_eq!(state.next_publish_at, Some(next_attempt_at));
    assert_eq!(state.publication_attempt_count, 1);

    // Before the backoff elapses the step skips without a GitHub call.
    let gh2 = MockGitHub::start().await;
    let (client2, cfg2) = mock_client(&gh2);
    let outcome = finalize_review(&client2, &cfg2, &reopened, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::SkipRetryNotDue);
    assert_eq!(gh2.counts().total(), 0, "no GitHub call while backoff runs");

    // After the backoff elapses the publication is retried (still
    // from the same durable result — never a model re-run).
    let gh3 = MockGitHub::start().await;
    mount_publish_open(&gh3, 778).await;
    let (client3, cfg3) = mock_client(&gh3);
    let later = now() + backoff_delay(1) + chrono::Duration::seconds(1);
    let outcome = finalize_review(&client3, &cfg3, &reopened, &due(1), later)
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::Published);
    let state = load_state(&reopened);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, Some(778));
    assert_eq!(state.publication_attempt_count, 2, "retried once");
}

// ---------------------------------------------------------------------------
// #385: a 304 on the lifecycle poll must publish, not retry forever
// ---------------------------------------------------------------------------

/// Mount the PR lifecycle endpoint as a two-arm conditional GET:
/// 200 + ETag for the unconditional request, 304 for the re-GET.
async fn mount_pulls_etag_then_304(gh: &MockGitHub) {
    let pulls_path = format!("/repos/{OWNER}/{REPO}/pulls/{PR}");
    gh.mount_with(|_| {
        Mock::given(method("GET"))
            .and(path(pulls_path.as_str()))
            .and(NoHeader("if-none-match"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"abc\"")
                    .set_body_string(r#"{"merged": false, "state": "open"}"#),
            )
            .expect(1)
    })
    .await;
    gh.mount_with(|_| {
        Mock::given(method("GET"))
            .and(path(pulls_path.as_str()))
            .and(header("if-none-match", "\"abc\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
    })
    .await;
}

#[tokio::test]
async fn lifecycle_poll_304_replay_publishes_instead_of_retrying() {
    let gh = MockGitHub::start().await;
    mount_pulls_etag_then_304(&gh).await;
    // Publish-path endpoints: empty marker-search page + create.
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([])],
    )
    .await;
    gh.mount_status(
        "POST",
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        201,
        serde_json::json!({ "id": 779, "body": "" }),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-304", 1, PublicationState::Pending);

    // Pre-warm the ETag cache through the same client the finalizer
    // will use, so the lifecycle poll below is the conditional GET.
    let warm = poll_pr_merge_status(&client, OWNER, REPO, PR).await;
    assert!(warm.is_ok(), "cache pre-warm poll succeeds");

    // The finalizer's first GitHub call now 304s. It must parse the
    // cached body and publish — NOT land in failed_retryable (the
    // #385 symptom: publication_state stuck, comment never posts).
    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize step completes");
    assert_eq!(outcome, FinalizeOutcome::Published);
    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, Some(779));
}

// ---------------------------------------------------------------------------
// AC2: stale-generation suppression — history only, no comment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_generation_is_suppressed_without_publishing() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    // Current state generation 2 (a newer admission bumped it); the
    // completing run finished under generation 1.
    let (store, _dir) = seeded_store("fin-stale", 2, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::SuppressedStaleGeneration);

    // No GitHub call at all; no state transition (non-transition).
    assert_eq!(gh.counts().total(), 0, "no HTTP for a suppressed run");
    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Pending);
    assert_eq!(state.publication_attempt_count, 0);

    // History only (AC2): the durable result remains.
    let rows = store
        .history_for_head_sha(&repository(), PR, SHA)
        .expect("history read");
    assert_eq!(rows.len(), 1, "result stays in history");
}

// ---------------------------------------------------------------------------
// AC2: concurrent admission — the stale run's claim loses the
// generation CAS and publication never starts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_admission_suppresses_stale_publication() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-cas", 1, PublicationState::Pending);

    // A newer admission bumps the generation after run-1 completed.
    let target = ReviewTarget {
        repository: repository(),
        pull_request: PR,
        head_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        base_sha: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
        base_ref: "main".to_string(),
        merge_base: "dddddddddddddddddddddddddddddddddddddddd".to_string(),
    };
    store
        .enqueue_review(&target)
        .expect("second admission wins the race");
    let current = load_state(&store);
    assert_eq!(current.review_generation, 2, "generation bumped");

    // The stale run's finalization is suppressed before any publish.
    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::SuppressedStaleGeneration);
    assert_eq!(gh.counts().total(), 0, "publish never called");

    // Claim-level check: a generation-regressing save is rejected by
    // the store CAS — `claim_for_publication` maps it to Ok(false).
    let mut stale = current.clone();
    stale.review_generation = 1;
    let claimed = claim_for_publication(&store, &stale).expect("claim checks CAS");
    assert!(!claimed, "regressing claim is rejected");
}

// ---------------------------------------------------------------------------
// AC1 + AC5: crash recovery — `Publishing` claim resumes; the orphaned
// comment is adopted via the marker, never duplicated
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crashed_publishing_claim_resumes_and_publishes() {
    let gh = MockGitHub::start().await;
    // The crashed claim already created the comment whose id was
    // never persisted: the marker search finds it and publish adopts
    // it (byte-identical → Unchanged, no duplicate create).
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "open", "merged": false }),
    )
    .await;
    let body = rendered_body();
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([serde_json::json!({
            "id": 99,
            // The marker scan matches the UNTAGGED legacy prefix in the
            // list body (the search is generation-aware from #394 Task
            // 3; pre-#394 comments parse as gen 0). The GET below
            // returns the freshly-rendered body — the byte-identical
            // compare target.
            "body": format!("an older review body\n{REVIEW_MARKER}"),
        })])],
    )
    .await;
    gh.mount_status(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/issues/comments/99"),
        200,
        serde_json::json!({ "id": 99, "body": body }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-crash", 1, PublicationState::Publishing);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(
        outcome,
        FinalizeOutcome::PublishedUnchanged,
        "adopted the orphaned marker comment (byte-identical)"
    );

    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, Some(99));
    assert_eq!(gh.counts().post, 0, "no duplicate create after crash");
}

#[tokio::test]
async fn crash_after_publish_before_mark_never_duplicates() {
    // Crash simulation: the claim is `Publishing` (id not persisted)
    // and the marker comment ALREADY exists with an older body —
    // publish must PATCH the adopted id, not create a second comment.
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "open", "merged": false }),
    )
    .await;
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([serde_json::json!({
            "id": 55,
            "body": format!("an older review body\n{REVIEW_MARKER}"),
        })])],
    )
    .await;
    gh.mount_status(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/issues/comments/55"),
        200,
        serde_json::json!({ "id": 55, "body": "an older review body" }),
    )
    .await;
    gh.mount_status(
        "PATCH",
        &format!("/repos/{OWNER}/{REPO}/issues/comments/55"),
        200,
        serde_json::json!({ "id": 55, "body": rendered_body() }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-dup", 1, PublicationState::Publishing);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(
        outcome,
        FinalizeOutcome::Published,
        "adopted id patched in place"
    );

    assert_eq!(gh.counts().post, 0, "never a second comment");
    assert_eq!(gh.counts().patch, 1, "adopted id patched");
    assert_eq!(load_state(&store).sticky_comment_id, Some(55));
}

// ---------------------------------------------------------------------------
// Pre-state guards: already-final and missing state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn already_final_skips_without_github_call() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-final", 1, PublicationState::Published);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::AlreadyFinal);
    assert_eq!(gh.counts().total(), 0, "zero GitHub calls");
}

#[tokio::test]
async fn missing_state_row_is_quiet_no_state() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let dir = tempdir("fin-nostate");
    let store = ReviewStore::open(&dir).expect("store opens");

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::NoState);
    assert_eq!(gh.counts().total(), 0, "zero GitHub calls");
}

// ---------------------------------------------------------------------------
// AC1: non-success durable result finalizes quietly (never re-runs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_execution_result_finalizes_without_publication() {
    let gh = MockGitHub::start().await;
    let (client, cfg) = mock_client(&gh);
    let dir = tempdir("fin-failed");
    let store = ReviewStore::open(&dir).expect("store opens");
    let mut state = ReviewState::new(repository(), PR, 1);
    state.publication_state = PublicationState::Pending;
    store.save_review_state(&state).expect("seed state");
    store
        .append_history(history_row("run-1", 1, None))
        .expect("seed failed-run history");

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::NoState, "quiet finalize");
    assert_eq!(gh.counts().total(), 0, "nothing published, no HTTP");
    let state = load_state(&store);
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(
        state.last_publish_error.as_deref(),
        Some("no_publishable_result")
    );
}

// ---------------------------------------------------------------------------
// Backoff schedule (AC4 helper contract)
// ---------------------------------------------------------------------------

#[test]
fn backoff_schedule_is_exponential_with_one_hour_cap() {
    assert_eq!(backoff_delay(1), chrono::Duration::seconds(60));
    assert_eq!(backoff_delay(2), chrono::Duration::seconds(120));
    assert_eq!(backoff_delay(3), chrono::Duration::seconds(240));
    assert_eq!(backoff_delay(5), chrono::Duration::seconds(960));
    assert_eq!(backoff_delay(6), chrono::Duration::seconds(1920));
    // Cap: 2^7 * 30s = 3840s > 3600s → 1h; everything above stays 1h.
    assert_eq!(backoff_delay(7), chrono::Duration::seconds(3600));
    assert_eq!(backoff_delay(12), chrono::Duration::seconds(3600));
    assert_eq!(backoff_delay(50), chrono::Duration::seconds(3600));
}

// ---------------------------------------------------------------------------
// DAR §13 publish-event capture (issue #318: per-transition emission
// tests). Serial discipline: tracing callsite interest is cached
// process-wide; the subscriber is installed for the current thread
// only, and the #[tokio::test] runtime drives the future on it.
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn publish_started_and_published_emit_on_happy_path() {
    let gh = MockGitHub::start().await;
    mount_publish_open(&gh, 777).await;
    let (client, cfg) = mock_client(&gh);
    let (store, dir) = seeded_store("fin-publish-event", 1, PublicationState::Pending);

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
        finalize_review(&client, &cfg, &store, &due(1), now())
            .await
            .expect("finalize succeeds")
    };
    drop(appender_guard);

    assert_eq!(outcome, FinalizeOutcome::Published);

    let body = std::fs::read_to_string(&capture).expect("read capture file");
    // The publish transition emits started → published.
    for expected in [EVENT_PUBLISH_STARTED, EVENT_PUBLISHED] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    assert!(
        !body.contains(EVENT_PUBLISH_FAILED_RETRYABLE),
        "retryable-failure leaked into the published path: {body}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn publish_failed_retryable_emits_on_github_failure() {
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
        503,
        serde_json::json!({ "message": "unavailable" }),
    )
    .await;
    let (client, cfg) = mock_client(&gh);
    let (store, dir) = seeded_store("fin-fail-event", 1, PublicationState::Pending);

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
        finalize_review(&client, &cfg, &store, &due(1), now())
            .await
            .expect("finalize succeeds")
    };
    drop(appender_guard);

    assert!(matches!(outcome, FinalizeOutcome::FailedRetryable { .. }));

    let body = std::fs::read_to_string(&capture).expect("read capture file");
    // The retryable-failure transition emits started →
    // publish_failed_retryable; the comment was never published.
    for expected in [EVENT_PUBLISH_STARTED, EVENT_PUBLISH_FAILED_RETRYABLE] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    assert!(
        !body.contains(EVENT_PUBLISHED),
        "published leaked into the failed path: {body}"
    );
}

// ---------------------------------------------------------------------------
// Update banner wiring (issue #393) — finalize passes the generation
// ---------------------------------------------------------------------------

fn posted_comment_body(gh: &MockGitHub) -> String {
    let posts: Vec<_> = gh
        .received_requests()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .collect();
    assert_eq!(posts.len(), 1, "exactly one comment POST");
    let payload: serde_json::Value =
        serde_json::from_slice(&posts[0].body).expect("POST body is JSON");
    payload["body"].as_str().expect("body field").to_string()
}

#[tokio::test]
async fn generation_two_finalize_publishes_banner_body() {
    let gh = MockGitHub::start().await;
    mount_publish_open(&gh, 601).await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-banner", 2, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(2), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::Published);

    // `SHA` is forty `a`s (finalizer_test.rs:43), so the 12-char
    // prefix is "aaaaaaaaaaaa". Build the expected fragment from the
    // const so the assertion tracks it.
    let body = posted_comment_body(&gh);
    let expected = format!(
        "> [!IMPORTANT] Updated for commit `{}` (review generation 2)",
        &SHA[..12]
    );
    assert!(
        body.contains(&expected),
        "banner on the wire for gen 2: {body}"
    );
}

#[tokio::test]
async fn generation_one_finalize_publishes_without_banner() {
    let gh = MockGitHub::start().await;
    mount_publish_open(&gh, 602).await;
    let (client, cfg) = mock_client(&gh);
    let (store, _dir) = seeded_store("fin-banner-one", 1, PublicationState::Pending);

    let outcome = finalize_review(&client, &cfg, &store, &due(1), now())
        .await
        .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::Published);

    let body = posted_comment_body(&gh);
    assert!(!body.contains("[!IMPORTANT]"), "no banner on gen 1: {body}");
    assert!(
        body.starts_with(&marker_for_generation(1)),
        "marker still first"
    );
}
