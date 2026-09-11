//! `caduceus review` — review observability CLI (issue #318, DAR §13).
//!
//! These tests drive the CLI as a subprocess via
//! `env!("CARGO_BIN_EXE_caduceus")` against BOTH state backends and
//! check:
//!
//! * `review status [<repo>]` — aggregate phase counts plus one line
//!   per entry (human) and the full §13 per-row field set (JSON).
//! * `review list` — the queue as a table (human) and full rows (JSON).
//! * `review show <repo> <pr>` — full detail + run history, with
//!   defensive `result_json` parsing (old schema versions never
//!   crash and are surfaced raw with a `parse_error`).
//! * Missing entry → `no_entry` diagnostic on the JSON path.
//! * Read-only: the subcommands never mutate the review stores.
//! * `$CADUCEUS_CONFIG` is honoured, including `state_backend: sqlite`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use chrono::Utc;

use caduceus::review::{
    ExecutionStatus, Finding, PublicationState, RepositoryId, Review, ReviewResult, ReviewState,
    ReviewTarget, Severity, Verdict, REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{ReviewHistoryRow, ReviewStore};

#[path = "../fixtures/mod.rs"]
mod fixtures;
use fixtures::tempdir;

const OWNER_A: &str = "Owner";
const REPO_A: &str = "Repo";
const PR_A: u64 = 42;
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OWNER_B: &str = "Other";
const REPO_B: &str = "Repo";
const PR_B: u64 = 7;
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OWNER_C: &str = "New";
const REPO_C: &str = "Repo";
const PR_C: u64 = 3;
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const SHA_OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_NEW: &str = "dddddddddddddddddddddddddddddddddddddddd";
const SHA_PENDING: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Json,
    Sqlite,
}

fn open_store(dir: &Path, backend: Backend) -> ReviewStore {
    match backend {
        Backend::Json => ReviewStore::open(dir).unwrap(),
        Backend::Sqlite => ReviewStore::open_sqlite(dir).unwrap(),
    }
}

fn repo(owner: &str, name: &str) -> RepositoryId {
    RepositoryId {
        owner: owner.to_string(),
        repo: name.to_string(),
    }
}

fn target(repository: &RepositoryId, pr: u64, sha: &str) -> ReviewTarget {
    ReviewTarget {
        repository: repository.clone(),
        pull_request: pr,
        head_sha: sha.to_string(),
        base_sha: "b".repeat(40),
        base_ref: "main".to_string(),
        merge_base: "m".repeat(40),
    }
}

/// A validator-correct `ReviewResult` document for the given verdict.
fn result_doc(verdict: Verdict) -> String {
    let findings = match verdict {
        Verdict::Fail => vec![Finding {
            severity: Severity::Blocking,
            title: "blocking".to_string(),
            body: "must fix before merge".to_string(),
            path: None,
            line: None,
            remediation: None,
        }],
        Verdict::Pass => vec![],
    };
    serde_json::to_string(&ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status: ExecutionStatus::Success,
        review: Some(Review {
            verdict,
            summary: format!("summary-{verdict:?}"),
            findings,
        }),
    })
    .unwrap()
}

/// An old-schema `result_json` document (v99): accepted by the store
/// as an opaque read-only blob, surfaced raw by the CLI.
fn old_schema_doc() -> String {
    serde_json::json!({
        "schema_version": 99,
        "status": "success",
        "review": { "verdict": "pass", "summary": "ancient", "findings": [] }
    })
    .to_string()
}

