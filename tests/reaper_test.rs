//! Task 3.3 acceptance tests for the reaper.
//!
//! The contract is in `CONTRACTS.md` and the task packet
//! `planning/caduceus-v0.1/tasks/3.3-reap-stale-claims-and-abandoned-worktrees.md`.
//!
//! The tests in this file exercise the reaper end-to-end
//! against the real on-disk queue. Each test creates a fresh
//! scratch state dir, enqueues an entry, writes a claim file
//! by hand, and then runs `reap_stale_claims` against a
//! controlled `now`. No GitHub, no real git, no real
//! processes.

use std::fs;
use std::path::{Path, PathBuf};

use caduceus::config::Config;
use caduceus::issue::IssueKey;
use caduceus::queue::{
    parse_queue_state, reap_stale_claims, serialize_queue_state, ClaimFileBody, ClaimToken, Phase,
    QueueEntry, QueueState, TicketType, CLAIMS_CORRUPT_DIRNAME, CLAIMS_DIRNAME,
    CLAIM_FILE_VERSION,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

fn digest_for(key: &IssueKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.display_key().as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn fresh_state() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_path = dir.path().to_path_buf();
    // Create the claims/ subdir so the reaper has somewhere to scan.
    fs::create_dir_all(state_path.join(CLAIMS_DIRNAME)).expect("claims dir");
    (dir, state_path)
}

fn write_claim_file(state_dir: &Path, body: &ClaimFileBody) -> PathBuf {
    let claims_dir = state_dir.join(CLAIMS_DIRNAME);
    fs::create_dir_all(&claims_dir).expect("claims dir");
    let digest = digest_for(&body.key);
    let path = claims_dir.join(format!("{digest}.claim"));
    let bytes = serde_json::to_vec(body).expect("serialize");
    fs::write(&path, &bytes).expect("write claim");
    path
}

fn claim_body(key: IssueKey, run_id: &str, pid: u32, started_at: DateTime<Utc>) -> ClaimFileBody {
    ClaimFileBody {
        version: CLAIM_FILE_VERSION,
        key,
        run_id: run_id.to_string(),
        pid,
        process_start_identity: "<boot>:0".to_string(),
        started_at,
        worktree_path: None,
    }
}

fn enqueue_in_progress(state_dir: &Path, key: &IssueKey, attempts: u32) {
    let cfg = Config::test_defaults(state_dir);
    let _ = cfg; // not used; we just need a state dir.
    let now = Utc::now();
    let entry = QueueEntry {
        key: key.clone(),
        phase: Phase::InProgress,
        ticket_type: TicketType::Code,
        attempts,
        last_error: None,
        last_run_id: Some("run-test".to_string()),
        next_attempt_at: None,
        finalization: None,
        queued_at: now,
        updated_at: now,
    };
    let state = QueueState {
        version: 1,
        entries: [(key.display_key(), entry)].into_iter().collect(),
    };
    let text = serialize_queue_state(&state).expect("serialize");
    let parsed = parse_queue_state(&text).expect("parse round-trip");
    fs::write(state_dir.join("state.json"), &text).expect("write state");
    let _ = parsed;
}

fn enqueue_in_phase(state_dir: &Path, key: &IssueKey, phase: Phase) {
    let now = Utc::now();
    let entry = QueueEntry {
        key: key.clone(),
        phase,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: now,
        updated_at: now,
    };
    let state = QueueState {
        version: 1,
        entries: [(key.display_key(), entry)].into_iter().collect(),
    };
    let text = serialize_queue_state(&state).expect("serialize");
    fs::write(state_dir.join("state.json"), &text).expect("write state");
}

fn key(name: &str) -> IssueKey {
    IssueKey {
        owner: name.to_string(),
        repo: "r".to_string(),
        number: 1,
    }
}

#[test]
fn stale_dead_pid_is_reaped() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-dead");
    enqueue_in_progress(&state, &k, 1);
    // Started 2 hours ago (well past stale_run_hours=1).
    let started = Utc::now() - chrono::Duration::hours(2);
    let body = claim_body(k.clone(), "run-dead", /* pid: */ 4_000_000, started);
    write_claim_file(&state, &body);
    let now = Utc::now();
    let report = reap(&state, now, 1);
    assert_eq!(report.stale_reaped, 1);
    assert_eq!(report.quarantined, 0);
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    // The entry is back to Queued, attempts unchanged.
    let state_text = fs::read_to_string(state.join("state.json")).expect("read state");
    let parsed = parse_queue_state(&state_text).expect("parse state");
    let entry = parsed.entries.get(&k.display_key()).expect("entry");
    assert_eq!(entry.phase, Phase::Queued);
    assert_eq!(entry.attempts, 1, "attempts must not be incremented");
    // The claim file is gone.
    let claims_dir = state.join(CLAIMS_DIRNAME);
    let claim_count = count_claims(&claims_dir);
    assert_eq!(claim_count, 0);
}

