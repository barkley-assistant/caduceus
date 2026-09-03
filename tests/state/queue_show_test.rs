//! `caduceus queue show` — read-only queue inspection.
//!
//! These tests drive the CLI as a subprocess via
//! `env!("CARGO_BIN_EXE_caduceus")` and check:
//!
//! * The list form renders every entry as a human table in
//!   `BTreeMap` (lexical) order with the documented columns.
//! * The list `--json` form emits the versioned `queue/1.0`
//!   envelope with an ordered `entries` array.
//! * The detail form (`<key>`) prints full entry detail including
//!   the finalization checkpoint (branch, run id, stage, PR).
//! * The detail `--json` form puts the entry in `payload` with the
//!   checkpoint fields present.
//! * A missing key errors on the human path and emits
//!   `diagnostic: "no_entry"` on the JSON path.
//! * An empty queue prints a placeholder / `entries: []`.
//! * `$CADUCEUS_CONFIG` is honoured (the fixture's config is used).

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
        run_id: format!("RUN-{}", k.number),
        branch_name: format!("automation/issue-{}-run-1", k.number),
        result_path: state_dir.join("runs").join("RUN-1.result.json"),
        stage: FinalizationStage::Pushed,
        commit_oid: Some("abc123".to_string()),
        pr_number: Some(42),
        pr_url: Some(format!("https://github.com/{}/pull/42", k.display_key())),
    }
}

fn seed_state(state_dir: &Path, entries: &[(IssueKey, Phase, Option<FinalizationCheckpoint>)]) {
    let mut map = BTreeMap::new();
    for (k, phase, checkpoint) in entries {
        map.insert(k.display_key(), entry(k, *phase, checkpoint.clone()));
    }
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: map,
        },
    );
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

fn sample_keys() -> Vec<IssueKey> {
    // Deliberately mixed case so the table proves the lowercase
    // display key is what is rendered, in lexical BTreeMap order.
    vec![
        key("Owner", "Repo", 3),
        key("Alpha", "Project", 1),
        key("Zed", "Repo", 2),
    ]
}

// List form: human table

