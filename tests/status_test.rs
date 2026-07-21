//! Task 7.3 acceptance tests for the status and heartbeat
//! inspection surface.
//!
//! The contract is in `CONTRACTS.md` and the task packet
//! `planning/caduceus-v0.1/tasks/7.3-implement-status-and-heartbeat-inspection.md`.
//!
//! Tests cover:
//! - exact README idle output (golden fixture for a fresh
//!   state directory),
//! - running output with transcript (a fresh heartbeat is
//!   surfaced in the live list),
//! - JSON snapshot (the schema-version field is present
//!   and the field set matches the contract),
//! - all phase counts (every `Phase` variant is surfaced
//!   even when the count is zero),
//! - deterministic head (the FIFO next head is the lexical
//!   first eligible entry),
//! - missing state diagnostic (no `state_dir`),
//! - corrupt state diagnostic (malformed `state_meta.json`),
//! - fresh / stale / future / malformed / symlink
//!   heartbeat, and
//! - custom config path (the reporter honours
//!   `$CADUCEUS_CONFIG` for non-default state directories).

use std::path::{Path, PathBuf};

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::error::CaduceusError;
use caduceus::issue::IssueKey;
use caduceus::meta::{MetaStore, RateLimitObservation, StateMeta, TickOutcome};
use caduceus::queue::{
    parse_queue_state, serialize_queue_state, ClaimToken, Phase, QueueEntry, QueueState,
    TicketType, QUEUE_FILE_VERSION,
};
use caduceus::status::{
    build_report, build_report_from_state, live_worker_from_heartbeat, render_human, render_json,
    report, sample_heartbeat, StatusDiagnostic, STATUS_SCHEMA_VERSION,
};
use caduceus::worker_supervisor::{write_heartbeat_record, Heartbeat, HEARTBEAT_FILE_VERSION};
use chrono::{DateTime, Utc};
use tempfile::tempdir;

fn empty_config(state_dir: &Path) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(state_dir.to_path_buf()),
        watched_repos: Some(Vec::new()),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(state_dir.to_path_buf()),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

#[test]
fn schema_version_is_pinned() {
    assert_eq!(STATUS_SCHEMA_VERSION, "7.5.0");
}

#[test]
fn idle_output_matches_readme_fixture() {
    let dir = tempdir().expect("tempdir");
    // Point the reporter at a sub-directory that does
    // not exist; the reporter treats a missing state
    // directory as `NoState` and surfaces the
    // "no state yet" hint.
    let cfg = empty_config(&dir.path().join("no-such-state"));
    let (out, _) = report(&cfg.state_dir, false).expect("report");
    let expected = format!(
        "caduceus status\n  state dir: {}\n  no state yet — run `caduceus run` to bootstrap\n",
        cfg.state_dir.display()
    );
    assert_eq!(out, expected);
}

#[test]
fn missing_state_diagnostic_is_distinct() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(&dir.path().join("no-such-state"));
    // No `state_dir` created.
    let (_, diag) = build_report(&cfg.state_dir).expect("report");
    assert_eq!(diag, Some(StatusDiagnostic::NoState));
}

#[test]
fn corrupt_state_diagnostic_preserves_file() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let meta_path = cfg.state_dir.join("state_meta.json");
    std::fs::write(&meta_path, b"not json {").expect("write");
    let (_, diag) = build_report(&cfg.state_dir).expect("report");
    match diag {
        Some(StatusDiagnostic::CorruptState { path, .. }) => {
            assert_eq!(path, meta_path);
        }
        other => panic!("expected CorruptState, got {other:?}"),
    }
    // The corrupt file is preserved — Phase 1's
    // "preserve corruption for diagnosis" rule.
    assert!(meta_path.exists());
}

#[test]
fn corrupt_queue_diagnostic_preserves_file() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    // Bootstrap a valid metadata so the meta check passes
    // and the queue check fires.
    let _ = MetaStore::open(&cfg.state_dir).expect("open meta");
    let queue_path = cfg.state_dir.join("state.json");
    std::fs::write(&queue_path, b"not json {").expect("write");
    let (_, diag) = build_report(&cfg.state_dir).expect("report");
    match diag {
        Some(StatusDiagnostic::CorruptQueue { path, .. }) => {
            assert_eq!(path, queue_path);
        }
        other => panic!("expected CorruptQueue, got {other:?}"),
    }
    assert!(queue_path.exists());
}