/// Seed one full run: enqueue, claim, complete, append history, and
/// persist the state row. The entry ends `Done` with a published
/// sticky comment.
fn seed_completed_run(
    store: &ReviewStore,
    repository: &RepositoryId,
    pr: u64,
    sha: &str,
    run_id: &str,
    verdict: Verdict,
    result_json: String,
) {
    store.enqueue_review(&target(repository, pr, sha)).unwrap();
    let claimed = store
        .acquire_next_review(run_id, std::process::id(), Utc::now())
        .unwrap()
        .unwrap();
    let generation = claimed.entry.review_generation;
    store.complete_review(claimed.claim.clone()).unwrap();
    store
        .append_history(ReviewHistoryRow {
            review_run_id: run_id.to_string(),
            repository: repository.clone(),
            pull_request: pr,
            head_sha: sha.to_string(),
            review_generation: generation,
            completed_at: Utc::now(),
            result_json,
        })
        .unwrap();
    store
        .save_review_state(&ReviewState {
            repository: repository.clone(),
            pull_request: pr,
            last_reviewed_head_sha: Some(sha.to_string()),
            last_verdict: Some(verdict),
            last_reviewed_at: Some(Utc::now()),
            sticky_comment_id: Some(pr),
            last_run_id: Some(run_id.to_string()),
            review_generation: generation,
            publication_state: PublicationState::Published,
            publication_attempt_count: 1,
            next_publish_at: None,
            last_publish_error: None,
        })
        .unwrap();
}

/// Standard three-entry seed: A (done, pass), B (done, fail), C
/// (queued). Entry order matters — `acquire_next_review` is FIFO, so
/// the completed runs are created in A/B order.
fn seed_standard(dir: &Path, backend: Backend) -> ReviewStore {
    let store = open_store(dir, backend);
    // A and B are enqueued first (FIFO claim order), C stays queued.
    seed_completed_run(
        &store,
        &repo(OWNER_A, REPO_A),
        PR_A,
        SHA_A,
        "RUN-A",
        Verdict::Pass,
        result_doc(Verdict::Pass),
    );
    seed_completed_run(
        &store,
        &repo(OWNER_B, REPO_B),
        PR_B,
        SHA_B,
        "RUN-B",
        Verdict::Fail,
        result_doc(Verdict::Fail),
    );
    store
        .enqueue_review(&target(&repo(OWNER_C, REPO_C), PR_C, SHA_C))
        .unwrap();
    store
}

/// Seed one entry whose history carries an old-schema `result_json`.
fn seed_old_history(dir: &Path, backend: Backend) -> ReviewStore {
    let store = open_store(dir, backend);
    seed_completed_run(
        &store,
        &repo("Legacy", "Repo"),
        5,
        &"d".repeat(40),
        "RUN-D",
        Verdict::Pass,
        old_schema_doc(),
    );
    store
}

/// One PR with three SHA entries (the #387 shape): OLD completed
/// FAIL (generation 1), NEW completed PASS (generation 2), PENDING
/// queued with no run (generation 3). The PR-level state ends with
/// `last_verdict = pass` (written by NEW's completion), so OLD's
/// own verdict (fail) disagrees with the PR's current verdict.
fn seed_multi_sha_pr(dir: &Path, backend: Backend) -> ReviewStore {
    let store = open_store(dir, backend);
    let repository = repo(OWNER_A, REPO_A);
    seed_completed_run(
        &store,
        &repository,
        PR_A,
        SHA_OLD,
        "RUN-OLD",
        Verdict::Fail,
        result_doc(Verdict::Fail),
    );
    seed_completed_run(
        &store,
        &repository,
        PR_A,
        SHA_NEW,
        "RUN-NEW",
        Verdict::Pass,
        result_doc(Verdict::Pass),
    );
    store
        .enqueue_review(&target(&repository, PR_A, SHA_PENDING))
        .unwrap();
    store
}

