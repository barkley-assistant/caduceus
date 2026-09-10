//! Review store tests (issue #295): round-trips, dedup, append-only
//! history, generation monotonicity, claim lifecycle, and durability
//! under both backends.
//!
//! AC coverage:
//!
//! - AC1 — both backends, strict serde, validated load (round-trips,
//!   unknown-field / wrong-version / key-mismatch / corrupt rejection).
//! - AC3 — append-only history keyed by `review_run_id`; multiple rows
//!   per `(repo, pr, head_sha)`; duplicate run id rejected.
//! - AC4 — `review_generation` persisted and monotonic per (repo, pr).
//! - AC5 — dedup = `last_reviewed_head_sha` + active queue, never a
//!   history uniqueness constraint.
//! - AC6 — persistence durable across restart; corrupted input
//!   rejected safely on both backends.

use caduceus::review::{
    ExecutionStatus, PublicationState, RepositoryId, Review, ReviewResult, ReviewState,
    ReviewTarget, Verdict, REVIEW_SCHEMA_VERSION,
};
use caduceus::state::queue::StateStore;
use caduceus::state::review::{
    parse_review_history, parse_review_queue_state, parse_review_state_map, review_queue_key,
    review_state_key, serialize_review_history, serialize_review_queue_state,
    serialize_review_state_map, EnqueueReason, ReviewEnqueueOutcome, ReviewHistoryFile,
    ReviewHistoryRow, ReviewPhase, ReviewQueueState, ReviewStateMap, ReviewStore,
    REVIEW_HISTORY_FILE_VERSION, REVIEW_QUEUE_FILE_VERSION, REVIEW_STATE_FILE_VERSION,
};
use chrono::{TimeZone, Utc};
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::fs;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const OWNER: &str = "OwnerOne";
const REPO: &str = "RepoOne";
const PR: u64 = 42;
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_D: &str = "dddddddddddddddddddddddddddddddddddddddd";

fn sample_repository() -> RepositoryId {
    RepositoryId {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    }
}

fn target_with_sha(head_sha: &str) -> ReviewTarget {
    ReviewTarget {
        repository: sample_repository(),
        pull_request: PR,
        head_sha: head_sha.to_string(),
        base_sha: SHA_B.to_string(),
        base_ref: "main".to_string(),
        merge_base: "cccccccccccccccccccccccccccccccccccccccc".to_string(),
    }
}

fn sample_target() -> ReviewTarget {
    target_with_sha(SHA_A)
}

fn valid_result_json(status: ExecutionStatus) -> String {
    serde_json::to_string(&ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status,
        review: match status {
            ExecutionStatus::Success => Some(Review {
                verdict: Verdict::Pass,
                summary: "ok".to_string(),
                findings: vec![],
            }),
            ExecutionStatus::Failure => None,
        },
    })
    .unwrap()
}

fn history_row(run_id: &str, sha: &str, generation: u64) -> ReviewHistoryRow {
    ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: sample_repository(),
        pull_request: PR,
        head_sha: sha.to_string(),
        review_generation: generation,
        completed_at: Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap(),
        result_json: valid_result_json(ExecutionStatus::Success),
    }
}

// ---------------------------------------------------------------------------
// Part 1 — parse/serialize contracts (pure functions)
// ---------------------------------------------------------------------------

#[test]
fn review_queue_round_trips_strict() {
    let target = sample_target();
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        review_queue_key(&target),
        caduceus::state::review::ReviewQueueEntry {
            target: target.clone(),
            phase: ReviewPhase::Queued,
            attempts: 0,
            last_error: None,
            last_run_id: None,
            next_attempt_at: None,
            queued_at: Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap(),
            review_generation: 1,
            blocked_source: None,
            blocked_recovery_hint: None,
        },
    );
    let state = ReviewQueueState {
        version: REVIEW_QUEUE_FILE_VERSION,
        entries,
    };
    let text = serialize_review_queue_state(&state).unwrap();
    let back = parse_review_queue_state(&text).unwrap();
    assert_eq!(back, state);
}

