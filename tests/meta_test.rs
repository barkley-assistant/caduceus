//! Task 7.2 acceptance tests for daemon metadata persistence.
//!
//! The tests exercise the canonical `StateMeta` envelope, the
//! `MetaStore` read-modify-write store, the corrupt-file
//! quarantine path, the rate-limit observer merge semantics, and
//! diagnostic coalescing.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use chrono::{Duration, TimeZone, Utc};

use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::meta::{
    append_diagnostic, load, save, MetaStore, RateLimitObservation, RateLimitObserver, StateMeta,
    TickOutcome, META_VERSION,
};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-meta-test-{label}-{nonce}"));
    fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn sample_key() -> IssueKey {
    IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 7,
    }
}

fn sample_meta() -> StateMeta {
    StateMeta {
        version: META_VERSION,
        last_tick_started: Some(Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 0).unwrap()),
        last_tick_finished: Some(Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 1).unwrap()),
        last_outcome: Some(TickOutcome::Processed),
        last_http_status: Some(200),
        next_allowed_poll_at: Some(Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 30).unwrap()),
        last_reap_at: None,
        last_reaped_count: 0,
        rate_limit: None,
        last_error: None,
        recent_diagnostics: Vec::new(),
    }
}

fn write_raw(path: &PathBuf, body: &[u8]) {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("create file");
    f.write_all(body).expect("write");
    f.sync_all().ok();
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[test]
fn empty_meta_round_trips() {
    let root = tempdir("empty");
    let meta = StateMeta::empty();
    save(&root, &meta).expect("save");
    let loaded = load(&root).expect("load");
    assert_eq!(loaded, meta);
}

#[test]
fn full_meta_round_trips() {
    let root = tempdir("full");
    let meta = sample_meta();
    save(&root, &meta).expect("save");
    let loaded = load(&root).expect("load");
    assert_eq!(loaded, meta);
}

#[test]
fn minimal_meta_round_trips() {
    let root = tempdir("minimal");
    let meta = StateMeta {
        version: META_VERSION,
        last_tick_started: None,
        last_tick_finished: None,
        last_outcome: None,
        last_http_status: None,
        next_allowed_poll_at: None,
        last_reap_at: None,
        last_reaped_count: 0,
        rate_limit: None,
        last_error: None,
        recent_diagnostics: Vec::new(),
    };
    save(&root, &meta).expect("save");
    let loaded = load(&root).expect("load");
    assert_eq!(loaded, meta);
}

// ---------------------------------------------------------------------------
// Atomic write and corrupt handling
// ---------------------------------------------------------------------------

#[test]
fn load_returns_empty_when_no_file_exists() {
    let root = tempdir("no-file");
    let loaded = load(&root).expect("empty load");
    assert_eq!(loaded, StateMeta::empty());
}

#[test]
fn corrupt_file_is_preserved_and_a_marker_is_written() {
    let root = tempdir("corrupt");
    let meta_path = root.join("state_meta.json");
    write_raw(&meta_path, b"this is not JSON");
    let err = load(&root).expect_err("corrupt");
    match err {
        CaduceusError::StateCorrupt { path, message } => {
            assert!(path.ends_with("state_meta.json"));
            assert!(message.contains("parse state_meta"), "got: {message}");
        }
        other => panic!("expected StateCorrupt; got: {other:?}"),
    }
    // Original is preserved (the corrupt file is *not* deleted).
    assert!(meta_path.exists(), "original file must be preserved");
    // A marker is written.
    let marker = root.join("state_meta.corrupt");
    assert!(marker.exists(), "corrupt marker must be written");
    let body = fs::read_to_string(&marker).expect("read marker");
    assert!(body.contains("state_meta quarantine"), "got: {body}");
    // A timestamped backup is created.
    let backups: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("state_meta.json.corrupt-")
        })
        .collect();
    assert!(!backups.is_empty(), "no backup was created");
}

