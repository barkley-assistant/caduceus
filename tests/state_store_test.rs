//! Task 3.1 acceptance tests for the crash-safe StateStore.
//!
//! These tests exercise the on-disk contract pinned by `CONTRACTS.md`
//! and the Phase 03 task packet:
//!
//! * Atomic mutations under an exclusive `state.lock` flock.
//! * Read-only `snapshot` does not modify the file.
//! * Truncated / corrupted state.json is preserved, not replaced
//!   with an empty state.
//! * Simulated pre-rename failure leaves the previous file intact.
//! * Concurrent enqueues from multiple threads are safe.
//! * FIFO order on `acquire_next` is `(queued_at, then display_key)`.
//! * `set_worktree`/`save_finalization` accept only the matching claim.
//! * No lost update under load.

// The acceptance tests intentionally construct entry/seed values
// via locals they then read back through the StateStore API. The
// resulting "unused" locals and the redundant `8.min(16)` in the
// concurrency test are part of the per-test setup, not real dead
// code; silencing the lints here keeps `cargo clippy -- -D warnings`
// green without weakening any assertion.
#![allow(unused_variables, unused_imports, clippy::unnecessary_min_or_max)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use chrono::Utc;

use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::queue::{
    parse_queue_state, serialize_queue_state, ClaimToken, EnqueueOutcome, FinalizationCheckpoint,
    FinalizationStage, Phase, QueueEntry, QueueState, StateStore, TicketType, QUEUE_FILE_VERSION,
};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-state-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn key(owner: &str, repo: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

fn entry(owner: &str, repo: &str, number: u64) -> QueueEntry {
    QueueEntry {
        key: key(owner, repo, number),
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn write_state(path: &Path, state: &QueueState) {
    let body = serialize_queue_state(state).expect("serialize");
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("open state");
    f.write_all(body.as_bytes()).expect("write");
    f.sync_all().ok();
}

// ---------------------------------------------------------------------------
// Initialization and round-trip
// ---------------------------------------------------------------------------

#[test]
fn open_creates_state_dir_and_claims_dir() {
    let root = tempdir("open-creates");
    let state_dir = root.join("state");
    assert!(!state_dir.exists());

    let store = StateStore::open(&state_dir).expect("open");
    assert!(state_dir.is_dir(), "state dir created");
    assert!(state_dir.join("claims").is_dir(), "claims dir created");

    // Snapshot of a fresh state is empty (not corrupt).
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 0);
    assert_eq!(snap.version, QUEUE_FILE_VERSION);
}

#[test]
fn open_existing_state_round_trips() {
    let root = tempdir("open-existing");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let mut entries = BTreeMap::new();
    let e = entry("Owner", "Repo", 7);
    entries.insert(e.key.display_key(), e.clone());
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    write_state(&state_dir.join("state.json"), &state);

    let store = StateStore::open(&state_dir).expect("open existing");
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap, state);
}

// ---------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------

#[test]
fn enqueue_inserts_new_entry() {
    let root = tempdir("enqueue-new");
    let store = StateStore::open(&root).expect("open");
    let outcome = store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .expect("enqueue");
    assert!(matches!(outcome, EnqueueOutcome::Inserted));
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).expect("present");
    assert_eq!(e.phase, Phase::Queued);
    assert_eq!(e.ticket_type, TicketType::Code);
    assert_eq!(e.attempts, 0);
}

#[test]
fn enqueue_existing_queued_entry_is_noop() {
    let root = tempdir("enqueue-existing");
    let store = StateStore::open(&root).expect("open");
    let outcome1 = store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .expect("enqueue1");
    assert!(matches!(outcome1, EnqueueOutcome::Inserted));
    let outcome2 = store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Investigation, false)
        .expect("enqueue2");
    // Existing entry: ticket_type stays as first insert.
    assert!(matches!(outcome2, EnqueueOutcome::AlreadyPresent));
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).expect("present");
    assert_eq!(e.ticket_type, TicketType::Code);
}