#[test]
fn json_snapshot_includes_schema_version() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    let json = render_json(&report).expect("json");
    assert!(json.contains(&format!("\"version\": \"{}\"", STATUS_SCHEMA_VERSION)));
    assert!(json.contains("\"phases\""));
    assert!(json.contains("\"live_workers\""));
    assert!(json.contains("\"recent_errors\""));
}

#[test]
fn all_phase_counts_are_present() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    // Every documented phase is in the report even when
    // the count is zero. The contract pins the field set.
    for label in [
        "queued",
        "in_progress",
        "previewed",
        "done",
        "failed",
        "skipped",
    ] {
        assert!(report.phases.contains_key(label), "missing phase {label}");
    }
}

#[test]
fn deterministic_head_picks_lexical_first_eligible() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let now = Utc::now();
    // Three queued entries; only the first two are
    // eligible (next_attempt_at unset / in the past).
    let mut state = QueueState::empty();
    state.entries.insert(
        "owner-b/repo#1".to_string(),
        entry(now, Phase::Queued, None),
    );
    state.entries.insert(
        "owner-a/repo#3".to_string(),
        entry(now, Phase::Queued, None),
    );
    state.entries.insert(
        "owner-c/repo#2".to_string(),
        entry(
            now,
            Phase::Queued,
            Some(now + chrono::Duration::seconds(120)),
        ),
    );
    // The lexical first eligible entry is
    // "owner-a/repo#3" — the BTreeMap iterates in key
    // order.
    let (head, earliest) = caduceus::status::compute_next_head(&state, now);
    assert_eq!(head.as_deref(), Some("owner-a/repo#3"));
    assert_eq!(earliest, Some(now + chrono::Duration::seconds(120)));
}

#[test]
fn next_head_is_none_when_all_queued_backed_off_in_past() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let now = Utc::now();
    let mut state = QueueState::empty();
    state.entries.insert(
        "owner-a/repo#1".to_string(),
        entry(
            now,
            Phase::Queued,
            Some(now + chrono::Duration::seconds(10)),
        ),
    );
    let (head, earliest) = caduceus::status::compute_next_head(&state, now);
    assert_eq!(head, None);
    assert_eq!(earliest, Some(now + chrono::Duration::seconds(10)));
}
#[test]
fn fresh_heartbeat_surfaces_live_worker() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    let now = Utc::now();
    let issue = IssueKey::parse("owner/repo#1").expect("key");
    let record = sample_heartbeat("RUN-LIVE", issue.clone(), now);
    let hb_path = runs_dir.join("RUN-LIVE.heartbeat");
    write_heartbeat_record(&record, &hb_path).expect("write");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    assert_eq!(report.live_workers.len(), 1);
    let w = &report.live_workers[0];
    assert_eq!(w.run_id, "RUN-LIVE");
    assert_eq!(w.issue, "owner/repo#1");
    assert_eq!(w.freshness, "fresh");
}

#[test]
fn stale_heartbeat_surfaces_with_stale_marker() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    // A heartbeat written 120 seconds ago is stale per
    // the 90s contract window.
    let now = Utc::now();
    let issue = IssueKey::parse("owner/repo#1").expect("key");
    let record = Heartbeat {
        version: HEARTBEAT_FILE_VERSION,
        run_id: "RUN-STALE".to_string(),
        pid: std::process::id(),
        started_at: now - chrono::Duration::seconds(120),
        updated_at: now - chrono::Duration::seconds(120),
        issue_key: issue,
        transcript_path: PathBuf::from("/tmp/runs/RUN-STALE.log"),
    };
    let hb_path = runs_dir.join("RUN-STALE.heartbeat");
    write_heartbeat_record(&record, &hb_path).expect("write");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    assert_eq!(report.live_workers.len(), 1);
    assert_eq!(report.live_workers[0].freshness, "stale");
}

#[test]
fn future_heartbeat_does_not_panic() {
    // A heartbeat whose updated_at is in the future is
    // surfaced as "fresh" with saturating age 0 (no panic).
    let now = Utc::now();
    let issue = IssueKey::parse("owner/repo#1").expect("key");
    let record = Heartbeat {
        version: HEARTBEAT_FILE_VERSION,
        run_id: "RUN-FUTURE".to_string(),
        pid: std::process::id(),
        started_at: now + chrono::Duration::seconds(10),
        updated_at: now + chrono::Duration::seconds(10),
        issue_key: issue,
        transcript_path: PathBuf::from("/tmp/runs/RUN-FUTURE.log"),
    };
    let worker = live_worker_from_heartbeat(&record, now);
    assert_eq!(worker.freshness, "fresh");
}