#[test]
fn unsupported_version_is_rejected() {
    let root = tempdir("bad-version");
    let meta_path = root.join("state_meta.json");
    write_raw(&meta_path, br#"{"version":999,"last_tick_started":null,"last_tick_finished":null,"last_outcome":null,"last_http_status":null,"next_allowed_poll_at":null,"last_reap_at":null,"last_reaped_count":0,"rate_limit":null,"last_error":null,"recent_diagnostics":[]}"#);
    let err = load(&root).expect_err("bad version");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
    assert!(msg.contains("unsupported metadata version"), "got: {msg}");
}

#[test]
fn unknown_field_is_rejected() {
    let root = tempdir("unknown-field");
    let meta_path = root.join("state_meta.json");
    write_raw(
        &meta_path,
        br#"{"version":1,"last_tick_started":null,"last_tick_finished":null,"last_outcome":null,"last_http_status":null,"next_allowed_poll_at":null,"last_reap_at":null,"last_reaped_count":0,"rate_limit":null,"last_error":null,"recent_diagnostics":[],"rogue":"x"}"#,
    );
    let err = load(&root).expect_err("deny_unknown_fields");
    let msg = format!("{err:?}");
    assert!(msg.contains("StateCorrupt"), "got: {msg}");
}

#[test]
fn atomic_write_failure_leaves_no_partial_file() {
    // Save to a directory whose parent is a file — the rename will
    // fail and the temp file must be cleaned up.
    let root = tempdir("atomic-fail");
    let blocker = root.join("blocker");
    write_raw(&blocker, b"not a dir");
    let err = save(&blocker, &sample_meta()).expect_err("rename fails");
    let msg = format!("{err:?}");
    assert!(msg.contains("Io") || msg.contains("Os"), "got: {msg}");
    // No .tmp file leaked into the parent.
    let parent_entries: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !parent_entries.iter().any(|n| n.ends_with(".tmp")),
        "tmp file leaked: {parent_entries:?}"
    );
}

// ---------------------------------------------------------------------------
// MetaStore: concurrent updates
// ---------------------------------------------------------------------------

#[test]
fn meta_store_update_serializes_through_mutex() {
    let root = tempdir("mutex");
    let store = MetaStore::open(&root).expect("open");
    store
        .update(|meta| {
            meta.last_tick_started = Some(Utc::now());
            meta.last_outcome = Some(TickOutcome::Processed);
            meta.last_reaped_count = 1;
        })
        .expect("update");

    let loaded = load(&root).expect("load");
    assert_eq!(loaded.last_outcome, Some(TickOutcome::Processed));
    assert_eq!(loaded.last_reaped_count, 1);
}

