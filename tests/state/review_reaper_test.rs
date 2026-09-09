//! Unit tests for the review-claim stale reaper (issue #371).
//!
//! `caduceus::state::queue::reap_stale_review_claims` is the sibling
//! of the issue-claim reaper: identical liveness rule
//! (`IDENTITY.is_alive` + `process_start_identity` match + age
//! cutoff), identical symlink rejection / `corrupt/` subdir skip /
//! non-`.claim` skip, and malformed/future-stamp quarantine into
//! `review-claims/corrupt/`. The queue revert returns an
//! `InProgress` entry to `Queued` with `next_attempt_at = now` and
//! NEVER touches `attempts` (no retry-budget burn).
//!
//! Covered here: symlink skip, malformed-body quarantine,
//! liveness-rule miss (a live pid is not reaped), and attempt-burn
//! absence. The end-to-end restart assertion lives in
//! `tests/review/crash_recovery_test.rs`.

use caduceus::config::Config;
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::state::review::{
    review_claim_digest, review_queue_key, ReviewClaimFileBody, ReviewPhase, ReviewStore,
    REVIEW_CLAIM_FILE_VERSION,
};
use chrono::{Duration, Utc};

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::tempdir;

const OWNER: &str = "octocat";
const REPO: &str = "hello-world";
const PR: u64 = 42;
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn target() -> ReviewTarget {
    ReviewTarget {
        repository: RepositoryId {
            owner: OWNER.to_string(),
            repo: REPO.to_string(),
        },
        pull_request: PR,
        head_sha: SHA.to_string(),
        base_sha: "b".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "m".repeat(40),
    }
}

/// Seed an `InProgress` review entry through the real claim path
/// (`enqueue_review` + `acquire_next_review`) and return the claim
/// file path. The claim the store writes carries the LIVE test
/// process's pid/identity and a recent `started_at`.
fn seed_in_progress(dir: &std::path::Path) -> std::path::PathBuf {
    let store = ReviewStore::open(dir).expect("open store");
    store.enqueue_review(&target()).expect("seed entry");
    let claimed = store
        .acquire_next_review("run-1", std::process::id(), Utc::now())
        .expect("acquire succeeds")
        .expect("one eligible entry");
    assert_eq!(claimed.entry.phase, ReviewPhase::InProgress);
    let key = review_queue_key(&target());
    let digest = review_claim_digest(&key);
    dir.join("review-claims").join(format!("{digest}.claim"))
}

/// Overwrite a claim file with a deliberately stale body (dead pid,
/// mismatched identity, `started_at` older than `stale_run_hours`).
fn write_stale_claim(
    dir: &std::path::Path,
    claim_path: &std::path::Path,
    run_id: &str,
    started_at: chrono::DateTime<Utc>,
) {
    let body = ReviewClaimFileBody {
        version: REVIEW_CLAIM_FILE_VERSION,
        target: target(),
        run_id: run_id.to_string(),
        // Definitively dead on Linux (pid_max is far smaller).
        pid: 999_999_999,
        // Never matches the live daemon's identity string.
        process_start_identity: "container-killed-daemon".to_string(),
        started_at,
        worktree_path: None,
    };
    std::fs::write(
        claim_path,
        serde_json::to_string(&body).expect("serialize claim"),
    )
    .expect("write stale claim");
    let _ = dir;
}

fn entry_phase(dir: &std::path::Path) -> ReviewPhase {
    let reopened = ReviewStore::open(dir).expect("re-open store");
    let queue = reopened.review_queue_snapshot().expect("load queue");
    queue.entries.values().next().expect("entry survives").phase
}

// ---------------------------------------------------------------------------
// Symlink skip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn symlink_in_review_claims_is_reported_and_never_followed() {
    let dir = tempdir("review-reaper-symlink");
    let cfg = Config::test_defaults(&dir);
    let claim_path = seed_in_progress(&dir);

    // A symlink inside `review-claims/` pointing at a stale-shaped
    // body OUTSIDE the dir. Following it would reap an unrelated
    // file; the reaper must report it and move on.
    let outside = dir.join("outside-stale.claim");
    write_stale_claim(&dir, &outside, "run-1", Utc::now() - Duration::hours(2));
    let link = dir.join("review-claims").join("evil.claim");
    std::os::unix::fs::symlink(&outside, &link).expect("create symlink");

    let report =
        caduceus::state::queue::reap_stale_review_claims(&dir, Utc::now(), cfg.stale_run_hours)
            .await
            .expect("reap succeeds");

    assert_eq!(report.count, 0, "no claim file acted on: {report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("refusing to act on symlink")),
        "symlink must be reported in errors: {report:?}"
    );
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "symlink untouched"
    );
    assert!(claim_path.exists(), "recent live claim not reaped");
    assert_eq!(entry_phase(&dir), ReviewPhase::InProgress);
}