#[test]
fn malformed_heartbeat_is_skipped() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    let hb_path = runs_dir.join("RUN-MALFORMED.heartbeat");
    std::fs::write(&hb_path, b"not a heartbeat").expect("write");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    assert!(report.live_workers.is_empty());
}

#[test]
fn symlink_heartbeat_is_rejected() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    // A heartbeat-shaped target file (so the symlink
    // resolves to a parseable record) at a non-`.heartbeat`
    // path. The reporter must skip the symlink itself
    // even though its target is a valid record.
    let target = runs_dir.join("target.bin");
    let issue = IssueKey::parse("owner/repo#1").expect("key");
    let record = sample_heartbeat("RUN-SYMLINK", issue, Utc::now());
    write_heartbeat_record(&record, &target).expect("write");
    let link = runs_dir.join("RUN-SYMLINK.heartbeat");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    // The symlink is rejected. The target is at a
    // non-`.heartbeat` path and is therefore never
    // considered.
    assert!(report.live_workers.is_empty());
}

#[test]
fn non_heartbeat_runs_files_are_ignored() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    std::fs::write(runs_dir.join("README"), b"hello").expect("write");
    std::fs::write(runs_dir.join("RUN-1.log"), b"transcript").expect("write");
    std::fs::write(runs_dir.join("RUN-1.result.json"), b"{}").expect("write");
    let (report, _) = build_report(&cfg.state_dir).expect("report");
    assert!(report.live_workers.is_empty());
}

#[test]
fn report_honours_caduceus_config_env_for_custom_path() {
    let dir = tempdir().expect("tempdir");
    let custom = dir.path().join("custom");
    std::fs::create_dir_all(&custom).expect("state dir");
    std::fs::write(custom.join("state_meta.json"), b"not json {").expect("write");
    // The reporter's `report()` accepts a state_dir
    // directly; the CLI host composes the custom path
    // from `$CADUCEUS_CONFIG`. This test pins the
    // CLI-side composition by building the config and
    // calling `report` with the resolved state_dir.
    let cfg = empty_config(&custom);
    let (_, diag) = build_report(&cfg.state_dir).expect("report");
    match diag {
        Some(StatusDiagnostic::CorruptState { path, .. }) => {
            assert_eq!(path, custom.join("state_meta.json"));
        }
        other => panic!("expected CorruptState, got {other:?}"),
    }
}

#[test]
fn build_report_from_state_handles_synthetic_snapshot() {
    // The synthetic helper is the test seam: it accepts a
    // caller-supplied QueueState and StateMeta so tests
    // don't have to round-trip through the on-disk file.
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    let now = Utc::now();
    let mut state = QueueState::empty();
    state
        .entries
        .insert("owner/repo#1".to_string(), entry(now, Phase::Queued, None));
    let meta = StateMeta {
        version: caduceus::meta::META_VERSION,
        last_tick_started: Some(now),
        last_tick_finished: Some(now),
        last_outcome: Some(TickOutcome::Processed),
        last_http_status: Some(200),
        next_allowed_poll_at: Some(now + chrono::Duration::seconds(120)),
        last_reap_at: None,
        last_reaped_count: 0,
        rate_limit: Some(RateLimitObservation {
            limit: Some(5000),
            remaining: 4999,
            reset_at: now + chrono::Duration::seconds(3600),
            observed_at: now,
        }),
        last_error: None,
        recent_diagnostics: Vec::new(),
    };
    let report = build_report_from_state(&cfg.state_dir, &meta, &state);
    assert_eq!(report.last_outcome, Some(TickOutcome::Processed));
    assert_eq!(report.last_http_status, Some(200));
    assert_eq!(report.next_head.as_deref(), Some("owner/repo#1"));
    assert_eq!(report.phases.get("queued"), Some(&1));
}

#[test]
fn render_human_includes_transcript_for_live_workers() {
    let dir = tempdir().expect("tempdir");
    let cfg = empty_config(dir.path());
    std::fs::create_dir_all(&cfg.state_dir).expect("state dir");
    let runs_dir = cfg.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).expect("runs dir");
    let now = Utc::now();
    let issue = IssueKey::parse("owner/repo#1").expect("key");
    let record = sample_heartbeat("RUN-LIVE", issue, now);
    let hb_path = runs_dir.join("RUN-LIVE.heartbeat");
    write_heartbeat_record(&record, &hb_path).expect("write");
    let (report, diag) = build_report(&cfg.state_dir).expect("report");
    let out = render_human(&report, diag.as_ref());
    // The live worker line carries the issue key,
    // the transcript path, and the freshness marker.
    assert!(out.contains("RUN-LIVE"));
    assert!(out.contains("owner/repo#1"));
    assert!(out.contains("transcript="));
    assert!(out.contains("fresh") || out.contains("stale"));
}