#[test]
fn concurrent_observer_updates_do_not_lose_fields() {
    let root = tempdir("concurrent");
    let store = Arc::new(MetaStore::open(&root).expect("open"));

    let mut handles = Vec::new();
    for i in 0..8 {
        let s = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            let observer = RateLimitObserver::new(&s);
            let reset_at = Utc::now() + Duration::seconds(60 + i);
            observer
                .observe(RateLimitObservation {
                    limit: Some(60),
                    remaining: 50 - i as u32,
                    reset_at,
                    observed_at: Utc::now(),
                })
                .expect("observe");
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    let snap = store.snapshot();
    let rl = snap.rate_limit.expect("rate limit recorded");
    // The persisted observation must come from one of the threads.
    assert!(
        (0..8)
            .any(|i| rl.reset_at == Utc::now() + Duration::seconds(60 + i) - Duration::seconds(0))
            || (0..8).any(|i| (rl.reset_at - (Utc::now() + Duration::seconds(60 + i)))
                .num_seconds()
                .abs()
                < 2)
    );
    // Concurrent updates to *different* fields must not lose them.
    store
        .update(|meta| meta.last_outcome = Some(TickOutcome::Idle))
        .expect("update outcome");
    store
        .update(|meta| meta.last_reaped_count = 17)
        .expect("update reap");
    let snap = store.snapshot();
    assert_eq!(snap.last_outcome, Some(TickOutcome::Idle));
    assert_eq!(snap.last_reaped_count, 17);
    assert!(snap.rate_limit.is_some(), "rate limit preserved");
}

#[test]
fn stale_observation_cannot_overwrite_a_newer_one() {
    let root = tempdir("stale");
    let store = MetaStore::open(&root).expect("open");
    let observer = RateLimitObserver::new(&store);

    let new_reset = Utc::now() + Duration::seconds(120);
    observer
        .observe(RateLimitObservation {
            limit: Some(60),
            remaining: 42,
            reset_at: new_reset,
            observed_at: Utc::now(),
        })
        .expect("observe new");

    // Stale: reset_at is earlier than the persisted one.
    let stale_reset = new_reset - Duration::seconds(30);
    observer
        .observe(RateLimitObservation {
            limit: Some(60),
            remaining: 1,
            reset_at: stale_reset,
            observed_at: Utc::now(),
        })
        .expect("observe stale");

    let snap = store.snapshot();
    let rl = snap.rate_limit.expect("rate limit");
    assert_eq!(rl.reset_at, new_reset);
    assert_eq!(rl.remaining, 42);
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[test]
fn append_diagnostic_adds_new_entry() {
    let mut meta = StateMeta::empty();
    append_diagnostic(
        &mut meta,
        "POLL_AMBIGUITY",
        Some(sample_key()),
        "issue appears in two result sets",
    );
    assert_eq!(meta.recent_diagnostics.len(), 1);
    assert_eq!(meta.recent_diagnostics[0].code, "POLL_AMBIGUITY");
    assert_eq!(meta.recent_diagnostics[0].issue_key, Some(sample_key()));
}

#[test]
fn append_diagnostic_coalesces_duplicate_within_one_hour() {
    let mut meta = StateMeta::empty();
    append_diagnostic(&mut meta, "POLL_AMBIGUITY", Some(sample_key()), "first");
    append_diagnostic(&mut meta, "POLL_AMBIGUITY", Some(sample_key()), "second");
    append_diagnostic(&mut meta, "POLL_AMBIGUITY", Some(sample_key()), "third");
    assert_eq!(
        meta.recent_diagnostics.len(),
        1,
        "duplicate (code, issue_key) within one hour must coalesce"
    );
    assert_eq!(meta.recent_diagnostics[0].message, "third");
}

#[test]
fn append_diagnostic_does_not_coalesce_different_codes() {
    let mut meta = StateMeta::empty();
    append_diagnostic(&mut meta, "POLL_AMBIGUITY", Some(sample_key()), "ambiguity");
    append_diagnostic(&mut meta, "CACHE_RECOVERY", Some(sample_key()), "cache");
    assert_eq!(meta.recent_diagnostics.len(), 2);
}

#[test]
fn append_diagnostic_does_not_coalesce_different_issue_keys() {
    let mut meta = StateMeta::empty();
    append_diagnostic(&mut meta, "CODE", Some(sample_key()), "issue 7");
    let mut other = sample_key();
    other.number = 8;
    append_diagnostic(&mut meta, "CODE", Some(other), "issue 8");
    assert_eq!(meta.recent_diagnostics.len(), 2);
}

#[test]
fn append_diagnostic_caps_at_twenty_entries() {
    let mut meta = StateMeta::empty();
    for i in 0..30 {
        let mut k = sample_key();
        k.number = i;
        append_diagnostic(&mut meta, format!("CODE-{i}"), Some(k), "msg");
    }
    assert_eq!(meta.recent_diagnostics.len(), 20);
    // The oldest entries (CODE-0..CODE-9) are dropped; the newest 20 remain.
    let first = &meta.recent_diagnostics[0];
    assert_eq!(first.code, "CODE-10");
    let last = meta.recent_diagnostics.last().unwrap();
    assert_eq!(last.code, "CODE-29");
}

#[test]
fn append_diagnostic_truncates_overlong_messages() {
    let mut meta = StateMeta::empty();
    let long = "a".repeat(2000);
    append_diagnostic(&mut meta, "CODE", None, long);
    let msg = &meta.recent_diagnostics[0].message;
    assert!(msg.len() <= 256);
    assert!(msg.starts_with("aaa"));
}

// ---------------------------------------------------------------------------
// MetaStore corrupt marker
// ---------------------------------------------------------------------------

#[test]
fn meta_store_open_preserves_corrupt_marker_state() {
    let root = tempdir("meta-corrupt-marker");
    let store = MetaStore::open(&root).expect("open");
    assert!(!store.is_corrupt());
    fs::write(store.corrupt_marker_path(), "marker").unwrap();
    assert!(store.is_corrupt());
    store.clear_corrupt_marker().expect("clear");
    assert!(!store.is_corrupt());
}

// ---------------------------------------------------------------------------
// RateLimitObservation helpers
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_observation_is_newer_than_helper() {
    let now = Utc::now();
    let newer = RateLimitObservation {
        limit: Some(60),
        remaining: 1,
        reset_at: now + Duration::seconds(60),
        observed_at: now,
    };
    let older = RateLimitObservation {
        limit: Some(60),
        remaining: 5,
        reset_at: now,
        observed_at: now,
    };
    assert!(newer.is_newer_than(&older));
    assert!(!older.is_newer_than(&newer));
}

// ---------------------------------------------------------------------------
// Tick outcome serialization
// ---------------------------------------------------------------------------

#[test]
fn tick_outcome_serializes_as_snake_case() {
    let json = serde_json::to_string(&TickOutcome::Concurrent).unwrap();
    assert_eq!(json, "\"concurrent\"");
}

// ---------------------------------------------------------------------------
// Timestamp serialization
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_observation_timestamp_serializes_as_rfc3339_utc() {
    let obs = RateLimitObservation {
        limit: Some(60),
        remaining: 0,
        reset_at: Utc.with_ymd_and_hms(2026, 7, 13, 14, 0, 0).unwrap(),
        observed_at: Utc.with_ymd_and_hms(2026, 7, 13, 13, 59, 30).unwrap(),
    };
    let json = serde_json::to_string(&obs).unwrap();
    assert!(json.contains("2026-07-13T14:00:00"), "got: {json}");
    assert!(json.contains("Z"), "got: {json}");
}