// ---------------------------------------------------------------------------
// Malformed-body quarantine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_review_claim_is_quarantined_not_reaped() {
    let dir = tempdir("review-reaper-malformed");
    let cfg = Config::test_defaults(&dir);
    let claim_path = seed_in_progress(&dir);

    std::fs::write(&claim_path, b"this is not json {").expect("write garbage claim");

    let report =
        caduceus::state::queue::reap_stale_review_claims(&dir, Utc::now(), cfg.stale_run_hours)
            .await
            .expect("reap succeeds");

    assert_eq!(report.quarantined, 1, "{report:?}");
    assert_eq!(report.count, 1, "{report:?}");
    assert!(!claim_path.exists(), "original claim moved to corrupt/");
    let corrupt = dir.join("review-claims").join("corrupt");
    let artefacts: Vec<_> = std::fs::read_dir(&corrupt)
        .expect("corrupt dir")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(artefacts.len(), 1, "one quarantined artefact");
    assert!(
        artefacts[0]
            .file_name()
            .to_string_lossy()
            .ends_with(".corrupt"),
        "quarantine suffix"
    );

    // Quarantine does NOT revert the phase — the claim is corrupt,
    // not confirmed stale.
    assert_eq!(entry_phase(&dir), ReviewPhase::InProgress);
}

// ---------------------------------------------------------------------------
// Liveness-rule miss: a live pid is not reaped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_pid_claim_is_not_reaped() {
    let dir = tempdir("review-reaper-live");
    let cfg = Config::test_defaults(&dir);
    let claim_path = seed_in_progress(&dir);

    // Age the claim (old `started_at`) while KEEPING the live test
    // process's pid and the identity the store recorded. The age
    // check passes, but the liveness rule must skip it: the recorded
    // pid is alive and the start identity matches.
    let body: ReviewClaimFileBody =
        serde_json::from_str(&std::fs::read_to_string(&claim_path).expect("read claim"))
            .expect("parse claim body");
    let aged = ReviewClaimFileBody {
        started_at: Utc::now() - Duration::hours(2),
        ..body
    };
    std::fs::write(
        &claim_path,
        serde_json::to_string(&aged).expect("serialize"),
    )
    .expect("rewrite aged claim");

    let report =
        caduceus::state::queue::reap_stale_review_claims(&dir, Utc::now(), cfg.stale_run_hours)
            .await
            .expect("reap succeeds");

    assert_eq!(
        report.stale_reaped, 0,
        "live pid must not be reaped: {report:?}"
    );
    assert!(claim_path.exists(), "claim untouched for a live worker");
    assert_eq!(entry_phase(&dir), ReviewPhase::InProgress);
}

// ---------------------------------------------------------------------------
// Attempt-burn absence: InProgress → Queued without touching attempts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reaped_entry_returns_to_queued_without_burning_attempts() {
    let dir = tempdir("review-reaper-attempts");
    let cfg = Config::test_defaults(&dir);
    let store = ReviewStore::open(&dir).expect("open store");
    store.enqueue_review(&target()).expect("seed entry");

    // Burn one attempt through the real API so the pre-reap value is
    // non-zero: retry_or_fail moves InProgress → Queued with
    // attempts = 1, then re-acquire claims it again (attempts stays 1).
    let claimed = store
        .acquire_next_review("run-1", std::process::id(), Utc::now())
        .expect("acquire succeeds")
        .expect("one eligible entry");
    let phase = store
        .retry_or_fail_review(claimed.claim, "simulated failure", 5)
        .expect("retry succeeds");
    assert_eq!(phase, ReviewPhase::Queued);
    let claimed = store
        .acquire_next_review(
            "run-2",
            std::process::id(),
            Utc::now() + Duration::seconds(301),
        )
        .expect("re-acquire succeeds")
        .expect("one eligible entry");
    assert_eq!(claimed.entry.attempts, 1, "pre-reap attempts");

    let key = review_queue_key(&target());
    let digest = review_claim_digest(&key);
    let claim_path = dir.join("review-claims").join(format!("{digest}.claim"));
    write_stale_claim(&dir, &claim_path, "run-2", Utc::now() - Duration::hours(2));

    let report =
        caduceus::state::queue::reap_stale_review_claims(&dir, Utc::now(), cfg.stale_run_hours)
            .await
            .expect("reap succeeds");
    assert_eq!(report.stale_reaped, 1, "{report:?}");
    assert!(!claim_path.exists(), "orphan claim unlinked");

    let reopened = ReviewStore::open(&dir).expect("re-open store");
    let queue = reopened.review_queue_snapshot().expect("load queue");
    let entry = queue.entries.values().next().expect("entry survives");
    assert_eq!(entry.phase, ReviewPhase::Queued, "InProgress → Queued");
    assert_eq!(entry.attempts, 1, "attempts NOT incremented by the reaper");
    assert!(
        entry.next_attempt_at.is_some(),
        "re-claimable immediately (next_attempt_at = now)"
    );
}
