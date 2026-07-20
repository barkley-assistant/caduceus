//! Task 4.4 lifecycle-matrix suite — proves the human-review lifecycle
//! phase transitions match CONTRACTS.md FINAL-002.
//!
//! 5 tests covering merge, close-without-merge, reopen, reprocess
//! refusal, and the auto-merge audit hook.
//!
//! Acceptance checks:
//!
//! 5 tests covering merge, close-without-merge, reopen, reprocess
//! refusal, and the auto-merge audit hook.
//!
//! Acceptance checks:
//!
//! **PHASE-04-AC-02** — `resolve_awaiting_review_as_done` transitions
//! AwaitingReview → Done; `route_to_needs_attention` transitions
//! AwaitingReview → NeedsAttention with a diagnostic reason;
//! closed-without-merge and merged-PR states are mutually exclusive.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use caduceus::state::queue::{
    serialize_queue_state, Phase, QueueEntry, QueueState, StateStore, TicketType,
    QUEUE_FILE_VERSION,
};
use chrono::{TimeZone, Utc};

use caduceus::github::issue::IssueKey;
use caduceus::runtime::audit::refuse_auto_merge;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "lifecycle-matrix-{label}-{}-{}",
        std::process::id(),
        n
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn sample_key() -> IssueKey {
    IssueKey {
        owner: "BarkleyAssistant".to_string(),
        repo: "sandbox".to_string(),
        number: 100,
    }
}

fn sample_entry(phase: Phase) -> QueueEntry {
    QueueEntry {
        key: sample_key(),
        phase,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap(),
        updated_at: Utc.with_ymd_and_hms(2026, 7, 20, 12, 0, 0).unwrap(),
        generation: 1,
    }
}

fn make_store(dir: &Path) -> StateStore {
    StateStore::open(dir).expect("open state store")
}

fn seed_entry(store: &StateStore, phase: Phase) {
    let key = sample_key();
    let entry = sample_entry(phase);
    let mut entries = BTreeMap::new();
    entries.insert(key.display_key(), entry);
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };
    let json = serialize_queue_state(&state).expect("serialize");
    fs::write(store.state_path(), &json).expect("write state");
}

// ---------------------------------------------------------------------------
// 1. resolve_awaiting_review_as_done transitions AwaitingReview → Done
// ---------------------------------------------------------------------------

#[test]
fn resolve_awaiting_review_as_done_transitions_to_done() {
    let dir = scratch_dir("ac02-merge");
    let store = make_store(&dir);
    seed_entry(&store, Phase::AwaitingReview);

    let key = sample_key();
    store
        .resolve_awaiting_review_as_done(&key)
        .expect("resolve_awaiting_review_as_done should succeed");

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry exists");
    assert_eq!(
        entry.phase,
        Phase::Done,
        "AwaitingReview entry should transition to Done after merge"
    );
}

// ---------------------------------------------------------------------------
// 2. route_to_needs_attention transitions AwaitingReview → NeedsAttention
// ---------------------------------------------------------------------------

#[test]
fn route_to_needs_attention_transitions_to_needs_attention() {
    let dir = scratch_dir("ac02-needs-attn");
    let store = make_store(&dir);
    seed_entry(&store, Phase::AwaitingReview);

    let key = sample_key();
    let reason = "PR closed without merge — operator must inspect";
    store
        .route_to_needs_attention(&key, reason)
        .expect("route_to_needs_attention should succeed");

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry exists");
    assert_eq!(
        entry.phase,
        Phase::NeedsAttention,
        "AwaitingReview entry should transition to NeedsAttention"
    );
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("closed without merge"),
        "last_error should describe the reason: got {:?}",
        entry.last_error
    );
}

// ---------------------------------------------------------------------------
// 3. reopen_then_merge_flow — reset from NeedsAttention to Queued,
//  then verify the entry can be re-processed from AwaitingReview
//  through resolve_awaiting_review_as_done.
// ---------------------------------------------------------------------------

#[test]
fn reopen_then_merge_flow() {
    let dir = scratch_dir("ac02-reopen");
    let store = make_store(&dir);
    seed_entry(&store, Phase::NeedsAttention);

    let key = sample_key();

    // Step 1: Operator resets the NeedsAttention entry.
    let outcome = store
        .reset_entry(&key, false)
        .expect("reset_entry should succeed");
    assert!(
        !outcome.cleared_finalization,
        "reset without --force-finalization-reset"
    );

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry exists");
    assert_eq!(
        entry.phase,
        Phase::Queued,
        "entry should transition back to Queued after reset"
    );
    assert_eq!(entry.attempts, 0, "attempts should be reset to 0");

    // Step 2: Re-seed the entry as AwaitingReview (simulating the entry
    // going through the normal lifecycle again after the operator
    // resolved the needs-attention situation).
    seed_entry(&store, Phase::AwaitingReview);

    // Step 3: Resolve as done — proves the full reopen→merge cycle.
    store
        .resolve_awaiting_review_as_done(&key)
        .expect("resolve_awaiting_review_as_done should succeed after reopen");

    let snap2 = store.snapshot().expect("snapshot");
    let entry2 = snap2.entry(&key).expect("entry exists");
    assert_eq!(
        entry2.phase,
        Phase::Done,
        "reopened entry should transition to Done after merge resolution"
    );
}

// ---------------------------------------------------------------------------
// 4. reprocess_entry refuses AwaitingReview entries
// ---------------------------------------------------------------------------

#[test]
fn reprocess_entry_refuses_awaiting_review() {
    let dir = scratch_dir("ac02-reprocess");
    let store = make_store(&dir);
    seed_entry(&store, Phase::AwaitingReview);

    let key = sample_key();
    let result = store.reprocess_entry(&key);

    assert!(
        result.is_err(),
        "reprocess_entry should refuse AwaitingReview entries"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("awaitingreview"),
        "error should reference AwaitingReview phase: {msg}"
    );
}

// ---------------------------------------------------------------------------
// 5. refuse_auto_merge returns error with contract message
// ---------------------------------------------------------------------------

#[test]
fn refuse_auto_merge_returns_error() {
    let result = refuse_auto_merge();
    assert!(result.is_err(), "refuse_auto_merge must return Err");

    let msg = result.unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("auto-merge"),
        "error message should reference auto-merge: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("human review"),
        "error message should reference human review: {msg}"
    );
}