/// Run the CLI binary as a subprocess against `dir` with a
/// `$CADUCEUS_CONFIG` whose `state_dir` is `dir` (and
/// `state_backend: sqlite` for the SQLite backend).
fn run_cli(dir: &Path, backend: Backend, args: &[&str]) -> std::process::Output {
    let mut hermes_home = dir.to_path_buf();
    hermes_home.push("hermes");
    fs::create_dir_all(&hermes_home).unwrap();
    let config_path = dir.join("config.yaml");
    let backend_yaml = match backend {
        Backend::Json => "",
        Backend::Sqlite => "  state_backend: \"sqlite\"\n",
    };
    let yaml = format!(
        "caduceus:\n  state_dir: \"{}\"\n{backend_yaml}  worker_command:\n    - \"python3\"\n    - \"{}/bridge.py\"\n  reduced_containment_acknowledged: true\n",
        dir.display(),
        dir.display()
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

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "expected success; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn row_by_pr(entries: &serde_json::Value, pr: u64) -> serde_json::Value {
    entries
        .as_array()
        .expect("entries must be an array")
        .iter()
        .find(|row| row["pr"].as_u64() == Some(pr))
        .unwrap_or_else(|| panic!("no row for pr {pr} in {entries}"))
        .clone()
}

fn row_by_head_sha(entries: &serde_json::Value, head_sha: &str) -> serde_json::Value {
    entries
        .as_array()
        .expect("entries must be an array")
        .iter()
        .find(|row| row["head_sha"].as_str() == Some(head_sha))
        .unwrap_or_else(|| panic!("no row for head {head_sha} in {entries}"))
        .clone()
}

// ---------------------------------------------------------------------------
// Help surface
// ---------------------------------------------------------------------------

#[test]
fn help_lists_review_subcommands() {
    let dir = tempdir("review-help");
    let output = run_cli(&dir, Backend::Json, &["review", "--help"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["status", "list", "show"] {
        assert!(stdout.contains(needle), "missing {needle} in {stdout}");
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_human_shows_counts_and_entries() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-status-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(&dir, backend, &["review", "status"]);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("caduceus review status"), "got {stdout}");
        assert!(stdout.contains("review queue: 3 entries"), "got {stdout}");
        assert!(stdout.contains("phases:"), "got {stdout}");
        assert!(stdout.contains("queued: 1"), "got {stdout}");
        assert!(stdout.contains("done: 2"), "got {stdout}");
        // Per-entry lines carry the key presentation fields (repo in
        // its canonical lowercase form on both backends).
        assert!(
            stdout.contains("owner/repo#42@aaaaaaaaaaaa"),
            "got {stdout}"
        );
        assert!(
            stdout.contains("verdict=fail publication=published run=RUN-B"),
            "got {stdout}"
        );
    }
}

#[test]
fn status_json_emits_envelope_counts_and_full_rows() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-status-json-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(&dir, backend, &["review", "status", "--json"]);
        assert_success(&output);
        let envelope = parse_json(&output);
        assert_eq!(envelope["schema"], "review/1.0");
        assert_eq!(envelope["app_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(envelope["diagnostic"], serde_json::Value::Null);
        let counts = &envelope["payload"]["counts"];
        assert_eq!(counts["queued"], 1);
        assert_eq!(counts["done"], 2);
        assert_eq!(counts["failed"], 0);
        let entries = &envelope["payload"]["entries"];
        assert_eq!(entries.as_array().unwrap().len(), 3);

        // Done-pass row (A): every §13 field is present with the
        // derived execution status and state-joined verdict.
        let a = row_by_pr(entries, PR_A);
        assert_eq!(a["repo"], "owner/repo");
        assert_eq!(a["review_state"], "done");
        assert_eq!(a["run_id"], "RUN-A");
        assert_eq!(a["review_generation"], 1);
        assert_eq!(a["execution_attempts"], 0);
        assert_eq!(a["execution_status"], "success");
        assert_eq!(a["verdict"], "pass");
        assert_eq!(a["publication_state"], "published");
        assert_eq!(a["publication_attempt_count"], 1);
        assert!(a["reviewed_at"].is_string(), "reviewed_at missing: {a}");
        assert!(a["base_sha"].is_string(), "base_sha missing: {a}");
        assert!(a["head_sha"].is_string(), "head_sha missing: {a}");
        assert!(a["merge_base"].is_string(), "merge_base missing: {a}");

        // Done-fail row (B): execution is success, verdict is fail —
        // the two never conflate.
        let b = row_by_pr(entries, PR_B);
        assert_eq!(b["execution_status"], "success");
        assert_eq!(b["verdict"], "fail");
        assert_eq!(b["run_id"], "RUN-B");

        // Queued row (C): no run yet.
        let c = row_by_pr(entries, PR_C);
        assert_eq!(c["review_state"], "queued");
        assert_eq!(c["run_id"], serde_json::Value::Null);
        assert_eq!(c["execution_status"], serde_json::Value::Null);
        assert_eq!(c["verdict"], serde_json::Value::Null);
        assert_eq!(c["publication_state"], "pending");
    }
}

#[test]
fn status_filters_by_repo() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-status-filter-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(&dir, backend, &["review", "status", "Other/Repo", "--json"]);
        assert_success(&output);
        let envelope = parse_json(&output);
        let counts = &envelope["payload"]["counts"];
        assert_eq!(counts["queued"], 0, "filter must drop the queued row");
        assert_eq!(counts["done"], 1);
        let entries = &envelope["payload"]["entries"];
        assert_eq!(entries.as_array().unwrap().len(), 1);
        assert_eq!(entries[0]["pr"], PR_B);
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[test]
fn list_human_renders_table() {
    let dir = tempdir("review-list");
    seed_standard(&dir, Backend::Json);
    let output = run_cli(&dir, Backend::Json, &["review", "list"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for col in [
        "key",
        "phase",
        "attempts",
        "generation",
        "verdict",
        "publication",
        "run_id",
    ] {
        assert!(stdout.contains(col), "missing column {col:?} in {stdout}");
    }
    assert!(
        stdout.contains("owner/repo#42@aaaaaaaaaaaa"),
        "got {stdout}"
    );
    assert!(stdout.contains("new/repo#3@cccccccccccc"), "got {stdout}");
}

#[test]
fn list_json_emits_full_rows_for_every_entry() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-list-json-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(&dir, backend, &["review", "list", "--json"]);
        assert_success(&output);
        let envelope = parse_json(&output);
        assert_eq!(envelope["schema"], "review/1.0");
        assert_eq!(envelope["diagnostic"], serde_json::Value::Null);
        let entries = envelope["payload"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        // Every §13 field exists on each row.
        for row in entries {
            for field in [
                "repo",
                "pr",
                "base_sha",
                "head_sha",
                "merge_base",
                "review_state",
                "run_id",
                "review_generation",
                "execution_attempts",
                "execution_status",
                "verdict",
                "last_error",
                "reviewed_at",
                "publication_state",
                "publication_attempt_count",
                "next_publication_attempt",
            ] {
                assert!(row.get(field).is_some(), "missing {field} in {row}");
            }
        }
    }
}

#[test]
fn empty_store_lists_placeholder() {
    let dir = tempdir("review-empty");
    let output = run_cli(&dir, Backend::Json, &["review", "list"]);
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no entries"), "got {stdout}");

    let output = run_cli(&dir, Backend::Json, &["review", "list", "--json"]);
    assert_success(&output);
    let envelope = parse_json(&output);
    assert_eq!(envelope["payload"]["entries"], serde_json::json!([]));
}

// ---------------------------------------------------------------------------
// per-SHA verdict (issue #387)
// ---------------------------------------------------------------------------

#[test]
fn list_and_status_render_each_entrys_own_verdict() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-per-sha-verdict-{backend:?}"));
        seed_multi_sha_pr(&dir, backend);

        // JSON rows (list and status render the same ReviewRow).
        for subcommand in ["list", "status"] {
            let output = run_cli(&dir, backend, &["review", subcommand, "--json"]);
            assert_success(&output);
            let envelope = parse_json(&output);
            let entries = &envelope["payload"]["entries"];

            // OLD: its own run says FAIL even though the PR's current
            // verdict is pass — the #387 regression.
            let old = row_by_head_sha(entries, SHA_OLD);
            assert_eq!(old["review_generation"], 1);
            assert_eq!(old["execution_status"], "success");
            assert_eq!(
                old["verdict"], "fail",
                "superseded FAIL entry must render its own verdict, not the PR's current pass"
            );

            // NEW: the current run — pass, same as before.
            let new = row_by_head_sha(entries, SHA_NEW);
            assert_eq!(new["verdict"], "pass");

            // PENDING: no completed run, so it falls back to the
            // PR-level last verdict (issue-Expected fallback).
            let pending = row_by_head_sha(entries, SHA_PENDING);
            assert_eq!(pending["execution_status"], serde_json::Value::Null);
            assert_eq!(
                pending["verdict"], "pass",
                "entry with no run falls back to the PR last verdict"
            );
        }

        // Human renderers carry the same columns.
        let output = run_cli(&dir, backend, &["review", "list"]);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("owner/repo#42@aaaaaaaaaaaa\tdone\t0\t1\tfail\t"),
            "OLD list row must show its own fail verdict: {stdout}"
        );
        assert!(
            stdout.contains("owner/repo#42@eeeeeeeeeeee\tqueued\t0\t3\tpass\t"),
            "PENDING list row must fall back to the PR verdict: {stdout}"
        );

        let output = run_cli(&dir, backend, &["review", "status"]);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("owner/repo#42@aaaaaaaaaaaa  phase=done attempts=0 gen=1 verdict=fail"),
            "OLD status line must show its own fail verdict: {stdout}"
        );
        assert!(
            stdout
                .contains("owner/repo#42@eeeeeeeeeeee  phase=queued attempts=0 gen=3 verdict=pass"),
            "PENDING status line must fall back to the PR verdict: {stdout}"
        );
    }
}