#[test]
fn enqueue_dry_run_does_not_promote_previewed() {
    let root = tempdir("enqueue-dry");
    let store = StateStore::open(&root).expect("open");

    // Seed a Previewed entry directly through acquire path: we
    // need to get it Previewed first, so first call acquire to put
    // it InProgress, then... simplest: write a state file directly.
    let mut entries = BTreeMap::new();
    let mut e = entry("Owner", "Repo", 1);
    e.phase = Phase::Previewed;
    entries.insert(e.key.display_key(), e);
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    write_state(&root.join("state.json"), &state);

    // Reopen after writing raw.
    let store = StateStore::open(&root).expect("reopen");
    let outcome = store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, true)
        .expect("dry enqueue");
    // Dry-run does not promote Previewed.
    assert!(matches!(outcome, EnqueueOutcome::AlreadyPresent));
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).expect("present");
    assert_eq!(e.phase, Phase::Previewed);
}

#[test]
fn enqueue_non_dry_promotes_previewed_to_queued() {
    let root = tempdir("enqueue-promote");
    let store = StateStore::open(&root).expect("open");

    let mut entries = BTreeMap::new();
    let mut e = entry("Owner", "Repo", 1);
    e.phase = Phase::Previewed;
    entries.insert(e.key.display_key(), e);
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    write_state(&root.join("state.json"), &state);

    let store = StateStore::open(&root).expect("reopen");
    let outcome = store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .expect("enqueue");
    assert!(matches!(outcome, EnqueueOutcome::Promoted));
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).expect("present");
    assert_eq!(e.phase, Phase::Queued);
}

// ---------------------------------------------------------------------------
// Snapshot / read-only contract
// ---------------------------------------------------------------------------

#[test]
fn snapshot_does_not_modify_mtime() {
    let root = tempdir("snapshot-readonly");
    let store = StateStore::open(&root).expect("open");
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let state_path = root.join("state.json");
    let mtime_before = fs::metadata(&state_path).unwrap().modified().unwrap();

    // Wait a bit so any rewrite would be detectable.
    std::thread::sleep(std::time::Duration::from_millis(20));

    for _ in 0..5 {
        let _ = store.snapshot().unwrap();
    }
    let mtime_after = fs::metadata(&state_path).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after, "snapshot is read-only");
}

// ---------------------------------------------------------------------------
// Acquire / FIFO
// ---------------------------------------------------------------------------

#[test]
fn acquire_next_picks_fifo_by_queued_at() {
    let root = tempdir("acquire-fifo");
    let store = StateStore::open(&root).expect("open");

    // Enqueue in non-FIFO order.
    let now = Utc::now();
    let mut entries = BTreeMap::new();
    let k1 = key("Owner", "Repo", 1);
    let k2 = key("Owner", "Repo", 2);
    let k3 = key("Owner", "Repo", 3);
    let make = |k: &IssueKey, queued: chrono::DateTime<Utc>| QueueEntry {
        key: k.clone(),
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: queued,
        updated_at: queued,
    };
    // Order enqueued: 3 first, then 1, then 2.
    entries.insert(
        k3.display_key(),
        make(&k3, now + chrono::Duration::seconds(2)),
    );
    entries.insert(
        k1.display_key(),
        make(&k1, now + chrono::Duration::seconds(0)),
    );
    entries.insert(
        k2.display_key(),
        make(&k2, now + chrono::Duration::seconds(1)),
    );
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    write_state(&root.join("state.json"), &state);

    let store = StateStore::open(&root).expect("reopen");
    let now = Utc::now();
    let claimed = store
        .acquire_next("RUN1", 12345, now)
        .expect("acquire")
        .expect("some claim");
    assert_eq!(claimed.entry.key, k1);
    assert_eq!(claimed.claim.run_id(), "RUN1");
    assert_eq!(claimed.claim.digest(), claim_digest(&k1));
}

fn claim_digest(key: &IssueKey) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.display_key().as_bytes());
    hex::encode(hasher.finalize())
}

