//! Integration test: `migrate-state --to-sqlite` imports JSON state
//! and metadata, preserves the originals as backups, and writes the
//! `state_backend` config flag.

use caduceus::config::Config;
use caduceus::meta::{MetaStore, TickOutcome};
use caduceus::migrate_to_sqlite::{migrate_to_sqlite, LockPolicy, SqliteMigrationOutcome};
use caduceus::queue::StateStore;
#[path = "fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::fs;
use std::path::Path;

fn write_json_state(state_dir: &Path) {
    fs::write(
        state_dir.join("state.json"),
        r#"{"version":1,"entries":{"owner/repo#1":{"phase":"queued","ticket_type":"code","attempts":0,"queued_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},"owner/repo#2":{"phase":"in_progress","ticket_type":"investigation","attempts":1,"last_error":"timeout","queued_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z"}}}"#,
    )
    .unwrap();
}

fn write_json_meta(state_dir: &Path) {
    fs::write(
        state_dir.join("state_meta.json"),
        r#"{"version":1,"last_tick_started":"2026-01-01T00:00:00Z","last_tick_finished":"2026-01-01T00:00:01Z","last_outcome":"processed","last_http_status":200,"next_allowed_poll_at":"2026-01-01T00:00:30Z","last_reap_at":"2026-01-01T00:00:00Z","last_reaped_count":3,"rate_limit":null,"last_error":"boom","recent_diagnostics":[]}"#,
    )
    .unwrap();
}

#[test]
fn migration_imports_queue_entries_and_metadata_to_sqlite() {
    let root = tempdir("backend");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    write_json_state(&state_dir);
    write_json_meta(&state_dir);

    let report = migrate_to_sqlite(&state_dir, false, LockPolicy::Acquire, None)
        .expect("migration succeeds");
    assert!(
        matches!(
            report.outcome,
            SqliteMigrationOutcome::Migrated { entries: 2 }
        ),
        "expected Migrated 2 entries, got {report:?}"
    );

    // Original JSON files are preserved as validated backups.
    assert!(
        state_dir.join("state.json").is_file(),
        "state.json backup must remain"
    );
    assert!(
        state_dir.join("state_meta.json").is_file(),
        "state_meta.json backup must remain"
    );

    // SQLite store now has the entries.
    let sqlite_store = StateStore::open_sqlite(&state_dir).expect("open sqlite store");
    let snap = sqlite_store.snapshot().expect("snapshot");
    assert_eq!(snap.entries.len(), 2, "must have two entries");
    let e = snap
        .entry(&caduceus::issue::IssueKey::parse("owner/repo#2").unwrap())
        .expect("owner/repo#2 present");
    assert_eq!(e.phase, caduceus::queue::Phase::InProgress);
    assert_eq!(e.ticket_type, caduceus::queue::TicketType::Investigation);
    assert_eq!(e.last_error.as_deref(), Some("timeout"));

    // SQLite metadata also imported.
    let sqlite_meta = MetaStore::open_sqlite(&state_dir).expect("open sqlite meta");
    let meta = sqlite_meta.snapshot();
    assert_eq!(meta.last_outcome, Some(TickOutcome::Processed));
    assert_eq!(meta.last_http_status, Some(200));
    assert_eq!(meta.last_reaped_count, 3);
    assert_eq!(meta.last_error.as_deref(), Some("boom"));
}

#[test]
fn migration_writes_state_backend_to_config_file() {
    let root = tempdir("config-write");
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    write_json_state(&state_dir);

    let config_path = root.join("config.yaml");
    fs::write(
        &config_path,
        r#"---
poll_interval_seconds: 120
state_dir: "STATE_DIR"
state_backend: "json"
worker_command: ["python3", "bridge.py"]
reduced_containment_acknowledged: true
"#
        .replace("STATE_DIR", &state_dir.to_string_lossy()),
    )
    .unwrap();

    migrate_to_sqlite(&state_dir, false, LockPolicy::Acquire, Some(&config_path))
        .expect("migration succeeds");

    let cfg = Config::load_from(&config_path).expect("config still loads");
    assert_eq!(cfg.state_backend, "sqlite");
}
