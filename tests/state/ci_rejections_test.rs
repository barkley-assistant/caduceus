//! CI-rejection store round-trip (issue #435).

use caduceus::review::RepositoryId;
use caduceus::state::review::{
    parse_review_ci_rejections, serialize_review_ci_rejections, ReviewStore,
    MAX_CI_REJECTIONS_PER_PR, REVIEW_CI_REJECTIONS_FILE_VERSION,
};
use caduceus::store::{open, SCHEMA_VERSION};
use rusqlite::Connection;
use tempfile::tempdir;

fn repo() -> RepositoryId {
    RepositoryId {
        owner: "Owner".to_string(),
        repo: "R".to_string(),
    }
}

#[test]
fn json_round_trip_and_idempotent_insert() {
    let dir = tempdir().expect("tempdir");
    let store = ReviewStore::open(dir.path()).expect("open");
    let r = repo();
    store.record_ci_rejection(&r, 7, "abc123").expect("record");
    store
        .record_ci_rejection(&r, 7, "abc123")
        .expect("idempotent");
    let shas = store.ci_rejected_head_shas(&r, 7).expect("read");
    assert_eq!(shas, vec!["abc123".to_string()]);

    let path = dir.path().join("review_ci_rejections.json");
    let body = std::fs::read_to_string(&path).expect("file written");
    let parsed = parse_review_ci_rejections(&body).expect("parse");
    assert_eq!(parsed.version, REVIEW_CI_REJECTIONS_FILE_VERSION);
    assert_eq!(parsed.rejections.len(), 1);
    assert_eq!(parsed.rejections[0].repository.owner, "owner");
    assert_eq!(parsed.rejections[0].repository.repo, "r");
    let again = serialize_review_ci_rejections(&parsed).expect("serialize");
    let reparsed = parse_review_ci_rejections(&again).expect("reparse");
    assert_eq!(reparsed, parsed);
}

#[test]
fn missing_file_is_empty() {
    let dir = tempdir().expect("tempdir");
    let store = ReviewStore::open(dir.path()).expect("open");
    let shas = store.ci_rejected_head_shas(&repo(), 1).expect("read");
    assert!(shas.is_empty());
}

#[test]
fn sqlite_round_trip() {
    let dir = tempdir().expect("tempdir");
    let store = ReviewStore::open_sqlite(dir.path()).expect("open sqlite");
    let r = repo();
    store
        .record_ci_rejection(&r, 3, "deadbeef")
        .expect("record");
    let shas = store.ci_rejected_head_shas(&r, 3).expect("read");
    assert_eq!(shas, vec!["deadbeef".to_string()]);
    // Case-normalised owner/repo on read.
    let other = RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    };
    let shas2 = store.ci_rejected_head_shas(&other, 3).expect("read");
    assert_eq!(shas2, vec!["deadbeef".to_string()]);
}

#[test]
fn per_pr_cap_drops_oldest_json() {
    let dir = tempdir().expect("tempdir");
    let store = ReviewStore::open(dir.path()).expect("open");
    let r = repo();
    let other = RepositoryId {
        owner: "other".to_string(),
        repo: "x".to_string(),
    };
    // Unrelated PR must not be pruned by this PR's cap.
    store
        .record_ci_rejection(&other, 1, "keepme")
        .expect("other");
    for i in 0..(MAX_CI_REJECTIONS_PER_PR + 3) {
        let sha = format!("sha{i:04x}");
        store.record_ci_rejection(&r, 7, &sha).expect("record");
    }
    let shas = store.ci_rejected_head_shas(&r, 7).expect("read");
    assert_eq!(shas.len(), MAX_CI_REJECTIONS_PER_PR);
    assert!(!shas.iter().any(|s| s == "sha0000"));
    assert!(shas.iter().any(|s| s == "sha0022")); // 0..35 → keep 3..34
    let other_shas = store.ci_rejected_head_shas(&other, 1).expect("other read");
    assert_eq!(other_shas, vec!["keepme".to_string()]);
}

#[test]
fn sqlite_keyed_read_isolates_prs() {
    let dir = tempdir().expect("tempdir");
    let store = ReviewStore::open_sqlite(dir.path()).expect("open sqlite");
    let r = repo();
    let other = RepositoryId {
        owner: "other".to_string(),
        repo: "x".to_string(),
    };
    store.record_ci_rejection(&r, 7, "aaa").expect("a");
    store.record_ci_rejection(&r, 8, "bbb").expect("b");
    store.record_ci_rejection(&other, 7, "ccc").expect("c");
    assert_eq!(
        store.ci_rejected_head_shas(&r, 7).expect("r7"),
        vec!["aaa".to_string()]
    );
    assert_eq!(
        store.ci_rejected_head_shas(&r, 8).expect("r8"),
        vec!["bbb".to_string()]
    );
}

#[test]
fn v9_store_migrates_to_schema_10_with_table() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("state.db");
    {
        let conn = Connection::open(&db_path).expect("create");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE schema_version (version INTEGER NOT NULL, migrated_at TEXT NOT NULL);
             INSERT INTO schema_version (version, migrated_at) VALUES (9, '2026-01-01T00:00:00Z');
             CREATE TABLE review_queue_entries (
                review_key TEXT PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
                pull_request INTEGER NOT NULL, head_sha TEXT NOT NULL, base_sha TEXT NOT NULL,
                base_ref TEXT NOT NULL, merge_base TEXT NOT NULL, phase TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_run_id TEXT,
                next_attempt_at TEXT, queued_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                review_generation INTEGER NOT NULL DEFAULT 1,
                blocked_source TEXT, blocked_recovery_hint TEXT
             );
             CREATE TABLE review_state (
                owner TEXT NOT NULL, repo TEXT NOT NULL, pull_request INTEGER NOT NULL,
                last_reviewed_head_sha TEXT, last_verdict TEXT, last_reviewed_at TEXT,
                sticky_comment_id INTEGER, last_run_id TEXT,
                review_generation INTEGER NOT NULL DEFAULT 1,
                publication_state TEXT NOT NULL DEFAULT 'pending',
                publication_attempt_count INTEGER NOT NULL DEFAULT 0,
                next_publish_at TEXT, last_publish_error TEXT,
                PRIMARY KEY (owner, repo, pull_request)
             );
             CREATE TABLE review_history (
                review_run_id TEXT PRIMARY KEY, owner TEXT NOT NULL, repo TEXT NOT NULL,
                pull_request INTEGER NOT NULL, head_sha TEXT NOT NULL,
                review_generation INTEGER NOT NULL, completed_at TEXT NOT NULL,
                result_json TEXT NOT NULL
             );",
        )
        .expect("seed v9");
    }
    let _ = open(&db_path).expect("open migrates");
    let conn = Connection::open(&db_path).expect("reopen");
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, SCHEMA_VERSION);
    let table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='review_ci_rejections'",
            [],
            |r| r.get(0),
        )
        .expect("probe");
    assert_eq!(table, 1);
}