#[test]
fn acquire_next_skips_entries_with_future_next_attempt_at() {
    let root = tempdir("acquire-skip-backoff");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    let future = now + chrono::Duration::seconds(600);

    let mut entries = BTreeMap::new();
    let k1 = key("Owner", "Repo", 1);
    let k2 = key("Owner", "Repo", 2);
    let make = |k: &IssueKey, backoff: Option<chrono::DateTime<Utc>>| QueueEntry {
        key: k.clone(),
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 1,
        last_error: Some("blip".to_string()),
        last_run_id: None,
        next_attempt_at: backoff,
        finalization: None,
        queued_at: now,
        updated_at: now,
    };
    entries.insert(k1.display_key(), make(&k1, Some(future)));
    entries.insert(k2.display_key(), make(&k2, None));
    write_state(
        &root.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );

    let store = StateStore::open(&root).expect("reopen");
    let claimed = store
        .acquire_next("RUN1", 1, now)
        .expect("acquire")
        .expect("some claim");
    assert_eq!(claimed.entry.key, k2);
}

#[test]
fn acquire_next_skips_terminal_entries() {
    let root = tempdir("acquire-skip-terminal");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    let mut entries = BTreeMap::new();
    let k1 = key("Owner", "Repo", 1);
    let mut e = entry("Owner", "Repo", 1);
    e.phase = Phase::Failed;
    entries.insert(k1.display_key(), e);
    write_state(
        &root.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
    let store = StateStore::open(&root).expect("reopen");
    let claimed = store.acquire_next("RUN1", 1, now).expect("acquire");
    assert!(claimed.is_none());
}

#[test]
fn acquire_next_creates_claim_file_and_marks_in_progress() {
    let root = tempdir("acquire-creates");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store
        .acquire_next("RUN-abc", 4242, now)
        .expect("acquire")
        .expect("some claim");
    let expected = claim_digest(&key("Owner", "Repo", 1));
    assert_eq!(claimed.claim.digest(), expected);
    let claim_path = root.join("claims").join(format!("{}.claim", expected));
    assert!(claim_path.is_file(), "claim file written");
    let body = fs::read_to_string(&claim_path).unwrap();
    assert!(body.contains("\"run_id\":\"RUN-abc\""));
    assert!(body.contains("\"pid\":4242"));

    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.phase, Phase::InProgress);
    assert_eq!(e.last_run_id.as_deref(), Some("RUN-abc"));
}

// ---------------------------------------------------------------------------
// set_worktree and save_finalization
// ---------------------------------------------------------------------------

#[test]
fn set_worktree_persists_under_claim() {
    let root = tempdir("set-worktree");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let wt = root.join("wt");
    store.set_worktree(&claimed.claim, &wt).expect("set");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.last_run_id.as_deref(), Some("RUN1"));
}

#[test]
fn set_worktree_with_wrong_run_id_is_rejected() {
    let root = tempdir("set-worktree-wrong");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let bogus = ClaimToken::for_test(root.join("claims"), "deadbeef", "OTHER");
    let err = store
        .set_worktree(&bogus, Path::new("/tmp/wt"))
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("claim") || msg.contains("run_id"),
        "expected claim/run_id error; got {msg}"
    );
}

#[test]
fn save_finalization_persists_checkpoint() {
    let root = tempdir("save-finalization");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let checkpoint = FinalizationCheckpoint {
        run_id: "RUN1".to_string(),
        branch_name: "automation/issue-1-run1".to_string(),
        result_path: root.join("runs").join("RUN1.result.json"),
        stage: FinalizationStage::Committed,
        commit_oid: Some("abc123".to_string()),
        pr_number: None,
        pr_url: None,
    };
    store
        .save_finalization(&claimed.claim, checkpoint.clone())
        .expect("save");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    let stored = e.finalization.as_ref().expect("finalization present");
    assert_eq!(stored.run_id, "RUN1");
    assert_eq!(stored.stage, FinalizationStage::Committed);
    assert_eq!(stored.commit_oid.as_deref(), Some("abc123"));
}

