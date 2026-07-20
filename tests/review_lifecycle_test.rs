//! Task 4.3 integration suite — proves human review lifecycle.
//!
//! Acceptance checks:
//!
//! - **4.3-AC-01** — `complete_awaiting_review` transitions an entry
//!   from `InProgress` to `AwaitingReview` phase.
//! - **4.3-AC-02** — `refuse_auto_merge` returns an error with the
//!   documented contract message.
//! - **4.3-AC-03** — `route_to_needs_attention` transitions an
//!   `AwaitingReview` entry to `NeedsAttention`.
//! - **4.3-AC-04** — `reprocess_entry` refuses to operate on an
//!   `AwaitingReview` entry.
//! - **4.3-AC-05** — `AwaitingReview` phase round-trips through
//!   serialization correctly.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use caduceus::state::queue::{
    parse_queue_state, serialize_queue_state, Phase, QueueEntry, QueueState, StateStore,
    TicketType, QUEUE_FILE_VERSION,
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
        "review-lifecycle-{label}-{}-{}",
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
// 4.3-AC-01: complete_awaiting_review transitions to AwaitingReview
// ---------------------------------------------------------------------------

#[test]
fn complete_awaiting_review_sets_awaiting_review_phase() {
    let dir = scratch_dir("ac01");
    let store = make_store(&dir);
    seed_entry(&store, Phase::InProgress);

    let key = sample_key();
    store
        .complete_awaiting_review(&key)
        .expect("complete_awaiting_review should succeed");

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry exists");
    assert_eq!(
        entry.phase,
        Phase::AwaitingReview,
        "entry should transition to AwaitingReview"
    );
}

// ---------------------------------------------------------------------------
// 4.3-AC-02: refuse_auto_merge returns error with contract message
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

// ---------------------------------------------------------------------------
// 4.3-AC-03: route_to_needs_attention transitions AwaitingReview to NeedsAttention
// ---------------------------------------------------------------------------

#[test]
fn route_to_needs_attention_from_awaiting_review() {
    let dir = scratch_dir("ac03");
    let store = make_store(&dir);
    seed_entry(&store, Phase::AwaitingReview);

    let key = sample_key();
    store
        .route_to_needs_attention(&key, "PR was closed without merge — operator must inspect")
        .expect("route_to_needs_attention should succeed");

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry exists");
    assert_eq!(
        entry.phase,
        Phase::NeedsAttention,
        "entry should transition to NeedsAttention"
    );
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("closed without merge"),
        "last_error should describe the reason"
    );
}

// ---------------------------------------------------------------------------
// 4.3-AC-04: reprocess_entry refuses AwaitingReview entries
// ---------------------------------------------------------------------------

#[test]
fn reprocess_entry_refuses_awaiting_review() {
    let dir = scratch_dir("ac04");
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
// 4.3-AC-05: AwaitingReview phase round-trips through serialization
// ---------------------------------------------------------------------------

#[test]
fn awaiting_review_phase_serializes_and_deserializes() {
    let entry = sample_entry(Phase::AwaitingReview);
    let mut entries = BTreeMap::new();
    entries.insert(entry.key.display_key(), entry);
    let state = QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    };

    let json = serialize_queue_state(&state).expect("serialize");
    let parsed = parse_queue_state(&json).expect("deserialize");

    let key = sample_key();
    let parsed_entry = parsed.entry(&key).expect("entry exists");
    assert_eq!(
        parsed_entry.phase,
        Phase::AwaitingReview,
        "AwaitingReview phase should survive round-trip"
    );
}