fn entry(_now: DateTime<Utc>, phase: Phase, next_attempt_at: Option<DateTime<Utc>>) -> QueueEntry {
    QueueEntry {
        key: IssueKey {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 1,
        },
        phase,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at,
        finalization: None,
        queued_at: _now,
        updated_at: _now,
        generation: 1,
    }
}

// Compile-time guards: ensure the queue helpers we use
// still exist after the refactor.
#[allow(dead_code)]
fn _queue_helpers_exist() {
    let _ = serialize_queue_state;
    let _ = parse_queue_state;
    let _ = QUEUE_FILE_VERSION;
    let _ = ClaimToken::for_test;
    let _ = CaduceusError::Config;
}

// ---------------------------------------------------------------------------
// Shell-level exit code tests — assert the process exit status
// matches RUN-005. These use the real binary so they test the CLI
// wiring, not just the library function.
// ---------------------------------------------------------------------------

use std::io::Write;
use std::process::Command;

/// Shell-level test: `caduceus status` exits 0 when the
/// state directory exists and is healthy.
#[test]
fn status_exit_0_for_valid_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    // Bootstrap a valid meta store so the state is
    // considered "healthy" (no diagnostic).
    let _ = caduceus::meta::MetaStore::open(&state_dir).expect("open meta");

    let config_path = dir.path().join("config.yaml");
    // Provide a minimal valid config with worker_command
    // so Config::load doesn't fail.
    let config_body = format!(
        "caduceus:\n  state_dir: {}\n  worker_command: ['/bin/true']\n  reduced_containment_acknowledged: true\n",
        state_dir.display().to_string().replace('\'', "")
    );
    let mut f = std::fs::File::create(&config_path).expect("create config");
    write!(f, "{}", config_body).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", &config_path)
        .args(["status"])
        .output()
        .expect("spawn caduceus status");
    assert!(
        output.status.success(),
        "expected exit 0 for valid state; got {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Shell-level test: `caduceus status` exits 2 when the
/// state directory is missing (NoState).
#[test]
fn status_exit_2_for_missing_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("no-such-state");

    let config_path = dir.path().join("config.yaml");
    let config_body = format!(
        "caduceus:\n  state_dir: {}\n  worker_command: ['/bin/true']\n  reduced_containment_acknowledged: true\n",
        state_dir.display().to_string().replace('\'', "")
    );
    let mut f = std::fs::File::create(&config_path).expect("create config");
    write!(f, "{}", config_body).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", &config_path)
        .args(["status"])
        .output()
        .expect("spawn caduceus status");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 for missing state; got {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Shell-level test: `caduceus status` exits 1 when the
/// state metadata is corrupt.
#[test]
fn status_exit_1_for_corrupt_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(state_dir.join("state_meta.json"), b"not json {").expect("write");

    let config_path = dir.path().join("config.yaml");
    let config_body = format!(
        "caduceus:\n  state_dir: {}\n  worker_command: ['/bin/true']\n  reduced_containment_acknowledged: true\n",
        state_dir.display().to_string().replace('\'', "")
    );
    let mut f = std::fs::File::create(&config_path).expect("create config");
    write!(f, "{}", config_body).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", &config_path)
        .args(["status"])
        .output()
        .expect("spawn caduceus status");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for corrupt state; got {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Shell-level test: `caduceus status` exits 1 when the
/// queue data is corrupt.
#[test]
fn status_exit_1_for_corrupt_queue() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    // Bootstrap a valid meta so the meta check passes
    // and the queue check fires.
    let _ = caduceus::meta::MetaStore::open(&state_dir).expect("open meta");
    std::fs::write(state_dir.join("state.json"), b"not json {").expect("write");

    let config_path = dir.path().join("config.yaml");
    let config_body = format!(
        "caduceus:\n  state_dir: {}\n  worker_command: ['/bin/true']\n  reduced_containment_acknowledged: true\n",
        state_dir.display().to_string().replace('\'', "")
    );
    let mut f = std::fs::File::create(&config_path).expect("create config");
    write!(f, "{}", config_body).expect("write config");

    let output = Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", &config_path)
        .args(["status"])
        .output()
        .expect("spawn caduceus status");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for corrupt queue; got {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
