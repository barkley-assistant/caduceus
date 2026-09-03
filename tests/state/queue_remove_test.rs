//! `caduceus queue remove` — operator-driven queue entry removal.
//!
//! These tests drive the CLI as a subprocess via
//! `env!("CARGO_BIN_EXE_caduceus")` and check:
//!
//! * Allowed-by-default phases (`Failed`, `Skipped`, `Queued`,
//!   `NeedsAttention`) are removed.
//! * `InProgress`, `AwaitingReview`, and `Done` are refused by
//!   default; `--force` relaxes the phase guard only.
//! * An active claim file is refused even with `--force`.
//! * `--dry-run` reports the plan without mutating state.
//! * `--json` emits the versioned `queue/1.0` envelope with a
//!   `RemoveOutcome` payload.
//! * The remote branch / PR are never deleted (warning text).
//! * The live path refuses while the daemon lock is held.
//! * A missing entry is an error; `$CADUCEUS_CONFIG` is honoured.

use chrono::Utc;

use caduceus::queue::{
    FinalizationCheckpoint, FinalizationStage, Phase, QueueEntry, QueueState, StateStore,
    TicketType, QUEUE_FILE_VERSION,
};
use caduceus::IssueKey;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn key(owner: &str, repo: &str, number: u64) -> IssueKey {
    IssueKey {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    }
}

fn entry(k: &IssueKey, phase: Phase, checkpoint: Option<FinalizationCheckpoint>) -> QueueEntry {
    QueueEntry {
        key: k.clone(),
        phase,
        ticket_type: TicketType::Code,
        attempts: 2,
        last_error: Some("seed".to_string()),
        last_run_id: Some("RUN-1".to_string()),
        next_attempt_at: None,
        finalization: checkpoint,
        queued_at: Utc::now(),
        updated_at: Utc::now(),
        blocked_source: None,
        blocked_recovery_hint: None,
        generation: 1,
    }
}

fn checkpoint(state_dir: &Path, k: &IssueKey) -> FinalizationCheckpoint {
    FinalizationCheckpoint {
        run_id: "RUN-1".to_string(),
        branch_name: format!("automation/issue-{}-run-1", k.number),
        result_path: state_dir.join("runs").join("RUN-1.result.json"),
        stage: FinalizationStage::Pushed,
        commit_oid: Some("abc123".to_string()),
        pr_number: Some(42),
        pr_url: Some(format!("https://github.com/{}/pull/42", k.display_key())),
    }
}

fn seed_state(state_dir: &Path, k: &IssueKey, phase: Phase) {
    let mut map = BTreeMap::new();
    map.insert(k.display_key(), entry(k, phase, None));
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: map,
        },
    );
}

fn seed_state_with_checkpoint(
    state_dir: &Path,
    k: &IssueKey,
    phase: Phase,
) -> FinalizationCheckpoint {
    let check = checkpoint(state_dir, k);
    let mut map = BTreeMap::new();
    map.insert(k.display_key(), entry(k, phase, Some(check.clone())));
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: map,
        },
    );
    check
}

fn write_state(path: &Path, state: &QueueState) {
    let body = caduceus::queue::serialize_queue_state(state).expect("serialize");
    fs::write(path, body).expect("write state");
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

fn parse_json(output: &std::process::Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).expect("stdout must be JSON")
}

fn assert_combined_contains(output: &std::process::Output, needle: &str) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.to_lowercase().contains(needle),
        "expected {needle:?} in output; got {combined:?}"
    );
}

fn assert_snapshot_has(state_dir: &Path, k: &IssueKey, present: bool) {
    let store = StateStore::open(state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    assert_eq!(snap.entry(k).is_some(), present, "entry presence mismatch");
}

// Allowed by default

#[test]
fn remove_failed_entry_succeeds_and_drops_checkpoint() {
    let state_dir = tempdir("remove-failed");
    let k = key("Owner", "Repo", 1);
    let check = seed_state_with_checkpoint(&state_dir, &k, Phase::Failed);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_combined_contains(&output, "removed owner/repo#1");
    assert_snapshot_has(&state_dir, &k, false);
    // The checkpoint was dropped with the entry; the operator is
    // warned that the remote branch / PR were NOT deleted.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&check.branch_name),
        "branch warning missing"
    );
    assert!(
        stderr.to_lowercase().contains("not deleted"),
        "expected explicit not-deleted warning; got {stderr}"
    );
}