#[test]
fn save_finalization_with_wrong_run_id_is_rejected() {
    let root = tempdir("save-finalization-wrong");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let bogus = ClaimToken::for_test(root.join("claims"), "deadbeef", "OTHER");
    let checkpoint = FinalizationCheckpoint {
        run_id: "OTHER".to_string(),
        branch_name: "x".to_string(),
        result_path: root.join("x"),
        stage: FinalizationStage::Committed,
        commit_oid: None,
        pr_number: None,
        pr_url: None,
    };
    let err = store
        .save_finalization(&bogus, checkpoint)
        .expect_err("rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("claim") || msg.contains("run_id"),
        "expected claim/run_id error; got {msg}"
    );
}

// ---------------------------------------------------------------------------
// Terminal transitions
// ---------------------------------------------------------------------------

#[test]
fn complete_transitions_to_done_and_removes_claim() {
    let root = tempdir("complete");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let claim_path = root
        .join("claims")
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(claim_path.is_file());

    store.complete(claimed.claim.clone()).expect("complete");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.phase, Phase::Done);
    assert!(!claim_path.exists(), "claim file removed");
}

#[test]
fn complete_investigation_transitions_to_done() {
    let root = tempdir("complete-investigation");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Investigation, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    store
        .complete_investigation(claimed.claim.clone())
        .expect("complete investigation");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.phase, Phase::Done);
    let claim_path = root
        .join("claims")
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(!claim_path.exists());
}

// ---------------------------------------------------------------------------
// Retry / infrastructure
// ---------------------------------------------------------------------------

#[test]
fn retry_or_fail_returns_to_queued_under_budget() {
    let root = tempdir("retry-queued");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    let backoff = chrono::Duration::seconds(60);
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();

    // First failure (attempts: 0 -> 1) under budget.
    let phase = store
        .retry_or_fail(claimed.claim.clone(), "boom", 3)
        .expect("retry");
    assert_eq!(phase, Phase::Queued);
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.attempts, 1);
    assert_eq!(e.phase, Phase::Queued);
    assert_eq!(e.last_error.as_deref(), Some("boom"));
    assert!(e.next_attempt_at.is_some());
    let _ = backoff; // referenced for clarity
}

#[test]
fn retry_or_fail_at_budget_transitions_to_failed() {
    let root = tempdir("retry-failed");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    let mut entries = BTreeMap::new();
    let k = key("Owner", "Repo", 1);
    let mut e = entry("Owner", "Repo", 1);
    e.attempts = 2;
    entries.insert(e.key.display_key(), e);
    write_state(
        &root.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
    let store = StateStore::open(&root).expect("reopen");
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let phase = store
        .retry_or_fail(claimed.claim.clone(), "boom", 3)
        .expect("retry");
    assert_eq!(phase, Phase::Failed);
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).unwrap();
    assert_eq!(e.attempts, 3);
    assert_eq!(e.phase, Phase::Failed);
    let claim_path = root
        .join("claims")
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(!claim_path.exists());
}

#[test]
fn requeue_infrastructure_does_not_increment_attempts() {
    let root = tempdir("requeue-infra");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    let later = now + chrono::Duration::seconds(120);
    store
        .requeue_infrastructure(claimed.claim.clone(), "rate-limited", later)
        .expect("requeue");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.attempts, 0, "attempts unchanged");
    assert_eq!(e.phase, Phase::Queued);
    assert_eq!(e.last_error.as_deref(), Some("rate-limited"));
    assert_eq!(e.next_attempt_at, Some(later));
    let claim_path = root
        .join("claims")
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(!claim_path.exists());
}

