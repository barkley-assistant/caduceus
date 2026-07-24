//! CLI.
//!
//! The CLI is the only v0.1 recovery operation for a `Failed`
//! "Finalization contract" / Task 3.4. These tests drive the
//! CLI as a subprocess via `env!("CARGO_BIN_EXE_caduceus")` and
//! check:
//!
//! * A `Failed` entry is reset to `Queued` with attempts=0.
//! * A `Skipped` entry is reset to `Queued`.
//! * A non-terminal entry (`Queued`, `InProgress`, `Previewed`,
//!   `Done`) is rejected.
//! * An entry with an active claim is rejected.
//! * A checkpoint is preserved by default.
//! * `--force-finalization-reset` drops the checkpoint and
//!   surfaces the branch/PR in a warning.
//! * `--dry-run` does not mutate state.
//! * A malformed issue ref is rejected.

#![allow(unused_variables, unused_imports)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::Utc;

use caduceus::queue::{
    FinalizationCheckpoint, FinalizationStage, Phase, QueueEntry, QueueState, StateStore,
    TicketType, QUEUE_FILE_VERSION,
};
use caduceus::{IssueKey, StateStore as _};

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-queue-reset-test-{label}-{nonce}"));
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

fn write_state(path: &Path, state: &QueueState) {
    let body = caduceus::queue::serialize_queue_state(state).expect("serialize");
    fs::write(path, body).expect("write state");
}

fn seed_failed(state_dir: &Path, k: &IssueKey, attempts: u32) {
    let mut entries = BTreeMap::new();
    let e = QueueEntry {
        key: k.clone(),
        phase: Phase::Failed,
        ticket_type: TicketType::Code,
        attempts,
        last_error: Some("seed".to_string()),
        last_run_id: Some("SEED".to_string()),
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        generation: 1,
    };
    entries.insert(k.display_key(), e);
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
}

fn seed_skipped(state_dir: &Path, k: &IssueKey) {
    let mut entries = BTreeMap::new();
    let e = QueueEntry {
        key: k.clone(),
        phase: Phase::Skipped,
        ticket_type: TicketType::Code,
        attempts: 3,
        last_error: Some("voice violation".to_string()),
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        generation: 1,
    };
    entries.insert(k.display_key(), e);
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
}

fn seed_queued(state_dir: &Path, k: &IssueKey) {
    let mut entries = BTreeMap::new();
    let e = QueueEntry {
        key: k.clone(),
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        generation: 1,
    };
    entries.insert(k.display_key(), e);
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
}

fn seed_failed_with_checkpoint(state_dir: &Path, k: &IssueKey) -> FinalizationCheckpoint {
    let checkpoint = FinalizationCheckpoint {
        run_id: "RUN-1".to_string(),
        branch_name: "automation/issue-1-run-1".to_string(),
        result_path: state_dir.join("runs").join("RUN-1.result.json"),
        stage: FinalizationStage::Pushed,
        commit_oid: Some("abc123".to_string()),
        pr_number: Some(42),
        pr_url: Some("https://github.com/owner/repo/pull/42".to_string()),
    };
    let mut entries = BTreeMap::new();
    let e = QueueEntry {
        key: k.clone(),
        phase: Phase::Failed,
        ticket_type: TicketType::Code,
        attempts: 3,
        last_error: Some("post-pr failure".to_string()),
        last_run_id: Some("RUN-1".to_string()),
        next_attempt_at: None,
        finalization: Some(checkpoint.clone()),
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        generation: 1,
    };
    entries.insert(k.display_key(), e);
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
    checkpoint
}