#[test]
fn list_and_status_surface_unparsable_result_as_null_verdict() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-old-doc-verdict-{backend:?}"));
        seed_old_history(&dir, backend);

        for subcommand in ["list", "status"] {
            let output = run_cli(&dir, backend, &["review", subcommand, "--json"]);
            // No panic, exit 0 — the defensive-parse contract.
            assert_success(&output);
            let envelope = parse_json(&output);
            let entries = &envelope["payload"]["entries"];
            let row = row_by_head_sha(entries, &"d".repeat(40));
            // The entry HAS a run (an old-schema one): the verdict
            // surfaces the defensive-parse null — DAR §4.3, never
            // back-migrated, never the PR-level substitution (#387).
            assert_eq!(row["verdict"], serde_json::Value::Null);
            assert_eq!(row["execution_status"], serde_json::Value::Null);
        }

        // Human render: "-" in the verdict column, no crash.
        let output = run_cli(&dir, backend, &["review", "list"]);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("legacy/repo#5@dddddddddddd\tdone\t0\t1\t-\t"),
            "old-schema entry must render '-' for the verdict: {stdout}"
        );
    }
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

#[test]
fn show_human_prints_full_detail_and_history() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-show-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(&dir, backend, &["review", "show", "Other/Repo", "7"]);
        assert_success(&output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("entry other/repo#7@bbbbbbbbbbbb"),
            "got {stdout}"
        );
        for needle in [
            "base_sha:",
            "head_sha:",
            "merge_base:",
            "phase: done",
            "run_id: RUN-B",
            "generation: 1",
            "execution_status: success",
            "verdict: fail",
            "publication_state: published",
            "publication_attempt_count: 1",
            "RUN-B",
            "summary-Fail",
        ] {
            assert!(stdout.contains(needle), "missing {needle:?} in {stdout}");
        }
    }
}