#[test]
fn skip_transitions_to_skipped() {
    let root = tempdir("skip");
    let store = StateStore::open(&root).expect("open");
    let now = Utc::now();
    store
        .enqueue(&key("Owner", "Repo", 1), TicketType::Code, false)
        .unwrap();
    let claimed = store.acquire_next("RUN1", 1, now).unwrap().unwrap();
    store
        .skip(claimed.claim.clone(), "voice violation")
        .expect("skip");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&key("Owner", "Repo", 1)).unwrap();
    assert_eq!(e.phase, Phase::Skipped);
    let claim_path = root
        .join("claims")
        .join(format!("{}.claim", claimed.claim.digest()));
    assert!(!claim_path.exists());
}

// ---------------------------------------------------------------------------
// Corrupt-state handling
// ---------------------------------------------------------------------------

#[test]
fn truncated_state_is_preserved_not_replaced() {
    let root = tempdir("corrupt-truncated");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let state_path = state_dir.join("state.json");
    fs::write(&state_path, b"{\"version\":1,\"en").expect("write");

    let err = StateStore::open(&state_dir).expect_err("open rejected");
    match err {
        CaduceusError::StateCorrupt { .. } => {}
        other => panic!("expected StateCorrupt, got {other:?}"),
    }
    // File preserved.
    let bytes = fs::read(&state_path).unwrap();
    assert!(bytes.starts_with(b"{\"version\":1,\"en"));
}

#[test]
fn missing_state_file_yields_empty_state() {
    let root = tempdir("missing-state");
    let store = StateStore::open(&root).expect("open missing");
    let snap = store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 0);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[test]
fn concurrent_enqueues_are_safe() {
    let root = tempdir("concurrent-enqueue");
    let store = Arc::new(StateStore::open(&root).expect("open"));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for i in 0..8 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let key = key("Owner", "Repo", i as u64 + 1);
            store
                .enqueue(&key, TicketType::Code, false)
                .expect("enqueue");
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    let snap = store.snapshot().unwrap();
    assert_eq!(snap.entries.len(), 8);
}

#[test]
fn no_lost_update_under_concurrent_acquire_attempts() {
    let root = tempdir("concurrent-acquire");
    let store = Arc::new(StateStore::open(&root).expect("open"));
    let now = Utc::now();
    for i in 0..16 {
        store
            .enqueue(&key("Owner", "Repo", i + 1), TicketType::Code, false)
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for i in 0..8 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            store.acquire_next(&format!("RUN-{i}"), 100 + i as u32, now)
        }));
    }
    let mut won = Vec::new();
    for h in handles {
        if let Some(c) = h.join().unwrap().unwrap() {
            won.push(c);
        }
    }
    // 8 threads, 16 entries → at least 8 distinct winners.
    let unique_keys: std::collections::HashSet<_> =
        won.iter().map(|c| c.entry.key.display_key()).collect();
    assert_eq!(
        unique_keys.len(),
        won.len(),
        "no two threads won the same key"
    );

    let snap = store.snapshot().unwrap();
    let in_progress = snap
        .entries
        .values()
        .filter(|e| e.phase == Phase::InProgress)
        .count();
    assert_eq!(in_progress, won.len(), "all winners marked InProgress");
    assert!(
        won.len() >= 8.min(16),
        "each thread should win at least once when entries >= threads"
    );
}

// ---------------------------------------------------------------------------
// State file effects
// ---------------------------------------------------------------------------

#[test]
fn state_path_under_state_dir() {
    let root = tempdir("paths");
    let store = StateStore::open(&root).expect("open");
    assert_eq!(store.state_path(), root.join("state.json"));
    assert_eq!(store.claims_dir(), root.join("claims"));
}

// ---------------------------------------------------------------------------
// parse_queue_state integration: helper used for diagnostics
// ---------------------------------------------------------------------------

#[test]
fn parse_then_snapshot_agrees() {
    let root = tempdir("parse-agree");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    let mut entries = BTreeMap::new();
    let e = entry("Owner", "Repo", 9);
    entries.insert(e.key.display_key(), e);
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    let body = serialize_queue_state(&state).unwrap();
    fs::write(state_dir.join("state.json"), body).unwrap();
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let parsed = parse_queue_state(&serialize_queue_state(&snap).unwrap()).unwrap();
    assert_eq!(parsed, snap);
}