#[test]
fn list_renders_all_entries_in_lexical_order_with_columns() {
    let state_dir = tempdir("show-list");
    let keys = sample_keys();
    let rows: Vec<(IssueKey, Phase, Option<FinalizationCheckpoint>)> = keys
        .iter()
        .map(|k| (k.clone(), Phase::Failed, None))
        .collect();
    seed_state(&state_dir, &rows);
    let output = run_cli(&state_dir, &["queue", "show"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Columns are present.
    for col in ["key", "phase", "ticket", "attempts", "generation", "age"] {
        assert!(stdout.contains(col), "missing column {col:?} in {stdout}");
    }
    // All three display keys appear, in lexical order.
    let alpha = stdout.find("alpha/project#1").expect("alpha key present");
    let owner = stdout.find("owner/repo#3").expect("owner key present");
    let zed = stdout.find("zed/repo#2").expect("zed key present");
    assert!(alpha < owner && owner < zed, "keys not lexical: {stdout}");
}

// List form: --json

#[test]
fn list_json_emits_versioned_envelope_with_ordered_entries() {
    let state_dir = tempdir("show-list-json");
    let keys = sample_keys();
    let rows: Vec<(IssueKey, Phase, Option<FinalizationCheckpoint>)> = keys
        .iter()
        .map(|k| (k.clone(), Phase::Failed, None))
        .collect();
    seed_state(&state_dir, &rows);
    let output = run_cli(&state_dir, &["queue", "show", "--json"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "queue/1.0");
    assert_eq!(envelope["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(envelope["diagnostic"], serde_json::Value::Null);
    let entries = envelope["payload"]["entries"]
        .as_array()
        .expect("payload.entries must be an array");
    assert_eq!(entries.len(), 3);
    // Ordered by display key.
    let rendered: Vec<String> = entries
        .iter()
        .map(|e| e["key"]["owner"].as_str().unwrap().to_lowercase())
        .collect();
    assert_eq!(rendered, vec!["alpha", "owner", "zed"]);
}

// Detail form: human

#[test]
fn detail_renders_full_entry_including_checkpoint() {
    let state_dir = tempdir("show-detail");
    let k = key("Owner", "Repo", 1);
    let check = checkpoint(&state_dir, &k);
    seed_state(
        &state_dir,
        &[(k.clone(), Phase::Failed, Some(check.clone()))],
    );
    let output = run_cli(&state_dir, &["queue", "show", "owner/repo#1"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("entry owner/repo#1"), "got {stdout}");
    // Checkpoint fields surface in the human view.
    for needle in [
        &check.branch_name,
        &check.run_id,
        "pushed",
        &check.pr_url.clone().unwrap(),
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in {stdout}");
    }
    assert!(
        stdout.contains("pr_number"),
        "missing PR column in {stdout}"
    );
}

// Detail form: --json

#[test]
fn detail_json_payload_is_the_entry_with_checkpoint() {
    let state_dir = tempdir("show-detail-json");
    let k = key("Owner", "Repo", 1);
    let check = checkpoint(&state_dir, &k);
    seed_state(
        &state_dir,
        &[(k.clone(), Phase::Failed, Some(check.clone()))],
    );
    let output = run_cli(&state_dir, &["queue", "show", "owner/repo#1", "--json"]);
    assert!(output.status.success(), "expected success");
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "queue/1.0");
    assert_eq!(envelope["diagnostic"], serde_json::Value::Null);
    let payload = &envelope["payload"];
    assert_eq!(payload["phase"], "failed");
    assert_eq!(payload["key"]["number"], 1);
    let fin = &payload["finalization"];
    assert_eq!(fin["branch_name"], check.branch_name);
    assert_eq!(fin["run_id"], check.run_id);
    assert_eq!(fin["stage"], "pushed");
    assert_eq!(fin["pr_number"], 42);
}

// Missing key

#[test]
fn detail_missing_key_human_path_errors() {
    let state_dir = tempdir("show-missing");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &[(k.clone(), Phase::Failed, None)]);
    let output = run_cli(&state_dir, &["queue", "show", "owner/repo#99"]);
    assert!(!output.status.success(), "expected failure");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.to_lowercase().contains("no entry"),
        "expected 'no entry'; got {combined:?}"
    );
}

#[test]
fn detail_missing_key_json_emits_no_entry_diagnostic() {
    let state_dir = tempdir("show-missing-json");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &[(k.clone(), Phase::Failed, None)]);
    let output = run_cli(&state_dir, &["queue", "show", "owner/repo#99", "--json"]);
    assert!(!output.status.success(), "expected non-zero exit");
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "queue/1.0");
    assert_eq!(envelope["diagnostic"], "no_entry");
    assert_eq!(envelope["payload"], serde_json::Value::Null);
}

// Empty queue

#[test]
fn empty_queue_lists_placeholder() {
    let state_dir = tempdir("show-empty");
    fs::create_dir_all(&state_dir).unwrap();
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: BTreeMap::new(),
        },
    );
    let output = run_cli(&state_dir, &["queue", "show"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no entries"),
        "expected placeholder; got {stdout}"
    );
}

#[test]
fn empty_queue_json_emits_empty_entries_array() {
    let state_dir = tempdir("show-empty-json");
    fs::create_dir_all(&state_dir).unwrap();
    write_state(
        &state_dir.join("state.json"),
        &QueueState {
            version: QUEUE_FILE_VERSION,
            entries: BTreeMap::new(),
        },
    );
    let output = run_cli(&state_dir, &["queue", "show", "--json"]);
    assert!(output.status.success(), "expected success");
    let envelope = parse_json(&output);
    assert_eq!(envelope["payload"]["entries"], serde_json::json!([]));
}

// Config resolution

#[test]
fn show_respects_caduceus_config_env() {
    // The run_cli helper points $CADUCEUS_CONFIG at the fixture's
    // config.yaml, whose state_dir is the fixture dir. If the env
    // var were ignored, the CLI would resolve the default state dir
    // (no state.json there) and the seeded entry would not appear.
    let state_dir = tempdir("show-config");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &[(k.clone(), Phase::Failed, None)]);
    let output = run_cli(&state_dir, &["queue", "show"]);
    assert!(
        output.status.success(),
        "expected success; got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("owner/repo#1"),
        "fixture config not honoured; got {stdout}"
    );
}

// Read-only: show never mutates state

#[test]
fn show_does_not_mutate_state() {
    let state_dir = tempdir("show-readonly");
    let k = key("Owner", "Repo", 1);
    seed_state(&state_dir, &[(k.clone(), Phase::Failed, None)]);
    let before = fs::read(state_dir.join("state.json")).expect("read state");
    let output = run_cli(&state_dir, &["queue", "show", "--json"]);
    assert!(output.status.success(), "expected success");
    let after = fs::read(state_dir.join("state.json")).expect("read state");
    assert_eq!(before, after, "show must not rewrite state.json");
    // A re-snapshot still contains the entry.
    let store = StateStore::open(&state_dir).expect("open");
    let snap = store.snapshot().unwrap();
    assert!(snap.entry(&k).is_some(), "entry must survive show");
}
