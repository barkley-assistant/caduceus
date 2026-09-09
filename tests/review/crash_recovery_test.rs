//! AC2 + AC3 — restart mid-run / mid-publish recovery (issue #314,
//! DAR §14, §15).
//!
//! AC2 (container kill): a SIGKILL'd in-flight review run leaves a
//! review claim file + an `InProgress` entry. Tick step 3.1
//! (`oci_lifecycle::reconcile_installation`) + step 3.2
//! (`reap_stale_claims` + `reap_stale_review_claims`, issue #371) are
//! supposed to clear the orphan and return the entry to `Queued` so
//! the next tick re-claims it (`src/daemon/tick/mod.rs:376`, `:392`).
//! This test drives the SAME functions the tick calls — no stub of
//! the recovery logic. The issue pass runs first (a no-op for review
//! claims); the review pass clears `review-claims/`.
//!
//! NOTE (Honesty Clause provenance): `reconcile_installation` requires
//! a live OCI engine (it runs `podman ps` against a labeled container
//! set), so this test uses the plan's D3 fallback — the post-reconcile
//! state is seeded (no OCI run row remains; the container is already
//! gone) and ONLY the real reapers are driven. The AC ("no orphan
//! claims after restart") is proven by asserting the reaper clears the
//! orphan review claim. On main @ 472a88f the issue reaper only walked
//! the ISSUE claims dir and this test FAILED — that failure was the
//! Honesty-Clause evidence that drove issue #371; the fix is the
//! sibling `reap_stale_review_claims` pass, and the test PASSES with
//! it.
//!
//! AC3 (restart mid-publish): a crash after the sticky comment is
//! posted but before `finalize_published` persists `Published` leaves
//! the row at `Publishing`. Re-entry adopts the existing comment by
//! marker (byte-identical idempotency, §9.2) — no duplicate comment —
//! and reaches `Published`.

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache};
use caduceus::review::sticky_comment::{render_sticky_comment, RenderInput, REVIEW_MARKER};
use caduceus::review::{
    finalize_review, ExecutionStatus, FinalizeOutcome, PublicationState, RepositoryId, Review,
    ReviewResult, ReviewState, ReviewTarget, Verdict, REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{
    review_claim_digest, review_queue_key, ReviewClaimFileBody, ReviewHistoryRow, ReviewPhase,
    ReviewStore, REVIEW_CLAIM_FILE_VERSION,
};
use chrono::{Duration, Utc};

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR: u64 = 42;
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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

fn history_row(run_id: &str, generation: u64, head_sha: &str) -> ReviewHistoryRow {
    ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: repo(),
        pull_request: PR,
        head_sha: head_sha.to_string(),
        review_generation: generation,
        completed_at: Utc::now(),
        result_json: result_json(Some(sample_review())),
    }
}