fn run_cli(state_dir: &Path, args: &[&str]) -> std::process::Output {
    // Use $CADUCEUS_CONFIG to point at a YAML config that sets the
    // state_dir we want. The CLI also needs HERMES_HOME for some
    // config paths; we set it to a temp dir to keep it isolated.
    let mut hermes_home = state_dir.to_path_buf();
    hermes_home.push("hermes");
    fs::create_dir_all(&hermes_home).unwrap();
    let config_path = state_dir.join("config.yaml");
    let yaml = format!(
        "caduceus:\n  state_dir: \"{}\"\n  worker_command:\n    - \"python3\"\n    - \"{}/bridge.py\"\n  reduced_containment_acknowledged: true\n",
        state_dir.display(),
        state_dir.display()
    );
    fs::write(&config_path, yaml).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_caduceus"));
    cmd.env("CADUCEUS_CONFIG", &config_path)
        .env("HERMES_HOME", &hermes_home)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().expect("spawn caduceus")
}

fn assert_reset_returned_failed_or_skipped(output: &std::process::Output, expected_kind: &str) {
    assert!(
        !output.status.success(),
        "expected non-zero exit; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.to_lowercase().contains(expected_kind),
        "expected {expected_kind:?} in output; got stdout: {stdout:?}, stderr: {stderr:?}"
    );
}

// Reset terminal entry (Failed and Skipped)

#[test]
fn reset_failed_entry_returns_to_queued() {
    let state_dir = tempdir("reset-failed");
    let k = key("Owner", "Repo", 1);
    seed_failed(&state_dir, &k, 3);
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.contains("reset"),
        "expected reset message; got {stdout}"
    );
    // Verify the state was actually updated.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Queued);
    assert_eq!(e.attempts, 0);
    assert!(e.last_error.is_none());
    assert!(e.last_run_id.is_none());
    assert!(e.next_attempt_at.is_none());
}

#[test]
fn reset_skipped_entry_returns_to_queued() {
    let state_dir = tempdir("reset-skipped");
    let k = key("Owner", "Repo", 1);
    seed_skipped(&state_dir, &k);
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Queued);
    assert_eq!(e.attempts, 0);
}

// Reset of non-terminal entry is rejected

#[test]
fn reset_queued_entry_is_rejected() {
    let state_dir = tempdir("reset-queued");
    let k = key("Owner", "Repo", 1);
    seed_queued(&state_dir, &k);
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    assert_reset_returned_failed_or_skipped(&output, "queued");
}

#[test]
fn reset_done_entry_is_rejected() {
    let state_dir = tempdir("reset-done");
    let k = key("Owner", "Repo", 1);
    let mut entries = BTreeMap::new();
    let e = QueueEntry {
        key: k.clone(),
        phase: Phase::Done,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization: None,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        generation: 1,
    };
    entries.insert(k.display_key(), e);
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries,
        },
    );
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    assert_reset_returned_failed_or_skipped(&output, "done");
}

// Active claim refusal

#[test]
fn reset_with_active_claim_is_rejected() {
    let state_dir = tempdir("reset-active-claim");
    let k = key("Owner", "Repo", 1);
    seed_failed(&state_dir, &k, 3);
    // Manually create a claim file to simulate an active claim.
    let claims_dir = state_dir.join("claims");
    fs::create_dir_all(&claims_dir).unwrap();
    let digest = caduceus::queue::display_digest(&k.display_key());
    fs::write(claims_dir.join(format!("{digest}.claim")), b"{}").unwrap();
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.to_lowercase().contains("claim"),
        "expected claim-related message; got {combined:?}"
    );
    // The state should still be Failed.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Failed, "state should be unchanged");
}

// Checkpoint preservation and forced reset

#[test]
fn reset_preserves_checkpoint_by_default() {
    let state_dir = tempdir("reset-preserve");
    let k = key("Owner", "Repo", 1);
    let checkpoint = seed_failed_with_checkpoint(&state_dir, &k);
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Queued);
    let stored = e.finalization.as_ref().expect("checkpoint preserved");
    assert_eq!(stored.run_id, checkpoint.run_id);
    assert_eq!(stored.branch_name, checkpoint.branch_name);
    assert_eq!(stored.stage, checkpoint.stage);
    assert_eq!(stored.commit_oid, checkpoint.commit_oid);
}