#[test]
fn matching_live_pid_is_retained() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-live");
    enqueue_in_progress(&state, &k, 1);
    // Recent enough that the age threshold alone does not
    // make it stale.
    let started = Utc::now() - chrono::Duration::minutes(5);
    let my_pid = std::process::id();
    let body = claim_body(k.clone(), "run-live", my_pid, started);
    write_claim_file(&state, &body);
    let now = Utc::now();
    let report = reap(&state, now, 1);
    assert_eq!(report.stale_reaped, 0);
    // Claim still there.
    let claims_dir = state.join(CLAIMS_DIRNAME);
    assert!(count_claims(&claims_dir) >= 1);
}

#[test]
fn recent_dead_pid_is_retained_until_threshold() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-recent");
    enqueue_in_progress(&state, &k, 0);
    // 5 minutes ago: well within the 1-hour threshold.
    let started = Utc::now() - chrono::Duration::minutes(5);
    let body = claim_body(k.clone(), "run-recent", 4_000_001, started);
    write_claim_file(&state, &body);
    let now = Utc::now();
    let report = reap(&state, now, 1);
    assert_eq!(
        report.stale_reaped, 0,
        "must not reap a recent dead pid below the threshold"
    );
}

#[test]
fn reused_pid_with_mismatched_start_identity_is_reaped() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-reuse");
    enqueue_in_progress(&state, &k, 0);
    let started = Utc::now() - chrono::Duration::hours(3);
    // Use a pid that is currently in use (this test process)
    // but claim an impossible start identity. The reaper
    // must compare the recorded identity against the
    // currently observed identity; mismatch → stale.
    let my_pid = std::process::id();
    let mut body = claim_body(k.clone(), "run-reuse", my_pid, started);
    body.process_start_identity = "<boot>:999999999".to_string();
    write_claim_file(&state, &body);
    let now = Utc::now();
    let report = reap(&state, now, 1);
    assert_eq!(report.stale_reaped, 1, "pid reuse must trigger reap");
}

#[test]
fn future_timestamp_is_quarantined_not_reaped() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-future");
    enqueue_in_phase(&state, &k, Phase::InProgress);
    // 10 minutes in the future — well past the 5-minute tolerance.
    let future = Utc::now() + chrono::Duration::minutes(10);
    let body = claim_body(k.clone(), "run-future", std::process::id(), future);
    write_claim_file(&state, &body);
    let now = Utc::now();
    let report = reap(&state, now, 1);
    assert_eq!(report.quarantined, 1);
    assert_eq!(report.stale_reaped, 0);
    // The file moved into claims/corrupt/ with the original
    // bytes preserved. The reaper prepends a `<!--
    // caduceus-reaper ... -->` comment to make provenance
    // obvious to an operator.
    let corrupt = state.join(CLAIMS_DIRNAME).join(CLAIMS_CORRUPT_DIRNAME);
    let entries: Vec<_> = fs::read_dir(&corrupt)
        .expect("corrupt dir")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);
    let bytes = fs::read(entries[0].path()).expect("read");
    assert!(bytes.starts_with(b"<!-- caduceus-reaper"));
    // The original claim file is gone.
    let claims = state.join(CLAIMS_DIRNAME);
    let remaining = count_claims(&claims);
    assert_eq!(remaining, 0);
}

#[test]
fn malformed_claim_is_quarantined() {
    let (_tmp, state) = fresh_state();
    let claims_dir = state.join(CLAIMS_DIRNAME);
    fs::create_dir_all(&claims_dir).expect("claims");
    // Garbage bytes — not JSON.
    fs::write(claims_dir.join("aaaa.claim"), b"not valid json").expect("write");
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.quarantined, 1);
    assert!(
        report.errors.iter().any(|e| e.contains("malformed")),
        "{:?}",
        report.errors
    );
}