#[test]
fn remove_skipped_entry_succeeds() {
    let state_dir = tempdir("remove-skipped");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Skipped);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(output.status.success(), "expected success");
    assert_snapshot_has(&state_dir, &k, false);
}

#[test]
fn remove_queued_entry_succeeds() {
    let state_dir = tempdir("remove-queued");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Queued);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(output.status.success(), "expected success");
    assert_snapshot_has(&state_dir, &k, false);
}

#[test]
fn remove_needs_attention_entry_succeeds() {
    let state_dir = tempdir("remove-needs-attention");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::NeedsAttention);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(output.status.success(), "expected success");
    assert_snapshot_has(&state_dir, &k, false);
}

// Refused by default

#[test]
fn remove_in_progress_refused_by_default() {
    let state_dir = tempdir("remove-in-progress");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::InProgress);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    // The guard surfaces the Debug form of the phase ("InProgress").
    assert_combined_contains(&output, "inprogress");
    assert_snapshot_has(&state_dir, &k, true);
}

#[test]
fn remove_awaiting_review_refused_by_default() {
    let state_dir = tempdir("remove-awaiting");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::AwaitingReview);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    assert_combined_contains(&output, "awaitingreview");
    assert_snapshot_has(&state_dir, &k, true);
}

#[test]
fn remove_done_refused_by_default() {
    let state_dir = tempdir("remove-done");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Done);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(!output.status.success(), "expected failure");
    assert_combined_contains(&output, "done");
    assert_snapshot_has(&state_dir, &k, true);
}

// --force relaxes the phase guard

#[test]
fn remove_in_progress_with_force_succeeds() {
    let state_dir = tempdir("remove-in-progress-force");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::InProgress);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1", "--force"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_snapshot_has(&state_dir, &k, false);
}

#[test]
fn remove_done_with_force_succeeds() {
    let state_dir = tempdir("remove-done-force");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Done);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1", "--force"]);
    assert!(output.status.success(), "expected success");
    assert_snapshot_has(&state_dir, &k, false);
}

#[test]
fn remove_awaiting_review_with_force_succeeds() {
    let state_dir = tempdir("remove-awaiting-force");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::AwaitingReview);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1", "--force"]);
    assert!(output.status.success(), "expected success");
    assert_snapshot_has(&state_dir, &k, false);
}

// Active claim guard is never relaxable

#[test]
fn remove_with_active_claim_refused_even_with_force() {
    let state_dir = tempdir("remove-claim");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::InProgress);
    // Manually create a claim file to simulate an active claim.
    let claims_dir = state_dir.join("claims");
    fs::create_dir_all(&claims_dir).unwrap();
    let digest = caduceus::queue::display_digest(&k.display_key());
    fs::write(claims_dir.join(format!("{digest}.claim")), b"{}").unwrap();
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1", "--force"]);
    assert!(
        !output.status.success(),
        "expected failure even with --force"
    );
    assert_combined_contains(&output, "claim");
    assert_snapshot_has(&state_dir, &k, true);
}

#[test]
fn remove_force_dry_run_with_active_claim_refused() {
    let state_dir = tempdir("remove-claim-dry");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::InProgress);
    let claims_dir = state_dir.join("claims");
    fs::create_dir_all(&claims_dir).unwrap();
    let digest = caduceus::queue::display_digest(&k.display_key());
    fs::write(claims_dir.join(format!("{digest}.claim")), b"{}").unwrap();
    let output = run_cli(
        &state_dir,
        &["queue", "remove", "owner/repo#1", "--force", "--dry-run"],
    );
    assert!(!output.status.success(), "expected failure");
    assert_combined_contains(&output, "claim");
    assert_snapshot_has(&state_dir, &k, true);
}