#[test]
fn forced_reset_drops_checkpoint_and_warns() {
    let state_dir = tempdir("reset-forced");
    let k = key("Owner", "Repo", 1);
    let checkpoint = seed_failed_with_checkpoint(&state_dir, &k);
    let output = run_cli(
        &state_dir,
        &[
            "queue",
            "reset",
            "owner/repo#1",
            "--force-finalization-reset",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    assert!(
        stdout.contains("reset"),
        "expected reset message; got {stdout}"
    );
    assert!(
        stderr.contains("warning") && stderr.contains(&checkpoint.branch_name),
        "expected warning with branch_name; got stderr: {stderr}"
    );
    assert!(
        stderr.contains(&checkpoint.pr_url.clone().unwrap()),
        "expected PR URL in warning; got stderr: {stderr}"
    );
    // The state should have been reset and the checkpoint cleared.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Queued);
    assert!(e.finalization.is_none(), "checkpoint dropped");
}

#[test]
fn forced_reset_does_not_delete_remote_branch_or_pr() {
    // The CLI is documented to never delete the remote branch or
    // PR; this is asserted via the warning text.
    let state_dir = tempdir("no-delete");
    let k = key("Owner", "Repo", 1);
    let _ = seed_failed_with_checkpoint(&state_dir, &k);
    let output = run_cli(
        &state_dir,
        &[
            "queue",
            "reset",
            "owner/repo#1",
            "--force-finalization-reset",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("not deleted") || stderr.to_lowercase().contains("manual"),
        "expected explicit 'not deleted' message; got {stderr}"
    );
}

// Dry-run

#[test]
fn reset_dry_run_does_not_mutate_state() {
    let state_dir = tempdir("reset-dry");
    let k = key("Owner", "Repo", 1);
    seed_failed(&state_dir, &k, 3);
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success; got {output:?}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("would reset"),
        "expected dry-run header; got {stdout}"
    );
    // State unchanged.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Failed);
    assert_eq!(e.attempts, 3);
    assert_eq!(e.last_error.as_deref(), Some("seed"));
}

#[test]
fn reset_dry_run_with_force_reports_would_drop() {
    let state_dir = tempdir("reset-dry-force");
    let k = key("Owner", "Repo", 1);
    let _ = seed_failed_with_checkpoint(&state_dir, &k);
    let output = run_cli(
        &state_dir,
        &[
            "queue",
            "reset",
            "owner/repo#1",
            "--dry-run",
            "--force-finalization-reset",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    assert!(
        stdout.contains("would drop"),
        "expected 'would drop' in dry-run output; got {stdout}"
    );
    // State unchanged (still Failed with checkpoint).
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Failed);
    assert!(e.finalization.is_some());
}

// Malformed issue ref

#[test]
fn reset_malformed_issue_ref_is_rejected() {
    let state_dir = tempdir("reset-malformed");
    let output = run_cli(&state_dir, &["queue", "reset", "not-a-key"]);
    assert!(!output.status.success(), "expected failure");
}

#[test]
fn reset_unknown_entry_is_rejected() {
    let state_dir = tempdir("reset-unknown");
    fs::create_dir_all(&state_dir).unwrap();
    // State file with no entries.
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: BTreeMap::new(),
        },
    );
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.to_lowercase().contains("no entry"),
        "expected 'no entry' message; got {combined:?}"
    );
}

// Concurrent tick refusal

#[test]
fn reset_refuses_when_daemon_lock_held() {
    let state_dir = tempdir("reset-locked");
    let k = key("Owner", "Repo", 1);
    seed_failed(&state_dir, &k, 3);
    // Hold the daemon lock.
    let lock = caduceus::DaemonLock::try_acquire(&state_dir)
        .expect("lock")
        .expect("some");
    let output = run_cli(&state_dir, &["queue", "reset", "owner/repo#1"]);
    drop(lock);
    assert!(!output.status.success(), "expected failure while lock held");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.to_lowercase().contains("another tick")
            || combined.to_lowercase().contains("held"),
        "expected concurrency message; got {combined:?}"
    );
    // State unchanged.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    let e = snap.entry(&k).expect("present");
    assert_eq!(e.phase, Phase::Failed);
}