#[test]
fn show_json_includes_history_with_parsed_fields() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-show-json-{backend:?}"));
        seed_standard(&dir, backend);
        let output = run_cli(
            &dir,
            backend,
            &["review", "show", "Other/Repo", "7", "--json"],
        );
        assert_success(&output);
        let envelope = parse_json(&output);
        assert_eq!(envelope["schema"], "review/1.0");
        let entry = &envelope["payload"]["entry"];
        assert_eq!(entry["repo"], "other/repo");
        assert_eq!(entry["pr"], 7);
        assert_eq!(entry["review_state"], "done");
        assert_eq!(entry["verdict"], "fail");
        let history = envelope["payload"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        let row = &history[0];
        assert_eq!(row["run_id"], "RUN-B");
        assert_eq!(row["status"], "success");
        assert_eq!(row["verdict"], "fail");
        assert_eq!(row["summary"], "summary-Fail");
        assert_eq!(row["parse_error"], serde_json::Value::Null);
        assert!(row["result_json"].is_string(), "raw document kept: {row}");
    }
}

#[test]
fn show_surfaces_old_schema_result_json_defensively() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let dir = tempdir(&format!("review-show-old-{backend:?}"));
        seed_old_history(&dir, backend);
        let output = run_cli(
            &dir,
            backend,
            &["review", "show", "Legacy/Repo", "5", "--json"],
        );
        assert_success(&output);
        let envelope = parse_json(&output);
        let history = envelope["payload"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        let row = &history[0];
        assert_eq!(row["status"], serde_json::Value::Null);
        assert_eq!(row["verdict"], serde_json::Value::Null);
        assert!(
            row["parse_error"]
                .as_str()
                .unwrap()
                .contains("unsupported schema_version 99"),
            "got {row}"
        );
        let raw_doc: serde_json::Value =
            serde_json::from_str(row["result_json"].as_str().unwrap()).unwrap();
        assert_eq!(raw_doc["schema_version"], 99, "raw doc kept: {row}");
        // The entry row itself still works (queue + state-joined
        // fields). The verdict column surfaces the defensive-parse
        // null because this entry HAS a run (an old-schema one) —
        // no PR-level substitution (issue #387).
        assert_eq!(
            envelope["payload"]["entry"]["verdict"],
            serde_json::Value::Null
        );
    }
}