// --dry-run

#[test]
fn remove_dry_run_does_not_mutate_state() {
    let state_dir = tempdir("remove-dry");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Failed);
    let output = run_cli(
        &state_dir,
        &["queue", "remove", "owner/repo#1", "--dry-run"],
    );
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("would remove"), "got {stdout}");
    assert_snapshot_has(&state_dir, &k, true);
}

#[test]
fn remove_dry_run_reports_checkpoint_drop() {
    let state_dir = tempdir("remove-dry-checkpoint");
    let k = key("Owner", "Repo", 1);
    seed_state_with_checkpoint(&state_dir, &k, Phase::Failed);
    let output = run_cli(
        &state_dir,
        &["queue", "remove", "owner/repo#1", "--dry-run"],
    );
    assert!(output.status.success(), "expected success");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("checkpoint would be dropped"),
        "expected checkpoint plan; got {stdout}"
    );
    assert_snapshot_has(&state_dir, &k, true);
}

// --json

#[test]
fn remove_json_emits_remove_outcome_payload() {
    let state_dir = tempdir("remove-json");
    let k = key("Owner", "Repo", 1);
    let check = seed_state_with_checkpoint(&state_dir, &k, Phase::Failed);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1", "--json"]);
    assert!(output.status.success(), "expected success");
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "queue/1.0");
    assert_eq!(envelope["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(envelope["diagnostic"], serde_json::Value::Null);
    let payload = &envelope["payload"];
    assert_eq!(payload["key"], "owner/repo#1");
    assert_eq!(payload["phase"], "failed");
    assert_eq!(
        payload["dropped_checkpoint"]["branch_name"],
        check.branch_name
    );
    assert_eq!(payload["dropped_checkpoint"]["pr_number"], 42);
    assert_snapshot_has(&state_dir, &k, false);
}

#[test]
fn remove_dry_run_json_emits_planned_outcome() {
    let state_dir = tempdir("remove-json-dry");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Failed);
    let output = run_cli(
        &state_dir,
        &["queue", "remove", "owner/repo#1", "--dry-run", "--json"],
    );
    assert!(output.status.success(), "expected success");
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "queue/1.0");
    assert_eq!(envelope["payload"]["key"], "owner/repo#1");
    assert_eq!(envelope["payload"]["phase"], "failed");
    assert_snapshot_has(&state_dir, &k, true);
}

// Missing entry / malformed ref

#[test]
fn remove_missing_entry_is_rejected() {
    let state_dir = tempdir("remove-missing");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Failed);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#99"]);
    assert!(!output.status.success(), "expected failure");
    assert_combined_contains(&output, "no entry");
    assert_snapshot_has(&state_dir, &k, true);
}

#[test]
fn remove_malformed_issue_ref_is_rejected() {
    let state_dir = tempdir("remove-malformed");
    let output = run_cli(&state_dir, &["queue", "remove", "not-a-key"]);
    assert!(!output.status.success(), "expected failure");
}

// Concurrent tick refusal

#[test]
fn remove_refuses_when_daemon_lock_held() {
    let state_dir = tempdir("remove-locked");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Failed);
    // Hold the daemon lock.
    let lock = caduceus::DaemonLock::try_acquire(&state_dir)
        .expect("lock")
        .expect("some");
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    drop(lock);
    assert!(!output.status.success(), "expected failure while lock held");
    assert_combined_contains(&output, "another tick");
    assert_snapshot_has(&state_dir, &k, true);
}

// Config resolution

#[test]
fn remove_respects_caduceus_config_env() {
    // The run_cli helper points $CADUCEUS_CONFIG at the fixture's
    // config.yaml, whose state_dir is the fixture dir. If the env
    // var were ignored, the default state dir would have no entry
    // and the remove would fail with "no entry".
    let state_dir = tempdir("remove-config");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &k, Phase::Failed);
    let output = run_cli(&state_dir, &["queue", "remove", "owner/repo#1"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_snapshot_has(&state_dir, &k, false);
}
