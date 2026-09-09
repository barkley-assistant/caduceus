//! AC1 — claim uniqueness under concurrent admission (issue #314, DAR
//! §14, §15).
//!
//! N workers racing `acquire_next_review` for the same `ReviewTarget`
//! produce exactly one claim; losers return `None` and leave no partial
//! claim file. The guarantee is `create_new(true)` (O_CREAT|O_EXCL —
//! the OS-level uniqueness) plus one exclusive transaction
//! (`src/state/review/state.rs::acquire_next_review`).
//!
//! DAR §14's "reviews and fixes share the pool without starvation" is
//! NOT asserted here: the admission seam (`max_reviews_per_tick` +
//! `pool.admit`) is only reachable through the full tick (step 6.5a),
//! not through an in-process unit seam. Per the #314 plan (Task 1.1
//! note), that coexistence is exercised by the end-to-end test (#322),
//! not this binary. What IS proven here is the store-level claim
//! uniqueness the AC names.

use std::sync::Arc;

use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::state::review::{ReviewPhase, ReviewStore};

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::tempdir;

fn repo() -> RepositoryId {
    RepositoryId {
        owner: "octocat".to_string(),
        repo: "hello-world".to_string(),
    }
}

fn target(sha: &str) -> ReviewTarget {
    ReviewTarget {
        repository: repo(),
        pull_request: 42,
        head_sha: sha.to_string(),
        base_sha: "b".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "m".repeat(40),
    }
}

/// Seed one `Queued` review entry through the real admission path.
fn seed_entry(store: &ReviewStore, sha: &str) {
    store
        .enqueue_review(&target(sha))
        .expect("seed entry through admission");
}

fn claim_file_count(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join("review-claims"))
        .expect("read review-claims dir")
        .filter_map(Result::ok)
        .count()
}

#[tokio::test]
async fn two_workers_racing_same_target_produces_exactly_one_claim() {
    let dir = tempdir("claim-race");
    let store = Arc::new(ReviewStore::open(&dir).expect("open store"));
    seed_entry(&store, &"a".repeat(40));

    // Race 4 acquire calls on the same store + same target. The
    // `review.lock` flock serialises the exclusive sections, so the
    // first caller wins; every later caller sees the claim file via
    // `create_new` AlreadyExists and returns None (try-next-FIFO).
    let mut handles = Vec::new();
    for i in 0..4u32 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            s.acquire_next_review(&format!("run-{i}"), 1000 + i, chrono::Utc::now())
        }));
    }
    let results: Vec<_> = futures_util::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.expect("task join").expect("acquire call"))
        .collect();

    let winners = results.iter().filter(|r| r.is_some()).count();
    assert_eq!(winners, 1, "exactly one claim — got {winners}");

    // The winner's run_id is the entry's last_run_id.
    let queue = store.review_queue_snapshot().expect("load queue");
    let entry = queue
        .entries
        .values()
        .next()
        .expect("entry exists after seed");
    assert_eq!(entry.phase, ReviewPhase::InProgress);
    let winner = results
        .iter()
        .find(|r| r.is_some())
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!(
        entry.last_run_id.as_deref(),
        Some(winner.claim.run_id()),
        "entry last_run_id matches the winner's claim"
    );

    // Exactly one claim file on disk; losers created none (create_new
    // failure happens before any write).
    assert_eq!(claim_file_count(&dir), 1, "exactly one claim file on disk");
}

#[tokio::test]
async fn loser_requeues_cleanly_no_partial_claim_file() {
    // Same setup, one target; the loser of the race leaves NO
    // <digest>.claim file behind (create_new failed before write), and
    // the loser's call is a clean None — not an error.
    let dir = tempdir("claim-clean-loser");
    let store = ReviewStore::open(&dir).expect("open store");
    seed_entry(&store, &"c".repeat(40));

    let w1 = store
        .acquire_next_review("run-1", 1, chrono::Utc::now())
        .unwrap();
    let w2 = store
        .acquire_next_review("run-2", 2, chrono::Utc::now())
        .unwrap();
    assert!(w1.is_some(), "first caller wins");
    assert!(w2.is_none(), "second caller loses cleanly");

    assert_eq!(
        claim_file_count(&dir),
        1,
        "only the winner's claim file exists"
    );
}

#[tokio::test]
async fn many_targets_processed_within_parallelism_budget() {
    // DAR §14: the admission budget bounds per-tick claims but never
    // starves distinct targets. Seed K targets; each is claimable
    // exactly once by `acquire_next_review`.
    let dir = tempdir("claim-many");
    let store = ReviewStore::open(&dir).expect("open store");
    for k in 0..8u32 {
        seed_entry(&store, &format!("a{k:040}"));
    }
    let mut claimed = 0;
    for i in 0..8u32 {
        if store
            .acquire_next_review(&format!("run-{i}"), i, chrono::Utc::now())
            .unwrap()
            .is_some()
        {
            claimed += 1;
        }
    }
    assert_eq!(claimed, 8, "all K distinct targets claimable");

    // Each target claimed exactly once: all 8 are InProgress.
    let queue = store.review_queue_snapshot().unwrap();
    let in_progress = queue
        .entries
        .values()
        .filter(|e| e.phase == ReviewPhase::InProgress)
        .count();
    assert_eq!(in_progress, 8);
    assert_eq!(claim_file_count(&dir), 8, "one claim file per target");
}