#[test]
fn show_missing_entry_human_path_errors() {
    let dir = tempdir("review-show-missing");
    let output = run_cli(&dir, Backend::Json, &["review", "show", "Owner/Repo", "99"]);
    assert!(!output.status.success(), "expected failure");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("no entry"),
        "expected 'no entry'; got {combined:?}"
    );
}

#[test]
fn show_missing_entry_json_emits_no_entry_diagnostic() {
    let dir = tempdir("review-show-missing-json");
    let output = run_cli(
        &dir,
        Backend::Json,
        &["review", "show", "Owner/Repo", "99", "--json"],
    );
    assert!(!output.status.success(), "expected non-zero exit");
    let envelope = parse_json(&output);
    assert_eq!(envelope["schema"], "review/1.0");
    assert_eq!(envelope["diagnostic"], "no_entry");
    assert_eq!(envelope["payload"], serde_json::Value::Null);
}

// ---------------------------------------------------------------------------
// Read-only
// ---------------------------------------------------------------------------

#[test]
fn show_does_not_mutate_json_store() {
    let dir = tempdir("review-show-readonly");
    seed_standard(&dir, Backend::Json);
    let before = fs::read(dir.join("review_queue.json")).expect("read queue");
    let output = run_cli(
        &dir,
        Backend::Json,
        &["review", "show", "Owner/Repo", "42", "--json"],
    );
    assert_success(&output);
    let after = fs::read(dir.join("review_queue.json")).expect("read queue");
    assert_eq!(before, after, "show must not rewrite review_queue.json");
    let store = ReviewStore::open(&dir).unwrap();
    let snap = store.review_queue_snapshot().unwrap();
    assert_eq!(snap.entries.len(), 3, "entries must survive show");
}

#[test]
fn show_does_not_mutate_sqlite_store() {
    let dir = tempdir("review-show-readonly-sqlite");
    seed_standard(&dir, Backend::Sqlite);
    let output = run_cli(
        &dir,
        Backend::Sqlite,
        &["review", "show", "Owner/Repo", "42", "--json"],
    );
    assert_success(&output);
    let store = ReviewStore::open_sqlite(&dir).unwrap();
    let snap = store.review_queue_snapshot().unwrap();
    assert_eq!(snap.entries.len(), 3, "entries must survive show");
}