fn mock_client(gh: &MockGitHub, dir: &std::path::Path) -> (Client, Config) {
    let mut cfg = Config::test_defaults(dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

// ---------------------------------------------------------------------------
// AC2 — restart mid-run (container kill): the orphan review claim must
// be reaped by the step 3.2 path
// ---------------------------------------------------------------------------

/// The AC2 recovery assertion. On main @ 472a88f this FAILED:
/// `reap_stale_claims` never walked `review-claims/`
/// (`src/state/queue/reaper.rs:59-64` reads `state_dir/claims/` only),
/// so the orphan claim survived and the `InProgress` entry was never
/// re-claimable. The failure was the Honesty-Clause evidence; the fix
/// is the sibling `reap_stale_review_claims` pass (issue #371), which
/// this test drives in the same order the tick does.
#[tokio::test]
async fn container_kill_then_restart_reaps_orphan_claim() {
    let dir = tempdir("crash-mid-run");
    let cfg = Config::test_defaults(&dir);
    let store = ReviewStore::open(&dir).expect("open store");
    let target = target(SHA);
    store.enqueue_review(&target).expect("seed entry");

    // The daemon claims the entry and the worker run starts (the
    // container is created under the daemon's OCI identity).
    let claimed = store
        .acquire_next_review("run-1", std::process::id(), Utc::now())
        .expect("acquire succeeds")
        .expect("one eligible entry");
    assert_eq!(claimed.entry.phase, ReviewPhase::InProgress);

    // Container kill: the daemon dies with the run in flight. The
    // claim file now records a dead process. A run that has been
    // in-flight longer than `stale_run_hours` (1h in test defaults)
    // is deterministically "stale" by the reaper's own rule; rewrite
    // the claim's started_at to model that crash, with a dead pid and
    // a mismatched process identity (the container-killed daemon).
    let key = review_queue_key(&target);
    let digest = review_claim_digest(&key);
    let claim_path = dir.join("review-claims").join(format!("{digest}.claim"));
    let stale_body = ReviewClaimFileBody {
        version: REVIEW_CLAIM_FILE_VERSION,
        target: target.clone(),
        run_id: "run-1".to_string(),
        // Definitively dead on Linux (pid_max is far smaller).
        pid: 999_999_999,
        // Never matches the live daemon's identity string.
        process_start_identity: "container-killed-daemon".to_string(),
        started_at: Utc::now() - Duration::hours(2),
        worktree_path: None,
    };
    std::fs::write(
        &claim_path,
        serde_json::to_string(&stale_body).expect("serialize claim"),
    )
    .expect("write stale claim body");

    // Restart: the tick runs step 3.1 (reconcile_installation — the
    // post-reconcile state is already seeded: no OCI run row survives)
    // and then step 3.2, the real stale-claim reapers — the issue pass
    // first (a no-op for review claims), then the review pass, in the
    // same order the tick calls them.
    let report = caduceus::state::queue::reap_stale_claims(&dir, Utc::now(), cfg.stale_run_hours)
        .await
        .expect("reap succeeds");
    let _ = caduceus::state::queue::reap_stale_review_claims(&dir, Utc::now(), cfg.stale_run_hours)
        .await
        .expect("review reap succeeds");

    // AC2 assertion: no orphan review claim remains.
    let orphan_claims = std::fs::read_dir(dir.join("review-claims"))
        .expect("read review-claims dir")
        .filter_map(Result::ok)
        .count();
    assert_eq!(
        orphan_claims, 0,
        "orphan review claim must be reaped on restart (reap report: {report:?})"
    );

    // And the entry is re-claimable: back to Queued.
    let reopened = ReviewStore::open(&dir).expect("re-open store");
    let queue = reopened.review_queue_snapshot().expect("load queue");
    let entry = queue
        .entries
        .values()
        .next()
        .expect("entry survives the restart");
    assert_eq!(
        entry.phase,
        ReviewPhase::Queued,
        "InProgress review entry is re-queued after the orphan reap"
    );
}

// ---------------------------------------------------------------------------
// AC3 — restart mid-publish: no duplicate comment; resume reaches
// Published
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_mid_publish_emits_no_duplicate_comment_and_reaches_published() {
    let gh = MockGitHub::start().await;
    // Crash-after-publish-before-mark: the comment was posted by the
    // crashed run but the id was never persisted. The marker search
    // finds it and publish adopts it (byte-identical → Unchanged → no
    // duplicate create, no PATCH).
    let rendered = render_sticky_comment(&RenderInput {
        review: &sample_review(),
        reviewed_head_sha: SHA,
        current_head_sha: None,
    });
    assert!(
        rendered.contains(REVIEW_MARKER),
        "rendered sticky carries the automation marker"
    );
    gh.mount(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/pulls/{PR}"),
        serde_json::json!({ "state": "open", "merged": false }),
    )
    .await;
    gh.mount_paged(
        &format!("/repos/{OWNER}/{REPO}/issues/{PR}/comments"),
        vec![serde_json::json!([serde_json::json!({
            "id": 99,
            "body": rendered,
        })])],
    )
    .await;
    gh.mount_status(
        "GET",
        &format!("/repos/{OWNER}/{REPO}/issues/comments/99"),
        200,
        serde_json::json!({ "id": 99, "body": rendered }),
    )
    .await;

    let dir = tempdir("crash-mid-publish");
    let (client, cfg) = mock_client(&gh, &dir);
    let store = ReviewStore::open(&dir).expect("open store");

    // Seed the crash state: `Publishing` row (comment posted, id NOT
    // persisted) + the durable history row the poll derives the due
    // finalization from.
    let mut state = ReviewState::new(repo(), PR, 1);
    state.publication_state = PublicationState::Publishing;
    store.save_review_state(&state).expect("seed state");
    store
        .append_history(history_row("run-1", 1, SHA))
        .expect("seed history");

    // The crashed run is due: the poll re-enters the non-terminal row.
    let due = store.due_finalizations().expect("due scan");
    assert_eq!(due.len(), 1, "the crashed Publishing row is due");
    assert_eq!(due[0].head_sha, SHA);

    // First re-entry: adopts the existing marker comment — Unchanged.
    let outcome = finalize_review(&client, &cfg, &store, &due[0], Utc::now())
        .await
        .expect("finalize succeeds");
    assert_eq!(
        outcome,
        FinalizeOutcome::PublishedUnchanged,
        "resumes without re-posting — got {outcome:?}"
    );
    assert_eq!(
        gh.counts().post,
        0,
        "no duplicate comment — the existing marker comment was adopted"
    );
    let state = store
        .review_state(&repo(), PR)
        .expect("state read")
        .expect("state row exists");
    assert_eq!(state.publication_state, PublicationState::Published);
    assert_eq!(state.sticky_comment_id, Some(99));

    // Restart: re-open the store; the row is final — nothing is due
    // and a re-finalize is a no-op.
    drop(store);
    let reopened = ReviewStore::open(&dir).expect("re-open store");
    assert!(
        reopened.due_finalizations().expect("due scan").is_empty(),
        "Published row is not due after restart"
    );
    let outcome = finalize_review(
        &client,
        &cfg,
        &reopened,
        &caduceus::review::DueFinalization {
            repository: repo(),
            pull_request: PR,
            run_generation: 1,
            head_sha: SHA.to_string(),
        },
        Utc::now(),
    )
    .await
    .expect("finalize succeeds");
    assert_eq!(outcome, FinalizeOutcome::AlreadyFinal);
    assert_eq!(gh.counts().post, 0, "still no duplicate after restart");
}