#[test]
fn missing_queue_entry_orphans_claim_and_unlinks() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-orphan");
    // Note: we don't enqueue any entry. The claim references
    // a key that is not in the queue.
    let started = Utc::now() - chrono::Duration::hours(2);
    let body = claim_body(k.clone(), "run-orphan", 4_000_002, started);
    write_claim_file(&state, &body);
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.stale_reaped, 1);
    let claims = state.join(CLAIMS_DIRNAME);
    assert_eq!(count_claims(&claims), 0);
}

#[test]
fn residual_claim_for_done_entry_only_unlinks() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-done");
    enqueue_in_phase(&state, &k, Phase::Done);
    // Even an "old" claim should not change a Done entry's
    // phase — the contract says residue just unlinks.
    let started = Utc::now() - chrono::Duration::hours(5);
    let body = claim_body(k.clone(), "run-done", 4_000_003, started);
    write_claim_file(&state, &body);
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.stale_reaped, 1);
    let state_text = fs::read_to_string(state.join("state.json")).expect("read state");
    let parsed = parse_queue_state(&state_text).expect("parse");
    let entry = parsed.entries.get(&k.display_key()).expect("entry");
    assert_eq!(entry.phase, Phase::Done, "phase must not change");
}

#[test]
fn residual_claim_for_failed_entry_only_unlinks() {
    let (_tmp, state) = fresh_state();
    let k = key("owner-failed");
    enqueue_in_phase(&state, &k, Phase::Failed);
    let started = Utc::now() - chrono::Duration::hours(5);
    let body = claim_body(k.clone(), "run-failed", 4_000_004, started);
    write_claim_file(&state, &body);
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.stale_reaped, 1);
    let state_text = fs::read_to_string(state.join("state.json")).expect("read state");
    let parsed = parse_queue_state(&state_text).expect("parse");
    let entry = parsed.entries.get(&k.display_key()).expect("entry");
    assert_eq!(entry.phase, Phase::Failed, "Failed entries stay Failed");
}

#[test]
fn symlink_in_claims_dir_is_reported_and_left_alone() {
    let (_tmp, state) = fresh_state();
    let claims_dir = state.join(CLAIMS_DIRNAME);
    fs::create_dir_all(&claims_dir).expect("claims");
    // Build a symlink: a regular file outside the dir, then
    // a symlink inside the dir pointing at it.
    let target = state.join("sentinel.txt");
    fs::write(&target, b"hello").expect("write sentinel");
    let link = claims_dir.join("evil.claim");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let report = reap(&state, Utc::now(), 1);
    // The reaper reports the symlink and leaves it
    // untouched.
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("refusing to act on symlink")),
        "{:?}",
        report.errors
    );
    assert!(link.exists(), "symlink must not be deleted");
}

#[test]
fn unknown_non_claim_file_is_left_alone() {
    let (_tmp, state) = fresh_state();
    let claims_dir = state.join(CLAIMS_DIRNAME);
    fs::create_dir_all(&claims_dir).expect("claims");
    fs::write(claims_dir.join("README"), b"ops: ignore me").expect("write");
    let report = reap(&state, Utc::now(), 1);
    assert!(report.errors.iter().any(|e| e.contains("unknown file")));
    let _ = fs::read(claims_dir.join("README")).expect("README must remain");
}

#[test]
fn reap_metadata_is_returned() {
    let (_tmp, state) = fresh_state();
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.count, 0);
    assert_eq!(report.stale_reaped, 0);
    assert_eq!(report.quarantined, 0);
    assert!(report.errors.is_empty());
}

#[test]
fn reap_handles_missing_claims_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().to_path_buf();
    // No claims/ subdir.
    let report = reap(&state, Utc::now(), 1);
    assert_eq!(report.count, 0);
}

// --- helpers --------------------------------------------------------------

fn count_claims(claims_dir: &Path) -> usize {
    fs::read_dir(claims_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".claim"))
                .count()
        })
        .unwrap_or(0)
}

// `tokio_test::block_on` is a runtime we don't otherwise
// depend on. The reaper is `async` so we need *some* runtime
// to drive it. We use the small ad-hoc helper here so the
// test file does not pull in a new dependency.
fn tokio_test_block_on<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    rt.block_on(f)
}

fn reap(state: &Path, now: DateTime<Utc>, stale_hours: u64) -> caduceus::queue::ReapReport {
    let r = tokio_test_block_on(reap_stale_claims(state, now, stale_hours));
    r.expect("reap")
}

#[allow(dead_code)]
fn _claim_token_is_compatible(token: &ClaimToken) {
    // The reaper tests do not need a real `ClaimToken`, but
    // we keep this stub so the import remains in scope if a
    // future test wants to construct one.
    let _ = token.digest();
}