#[test]
fn review_queue_rejects_unknown_fields() {
    // Rogue top-level field on a valid empty envelope.
    let bad = r#"{"version":1,"entries":{},"rogue":"x"}"#;
    let err = parse_review_queue_state(bad).expect_err("unknown top-level field rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("rogue"), "got: {msg}");

    // Rogue field inside an entry object (strict serde at every layer).
    let target = sample_target();
    let entry_json = serde_json::json!({
        "target": {
            "repository": {"owner": OWNER, "repo": REPO},
            "pull_request": PR,
            "head_sha": SHA_A,
            "base_sha": SHA_B,
            "base_ref": "main",
            "merge_base": "cccccccccccccccccccccccccccccccccccccccc",
        },
        "phase": "queued",
        "attempts": 0,
        "last_error": null,
        "last_run_id": null,
        "next_attempt_at": null,
        "queued_at": "2026-09-05T12:00:00Z",
        "updated_at": "2026-09-05T12:00:00Z",
        "review_generation": 1,
    });
    let bad_entry = format!(
        r#"{{"version":1,"entries":{{"{key}":{entry_json},"rogue":"y"}}}}"#,
        key = review_queue_key(&target),
    );
    let err = parse_review_queue_state(&bad_entry).expect_err("unknown field rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
}

#[test]
fn review_queue_rejects_wrong_version_both_directions() {
    for version in [0u32, 2] {
        let bad = format!(r#"{{"version":{version},"entries":{{}}}}"#);
        let err = parse_review_queue_state(&bad).expect_err("wrong envelope version");
        match err {
            caduceus::error::CaduceusError::StoreVersionUnsupported { backend, found, .. } => {
                assert_eq!(backend, "json");
                assert_eq!(found, version as i64);
            }
            other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
        }
    }
}

#[test]
fn review_queue_rejects_key_entry_mismatch() {
    let target = sample_target();
    let entry_json = serde_json::json!({
        "target": {
            "repository": {"owner": OWNER, "repo": REPO},
            "pull_request": PR,
            "head_sha": SHA_A,
            "base_sha": SHA_B,
            "base_ref": "main",
            "merge_base": "cccccccccccccccccccccccccccccccccccccccc",
        },
        "phase": "queued",
        "attempts": 0,
        "last_error": null,
        "last_run_id": null,
        "next_attempt_at": null,
        "queued_at": "2026-09-05T12:00:00Z",
        "updated_at": "2026-09-05T12:00:00Z",
        "review_generation": 1,
    });
    let bad = format!(r#"{{"version":1,"entries":{{"other/repo#1@{SHA_A}":{entry_json}}}}}"#);
    let err = parse_review_queue_state(&bad).expect_err("map key mismatch");
    let msg = format!("{err:?}");
    assert!(msg.contains("does not match entry"), "got: {msg}");
    assert!(msg.contains(&review_queue_key(&target)), "got: {msg}");
}

#[test]
fn review_state_map_round_trips_and_validates() {
    let mut states = std::collections::BTreeMap::new();
    let mut state = ReviewState::new(sample_repository(), PR, 7);
    state.last_reviewed_head_sha = Some(SHA_A.to_string());
    states.insert(review_state_key(&sample_repository(), PR), state);
    let map = ReviewStateMap {
        version: REVIEW_STATE_FILE_VERSION,
        states,
    };
    let text = serialize_review_state_map(&map).unwrap();
    let back = parse_review_state_map(&text).unwrap();
    assert_eq!(back, map);
    // The generation survives verbatim.
    let stored = back
        .states
        .get(&review_state_key(&sample_repository(), PR))
        .unwrap();
    assert_eq!(stored.review_generation, 7);
}

#[test]
fn review_state_map_rejects_generation_zero_entry() {
    // Not applicable to the state map (generation 0 is legal before
    // first admission there) — but the QUEUE rejects it; covered in
    // review_queue generation tests. Here: state map key mismatch.
    let mut states = std::collections::BTreeMap::new();
    states.insert(
        "wrong/repo#1".to_string(),
        ReviewState::new(sample_repository(), PR, 1),
    );
    let map = ReviewStateMap {
        version: REVIEW_STATE_FILE_VERSION,
        states,
    };
    let text = serialize_review_state_map(&map).unwrap();
    let err = parse_review_state_map(&text).expect_err("map key mismatch");
    let msg = format!("{err:?}");
    assert!(msg.contains("does not match entry"), "got: {msg}");
}

#[test]
fn review_history_round_trips_and_preserves_order() {
    let file = ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: vec![
            history_row("run-1", SHA_A, 1),
            history_row("run-2", SHA_A, 2),
        ],
    };
    let text = serialize_review_history(&file).unwrap();
    let back = parse_review_history(&text).unwrap();
    assert_eq!(back, file);
    assert_eq!(back.rows.len(), 2);
    assert_eq!(back.rows[0].review_run_id, "run-1");
    assert_eq!(back.rows[1].review_run_id, "run-2");
}

#[test]
fn review_history_rejects_non_json_result_blob() {
    let mut row = history_row("run-bad", SHA_A, 1);
    row.result_json = "not json".to_string();
    let file = ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: vec![row],
    };
    let text = serialize_review_history(&file).unwrap();
    let err = parse_review_history(&text).expect_err("non-JSON blob rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("not valid JSON"), "got: {msg}");
}

#[test]
fn review_history_accepts_older_schema_version_blob_as_opaque() {
    // An old-version blob (schema_version 0 — not the current 1) is
    // accepted as an opaque, read-only row: old versions are never
    // back-migrated (DAR §4.3).
    let mut row = history_row("run-old", SHA_A, 1);
    row.result_json = r#"{"schema_version":0,"status":"success","review":null}"#.to_string();
    let file = ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: vec![row],
    };
    let text = serialize_review_history(&file).unwrap();
    parse_review_history(&text).expect("older blob is opaque");
}

#[test]
fn review_history_rejects_semantically_invalid_current_blob() {
    // A current-version blob that fails the #305 rules (FAIL with
    // zero blocking findings) must fail the history load — the store
    // composes the same domain validator as the ingress.
    let mut row = history_row("run-bad-verdict", SHA_A, 1);
    row.result_json = r#"{"schema_version":1,"status":"success","review":{"verdict":"fail","summary":"s","findings":[]}}"#.to_string();
    let file = ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: vec![row],
    };
    let text = serialize_review_history(&file).unwrap();
    let err = parse_review_history(&text).expect_err("inconsistent blob rejected");
    assert!(format!("{err:?}").contains("blocking"), "got: {err:?}");

    // Presence violation, same treatment.
    let mut row = history_row("run-no-review", SHA_A, 1);
    row.result_json = r#"{"schema_version":1,"status":"success","review":null}"#.to_string();
    let file = ReviewHistoryFile {
        version: REVIEW_HISTORY_FILE_VERSION,
        rows: vec![row],
    };
    let text = serialize_review_history(&file).unwrap();
    let err = parse_review_history(&text).expect_err("presence violation rejected");
    assert!(
        format!("{err:?}").contains("must be present"),
        "got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Part 2 — JSON store ops
// ---------------------------------------------------------------------------

#[test]
fn json_enqueue_assigns_generation_and_upserts_state() {
    let dir = tempdir("rv-enq");
    let store = ReviewStore::open(&dir).unwrap();
    let outcome = store.enqueue_review(&sample_target()).unwrap();
    assert!(matches!(outcome, ReviewEnqueueOutcome::Inserted));
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st.review_generation, 1);
    let q = store.review_queue_snapshot().unwrap();
    assert_eq!(q.entries.len(), 1);

    // Second, different SHA → generation 2 on state, second queue entry
    // (AC4 monotonic).
    let t2 = target_with_sha(SHA_D);
    store.enqueue_review(&t2).unwrap();
    let st2 = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st2.review_generation, 2);
    assert_eq!(store.review_queue_snapshot().unwrap().entries.len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_enqueue_same_target_twice_is_already_present() {
    let dir = tempdir("rv-dup");
    let store = ReviewStore::open(&dir).unwrap();
    assert!(matches!(
        store.enqueue_review(&sample_target()).unwrap(),
        ReviewEnqueueOutcome::Inserted
    ));
    assert!(matches!(
        store.enqueue_review(&sample_target()).unwrap(),
        ReviewEnqueueOutcome::AlreadyPresent
    ));
    assert_eq!(store.review_queue_snapshot().unwrap().entries.len(), 1);
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st.review_generation, 1, "generation NOT bumped on dup");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explicit_reason_bypasses_active_entry_dedup_and_bumps_generation() {
    // AC1/AC3 (issue #335): a trusted-author explicit request enqueues
    // a NEW run for the SAME SHA even when an active entry exists —
    // the entry is REPLACED in place (same key, bumped generation),
    // never duplicated.
    let dir = tempdir("rv-explicit");
    let store = ReviewStore::open(&dir).unwrap();
    assert!(matches!(
        store.enqueue_review(&sample_target()).unwrap(),
        ReviewEnqueueOutcome::Inserted
    ));
    assert!(matches!(
        store
            .enqueue_review_with_reason(&sample_target(), EnqueueReason::ExplicitUserRequest)
            .unwrap(),
        ReviewEnqueueOutcome::Inserted
    ));
    let q = store.review_queue_snapshot().unwrap();
    assert_eq!(
        q.entries.len(),
        1,
        "explicit re-enqueue replaces, not appends"
    );
    let entry = q.entries.get(&review_queue_key(&sample_target())).unwrap();
    assert_eq!(
        entry.review_generation, 2,
        "generation bumped by explicit re-enqueue"
    );
    assert!(entry.phase.is_active());
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(
        st.review_generation, 2,
        "state generation follows the queue"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn auto_discovery_reason_returns_already_present_for_active_entry() {
    // AC2 (issue #335): the auto-discovery reason never bypasses the
    // active-entry dedup — polling can NEVER trigger same-SHA
    // re-review.
    let dir = tempdir("rv-auto-dup");
    let store = ReviewStore::open(&dir).unwrap();
    assert!(matches!(
        store
            .enqueue_review_with_reason(&sample_target(), EnqueueReason::AutoDiscovery)
            .unwrap(),
        ReviewEnqueueOutcome::Inserted
    ));
    assert!(matches!(
        store
            .enqueue_review_with_reason(&sample_target(), EnqueueReason::AutoDiscovery)
            .unwrap(),
        ReviewEnqueueOutcome::AlreadyPresent
    ));
    assert_eq!(store.review_queue_snapshot().unwrap().entries.len(), 1);
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st.review_generation, 1, "generation NOT bumped on auto dup");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explicit_reason_resets_publication_state() {
    // An explicit re-enqueue re-arms publication: a `Published` state
    // row from generation N must not block generation N+1's
    // publication (DAR §9.4).
    let dir = tempdir("rv-explicit-pub");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    // Progress the publication FSM so the reset is observable: mark
    // the state Published with retry debt, exactly as a completed
    // generation-N run would leave it.
    let mut st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    st.publication_state = PublicationState::Published;
    st.publication_attempt_count = 3;
    st.next_publish_at = Some(Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap());
    store.save_review_state(&st).unwrap();

    assert!(matches!(
        store
            .enqueue_review_with_reason(&sample_target(), EnqueueReason::ExplicitUserRequest)
            .unwrap(),
        ReviewEnqueueOutcome::Inserted
    ));
    let after = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(after.review_generation, 2);
    assert_eq!(after.publication_state, PublicationState::Pending);
    assert_eq!(after.publication_attempt_count, 0);
    assert_eq!(after.next_publish_at, None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_acquire_claim_and_complete_lifecycle() {
    let dir = tempdir("rv-lifecycle");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();

    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-lc-1", 4242, now)
        .unwrap()
        .expect("claim the queued entry");
    assert_eq!(claimed.entry.phase, ReviewPhase::InProgress);
    assert_eq!(claimed.entry.last_run_id.as_deref(), Some("run-lc-1"));

    // Claim file exists under review-claims/.
    let claims_dir = store.claims_dir();
    let claim_path = claims_dir.join(format!("{}.claim", claimed.claim.digest()));
    assert!(claim_path.is_file(), "claim file must exist");

    // The claims directory is the REVIEW one, not the issue queue's.
    assert!(claims_dir.ends_with("review-claims"));

    store.complete_review(claimed.claim).unwrap();
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.get(&review_queue_key(&sample_target())).unwrap();
    assert_eq!(entry.phase, ReviewPhase::Done);
    assert!(!claim_path.exists(), "claim file gone after completion");

    // Acquire again → None (no eligible entry).
    let again = store.acquire_next_review("run-lc-2", 4243, now).unwrap();
    assert!(again.is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_retry_or_fail_matches_issue_queue_semantics() {
    let dir = tempdir("rv-retry");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    // next_attempt_at is assigned from the real clock (mirroring the
    // issue queue's retry_or_fail), so the claim instants derive from
    // Utc::now() too.
    let now = Utc::now();

    let claimed = store
        .acquire_next_review("run-r1", 4242, now)
        .unwrap()
        .unwrap();
    let phase = store
        .retry_or_fail_review(claimed.claim, "boom", 2)
        .unwrap();
    assert_eq!(phase, ReviewPhase::Queued);
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.get(&review_queue_key(&sample_target())).unwrap();
    assert_eq!(entry.attempts, 1);
    assert!(entry.next_attempt_at.is_some(), "backoff window set");

    // Elapse the backoff, claim again, fail again → Failed.
    let later = Utc::now() + chrono::Duration::seconds(301);
    let claimed2 = store
        .acquire_next_review("run-r2", 4242, later)
        .unwrap()
        .unwrap();
    let phase2 = store
        .retry_or_fail_review(claimed2.claim, "boom again", 2)
        .unwrap();
    assert_eq!(phase2, ReviewPhase::Failed);
    let q2 = store.review_queue_snapshot().unwrap();
    let entry2 = q2.entries.get(&review_queue_key(&sample_target())).unwrap();
    assert_eq!(entry2.attempts, 2);
    assert!(entry2.next_attempt_at.is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_skip_review_records_reason() {
    let dir = tempdir("rv-skip");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-s1", 4242, now)
        .unwrap()
        .unwrap();
    store.skip_review(claimed.claim, "not needed").unwrap();
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.get(&review_queue_key(&sample_target())).unwrap();
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.last_error.as_deref(), Some("not needed"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn needs_attention_blocked_fields_round_trip_both_backends() {
    for open_store in [
        |dir: &std::path::Path| ReviewStore::open(dir).unwrap(),
        |dir: &std::path::Path| ReviewStore::open_sqlite(dir).unwrap(),
    ] {
        let dir = tempdir("rv-blocked-rt");
        let store = open_store(&dir);
        store.enqueue_review(&sample_target()).unwrap();
        let now = Utc::now();
        let claimed = store
            .acquire_next_review("run-na-1", 4242, now)
            .unwrap()
            .expect("claim");
        store
            .route_review_to_needs_attention(
                claimed.claim,
                "review mutation violation",
                "review/mutation_violation",
                "inspect the archived worktree at /tmp/wt",
            )
            .unwrap();
        let snap = store.review_queue_snapshot().unwrap();
        let entry = snap
            .entries
            .get(&review_queue_key(&sample_target()))
            .unwrap();
        assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
        assert_eq!(
            entry.blocked_recovery_hint.as_deref(),
            Some("inspect the archived worktree at /tmp/wt")
        );
        assert_eq!(
            entry.blocked_source.as_deref(),
            Some("review/mutation_violation")
        );
        assert_eq!(
            entry.last_error.as_deref(),
            Some("review mutation violation")
        );
        // AC4: terminal routing never touches the attempt counter.
        assert_eq!(entry.attempts, 0);
        assert!(entry.last_run_id.is_none());
        assert!(entry.next_attempt_at.is_none());
        // Claim released: nothing is re-acquirable.
        assert!(store
            .acquire_next_review("run-na-2", 4243, now)
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn v1_review_queue_json_without_blocked_fields_still_parses() {
    // The JSON envelope stays v1: an entry serialized WITHOUT the
    // blocked fields (a #295-era file) must still parse — the fields
    // are #[serde(default)] additive (issue-queue precedent).
    let target = sample_target();
    let entry_json = serde_json::json!({
        "target": {
            "repository": {"owner": OWNER, "repo": REPO},
            "pull_request": PR,
            "head_sha": SHA_A,
            "base_sha": SHA_B,
            "base_ref": "main",
            "merge_base": "cccccccccccccccccccccccccccccccccccccccc",
        },
        "phase": "queued",
        "attempts": 0,
        "last_error": null,
        "last_run_id": null,
        "next_attempt_at": null,
        "queued_at": "2026-09-05T12:00:00Z",
        "updated_at": "2026-09-05T12:00:00Z",
        "review_generation": 1,
    });
    let text = format!(
        r#"{{"version":1,"entries":{{"{key}":{entry_json}}}}}"#,
        key = review_queue_key(&target),
    );
    let parsed = parse_review_queue_state(&text).expect("v1 without blocked fields parses");
    let entry = parsed
        .entries
        .get(&review_queue_key(&target))
        .expect("entry present");
    assert_eq!(entry.blocked_source, None);
    assert_eq!(entry.blocked_recovery_hint, None);
}

#[test]
fn json_claim_mismatch_rejected() {
    let dir = tempdir("rv-mismatch");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let _claimed = store
        .acquire_next_review("run-m1", 4242, now)
        .unwrap()
        .unwrap();
    // A forged token with the wrong digest is rejected.
    let forged = caduceus::state::review::ReviewClaimToken::for_test(
        store.claims_dir(),
        "deadbeef",
        "run-m1",
    );
    let err = store.complete_review(forged).expect_err("forged token");
    let msg = format!("{err:?}");
    assert!(msg.contains("mismatch"), "got: {msg}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_generation_never_decreases_via_cas() {
    let dir = tempdir("rv-cas");
    let store = ReviewStore::open(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    let stored = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(stored.review_generation, 1);

    // Regression rejected.
    let mut regressed = stored.clone();
    regressed.review_generation = 0;
    let err = store.save_review_state(&regressed).expect_err("CAS guard");
    let msg = format!("{err:?}");
    assert!(msg.contains("regression"), "got: {msg}");

    // Equal-or-higher accepted.
    let mut advanced = stored.clone();
    advanced.review_generation = 2;
    store.save_review_state(&advanced).unwrap();
    let now_stored = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(now_stored.review_generation, 2);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Part 3 — history ops + dedup + durability (JSON)
// ---------------------------------------------------------------------------

#[test]
fn json_history_appends_and_permits_same_sha_multiples() {
    let dir = tempdir("rv-hist");
    let store = ReviewStore::open(&dir).unwrap();
    store
        .append_history(history_row("run-h1", SHA_A, 1))
        .unwrap();
    store
        .append_history(history_row("run-h2", SHA_A, 2))
        .unwrap();
    let rows = store
        .history_for_head_sha(&sample_repository(), PR, SHA_A)
        .unwrap();
    assert_eq!(rows.len(), 2, "AC3: same-SHA multiplicity");
    assert_eq!(rows[0].review_run_id, "run-h1");
    assert_eq!(rows[1].review_run_id, "run-h2");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_history_rejects_duplicate_run_id() {
    let dir = tempdir("rv-histdup");
    let store = ReviewStore::open(&dir).unwrap();
    store
        .append_history(history_row("run-d1", SHA_A, 1))
        .unwrap();
    let err = store
        .append_history(history_row("run-d1", SHA_A, 1))
        .expect_err("duplicate run id");
    let msg = format!("{err:?}");
    assert!(msg.contains("duplicate review_run_id"), "got: {msg}");
    assert_eq!(
        store
            .history_for_repository(&sample_repository())
            .unwrap()
            .len(),
        1
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_dedup_is_state_plus_active_queue_not_history() {
    let dir = tempdir("rv-dedup");
    let store = ReviewStore::open(&dir).unwrap();
    let target = sample_target();

    // Fresh target → false.
    assert!(!store.is_active_or_reviewed(&target).unwrap());

    // Enqueue → true (active queue).
    store.enqueue_review(&target).unwrap();
    assert!(store.is_active_or_reviewed(&target).unwrap());

    // Complete + record the reviewed SHA → still true via state even
    // though the queue entry is terminal (AC5).
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-dd1", 4242, now)
        .unwrap()
        .unwrap();
    store.complete_review(claimed.claim).unwrap();
    let mut state = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    state.last_reviewed_head_sha = Some(SHA_A.to_string());
    state.last_verdict = Some(caduceus::review::Verdict::Pass);
    store.save_review_state(&state).unwrap();
    assert!(store.is_active_or_reviewed(&target).unwrap());

    // A DIFFERENT sha on the same pr → false.
    let t2 = target_with_sha(SHA_D);
    assert!(!store.is_active_or_reviewed(&t2).unwrap());

    // History rows never affect dedup: append one for the different
    // SHA and confirm dedup still says false for it (dedup is NOT a
    // history query).
    store
        .append_history(history_row("run-dd2", SHA_D, 1))
        .unwrap();
    assert!(!store.is_active_or_reviewed(&t2).unwrap());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_restart_durability_round_trip() {
    let dir = tempdir("rv-durability");
    {
        let store = ReviewStore::open(&dir).unwrap();
        store.enqueue_review(&sample_target()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
        let claimed = store
            .acquire_next_review("run-dur", 4242, now)
            .unwrap()
            .unwrap();
        store.complete_review(claimed.claim).unwrap();
        let mut state = store
            .review_state(&sample_repository(), PR)
            .unwrap()
            .unwrap();
        state.last_reviewed_head_sha = Some(SHA_A.to_string());
        store.save_review_state(&state).unwrap();
        store
            .append_history(history_row("run-dur", SHA_A, 1))
            .unwrap();
    }
    // Re-open (drop = "restart") → all three surfaces identical.
    let store = ReviewStore::open(&dir).unwrap();
    let q = store.review_queue_snapshot().unwrap();
    assert_eq!(q.entries.len(), 1);
    assert_eq!(
        q.entries
            .get(&review_queue_key(&sample_target()))
            .unwrap()
            .phase,
        ReviewPhase::Done
    );
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st.last_reviewed_head_sha.as_deref(), Some(SHA_A));
    let rows = store.history_for_repository(&sample_repository()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].review_run_id, "run-dur");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn json_corrupted_files_rejected_safely() {
    for (filename, probe) in [
        ("review_queue.json", "review queue JSON parse"),
        ("review_state.json", "review state JSON parse"),
        ("review_history.json", "review history JSON parse"),
    ] {
        let dir = tempdir("rv-corrupt");
        // Open once to create the layout, then corrupt the file.
        {
            let _store = ReviewStore::open(&dir).unwrap();
        }
        fs::write(dir.join(filename), "{{{not json").unwrap();
        let err = ReviewStore::open(&dir).expect_err("corrupt file rejected");
        let msg = format!("{err:?}");
        assert!(msg.contains("StateCorrupt"), "got: {msg}");
        assert!(msg.contains(probe), "got: {msg}");
        // The corrupt file is preserved for diagnosis, never deleted.
        assert!(dir.join(filename).is_file(), "{filename} not deleted");
        let _ = fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// Part 4 — SQLite backend
// ---------------------------------------------------------------------------

#[test]
fn sqlite_store_round_trips_all_three_surfaces() {
    let dir = tempdir("rv-sqlite");
    {
        let store = ReviewStore::open_sqlite(&dir).unwrap();
        store.enqueue_review(&sample_target()).unwrap();
        let t2 = target_with_sha(SHA_D);
        store.enqueue_review(&t2).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
        let claimed = store
            .acquire_next_review("run-sq", 4242, now)
            .unwrap()
            .unwrap();
        store.complete_review(claimed.claim).unwrap();
        let mut state = store
            .review_state(&sample_repository(), PR)
            .unwrap()
            .unwrap();
        state.last_reviewed_head_sha = Some(SHA_A.to_string());
        state.sticky_comment_id = Some(99);
        store.save_review_state(&state).unwrap();
        store
            .append_history(history_row("run-sq", SHA_A, 1))
            .unwrap();
        store
            .append_history(history_row("run-sq2", SHA_A, 2))
            .unwrap();
    }
    // Re-open → identical snapshot/state/history (AC1 + AC6).
    let store = ReviewStore::open_sqlite(&dir).unwrap();
    let q = store.review_queue_snapshot().unwrap();
    assert_eq!(q.entries.len(), 2);
    assert_eq!(
        q.entries
            .get(&review_queue_key(&sample_target()))
            .unwrap()
            .phase,
        ReviewPhase::Done
    );
    let st = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    assert_eq!(st.last_reviewed_head_sha.as_deref(), Some(SHA_A));
    assert_eq!(st.sticky_comment_id, Some(99));
    assert_eq!(st.review_generation, 2, "second enqueue bumped to 2");
    let rows = store
        .history_for_head_sha(&sample_repository(), PR, SHA_A)
        .unwrap();
    assert_eq!(rows.len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_dedup_generation_history_mirror_json_semantics() {
    let dir = tempdir("rv-sq-mirror");
    let store = ReviewStore::open_sqlite(&dir).unwrap();
    let target = sample_target();

    // Dedup mirror.
    assert!(!store.is_active_or_reviewed(&target).unwrap());
    store.enqueue_review(&target).unwrap();
    assert!(store.is_active_or_reviewed(&target).unwrap());

    // Generation mirror (AC4): CAS rejects regressions.
    let stored = store
        .review_state(&sample_repository(), PR)
        .unwrap()
        .unwrap();
    let mut regressed = stored.clone();
    regressed.review_generation = 0;
    assert!(store.save_review_state(&regressed).is_err());

    // History mirror (AC3): duplicate run id rejected; same-SHA
    // multiplicity allowed.
    store
        .append_history(history_row("run-m1", SHA_A, stored.review_generation))
        .unwrap();
    assert!(store
        .append_history(history_row("run-m1", SHA_A, stored.review_generation))
        .is_err());
    store
        .append_history(history_row("run-m2", SHA_A, stored.review_generation))
        .unwrap();
    assert_eq!(
        store
            .history_for_head_sha(&sample_repository(), PR, SHA_A)
            .unwrap()
            .len(),
        2
    );

    // Different SHA → false.
    assert!(!store
        .is_active_or_reviewed(&target_with_sha(SHA_D))
        .unwrap());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_claim_lifecycle_with_backoff() {
    let dir = tempdir("rv-sq-claim");
    let store = ReviewStore::open_sqlite(&dir).unwrap();
    store.enqueue_review(&sample_target()).unwrap();
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-sc1", 4242, now)
        .unwrap()
        .unwrap();
    let claim_path = store
        .claims_dir()
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(claim_path.is_file());
    let phase = store
        .retry_or_fail_review(claimed.claim, "boom", 1)
        .unwrap();
    assert_eq!(phase, ReviewPhase::Failed);
    assert!(!claim_path.exists(), "claim unlinked on terminal");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_corrupt_row_rejected_on_load() {
    let dir = tempdir("rv-sq-corrupt");
    {
        let _store = ReviewStore::open_sqlite(&dir).unwrap();
    }
    // Hand-write a bad enum label into review_state.publication_state.
    let conn = rusqlite::Connection::open(dir.join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO review_state (owner, repo, pull_request, review_generation, publication_state)
         VALUES ('owner', 'repo', 1, 1, 'not_a_real_state')",
        [],
    )
    .unwrap();
    drop(conn);
    let err = ReviewStore::open_sqlite(&dir).expect_err("corrupt row rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("publication_state"), "got: {msg}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_unknown_version_rejected_and_v7_migrates() {
    let dir = tempdir("rv-sq-migrate");
    // Build a real v7-era store: the current daemon minus the review
    // tables — construct via the era DDL shape used by
    // migration_framework_test (v7 = v6 tables + oci_runs, no review
    // tables).
    let db_path = dir.join("state.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
             INSERT INTO schema_version (version, migrated_at) VALUES (7, '2026-01-01T00:00:00Z');
             CREATE TABLE queue_entries (issue_key TEXT PRIMARY KEY, phase TEXT NOT NULL, ticket_type TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT, next_attempt_at TEXT, finalization TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 1, blocked_source TEXT, blocked_recovery_hint TEXT);
             CREATE TABLE state_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE claims (claim_id TEXT PRIMARY KEY, issue_key TEXT NOT NULL, worker_pid INTEGER, token TEXT NOT NULL, claimed_at TEXT NOT NULL, expires_at TEXT NOT NULL);
             CREATE TABLE checkpoints (run_id TEXT NOT NULL, stage TEXT NOT NULL, checkpoint_data TEXT, created_at TEXT NOT NULL, operation_id TEXT, remote_marker TEXT, PRIMARY KEY (run_id, stage));
             CREATE TABLE circuit_state (scope TEXT NOT NULL, scope_id TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'closed', consecutive_failures INTEGER NOT NULL DEFAULT 0, last_failure_at INTEGER, opened_at INTEGER, last_probe_at INTEGER, PRIMARY KEY (scope, scope_id));
             CREATE TABLE leases (issue_key TEXT PRIMARY KEY, owner_id TEXT NOT NULL, fencing_token INTEGER NOT NULL, expires_at INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('held', 'released', 'expired')));
             CREATE TABLE oci_runs (run_id TEXT PRIMARY KEY, container_id TEXT, state TEXT NOT NULL, engine TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, daemon_id TEXT NOT NULL, issue_id TEXT NOT NULL, worker_command_sha256 TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO queue_entries (issue_key, phase, ticket_type, attempts, queued_at, updated_at, generation)
             VALUES ('owner/repo#1', 'queued', 'code', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        drop(conn);
    }

    // ReviewStore::open_sqlite migrates v7 → v8 and opens cleanly.
    {
        let store = ReviewStore::open_sqlite(&dir).unwrap();
        store.enqueue_review(&sample_target()).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version,
            caduceus::store::SCHEMA_VERSION,
            "v7 migrated to current"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM queue_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "issue row preserved through migration");
        drop(conn);
    }
    // Re-open: the review data survived.
    let store = ReviewStore::open_sqlite(&dir).unwrap();
    assert_eq!(store.review_queue_snapshot().unwrap().entries.len(), 1);

    // Future version → rejected.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version, migrated_at) VALUES (99, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);
    }
    let err = ReviewStore::open_sqlite(&dir).expect_err("v99 rejected");
    match err {
        caduceus::error::CaduceusError::StoreVersionUnsupported { backend, found, .. } => {
            assert_eq!(backend, "sqlite");
            assert_eq!(found, 99);
        }
        other => panic!("expected StoreVersionUnsupported; got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Part 5 — cross-backend: JSON side files never collide with issue queue
// ---------------------------------------------------------------------------

#[test]
fn json_review_files_are_siblings_of_state_json() {
    let dir = tempdir("rv-siblings");
    {
        let review = ReviewStore::open(&dir).unwrap();
        let _issue = StateStore::open(&dir).unwrap();
        // A mutation materialises the review sidecar files (an empty
        // file is absent, same convention as state.json).
        review.enqueue_review(&sample_target()).unwrap();
    }
    assert!(dir.join("review_queue.json").is_file());
    assert!(dir.join("review_state.json").is_file());
    assert!(dir.join("review.lock").is_file());
    assert!(dir.join("review-claims").is_dir());
    // The issue queue's own surfaces are untouched (its lock/claims
    // materialise lazily, same convention).
    assert!(dir.join("claims").is_dir());
    // The review store's files do not confuse the issue store…
    let issue = StateStore::open(&dir).unwrap();
    assert_eq!(issue.snapshot().unwrap().entries.len(), 0);
    let _ = fs::remove_dir_all(&dir);
}
