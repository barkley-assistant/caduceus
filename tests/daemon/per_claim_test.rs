//! Regression tests for issue #118 — the per-claim tick must
//! trust a parseable `worker-result.json` over the bridge's
//! non-zero exit code.
//!
//! Acceptance checks (LOOP-01..04): see
//! https://github.com/barkley-assistant/caduceus/issues/118
//!
//! The tests spawn the real `caduceus` binary against a
//! wiremock GitHub surface and a disposable `git daemon` origin
//! on 127.0.0.1, mirroring the harness in
//! `tests/integration/release_canary_test.rs`. Each test seeds a
//! `Queued` entry and a stub worker shell script that either
//! writes a valid `worker-result.json` then `exit 1` (LOOP-01/02)
//! or just `exit 1` (LOOP-03), and asserts on the resulting
//! `StateStore` snapshot.
//!
//! LOOP-05 lives in `tests/daemon/status_test.rs` because the
//! `caduceus status --json` subprocess harness is already wired
//! up there.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use caduceus::issue::IssueKey;
use caduceus::queue::{
    serialize_queue_state, FinalizationCheckpoint, FinalizationStage, Phase, QueueEntry,
    QueueState, TicketType, QUEUE_FILE_VERSION,
};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::{clone_main, run_with_timeout, GitDaemon, MockGitHub};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const TICK_TIMEOUT: Duration = Duration::from_secs(60);
const OWNER: &str = "owner";
const REPO: &str = "repo";
const ISSUE_NUMBER: u64 = 86;
const CODE_LABEL: &str = "autofix";
const EXPECTED_PR_NUMBER: u64 = 117;

// ---------------------------------------------------------------------------
// Harness: stub worker script.
// ---------------------------------------------------------------------------

/// Write a worker script that writes a fix, writes a valid
/// success `worker-result.json`, then `exit 1`. This is the LOOP-01/02
/// repro: a bridge that produced real output but exited non-zero.
fn write_worker_exit1_with_success(dir: &Path) -> PathBuf {
    let path = dir.join("worker-loop-success.sh");
    let body = "#!/bin/sh\n\
        set -e\n\
        echo \"loop repro fix\" > ./fix.txt\n\
        cat > ./worker-result.json <<'EOF'\n\
        {\"status\":\"success\",\"summary\":\"loop repro 86\",\"commit_message\":\"fix: repro 86\",\"pull_request_title\":\"Repro issue 86\",\"investigation\":false}\n\
        EOF\n\
        exit 1\n";
    write_executable(&path, body);
    path
}

/// Write a worker script that writes a `worker-result.json` whose
/// own `status` is `failure`, then `exit 1`. The daemon must treat
/// the declared failure as a worker-attributable failure and retry,
/// regardless of the result file being parseable (worker contract:
/// `status: "failure"` means "record the failure and retry").
fn write_worker_exit1_with_failure(dir: &Path) -> PathBuf {
    let path = dir.join("worker-loop-failure.sh");
    let body = "#!/bin/sh\n\
        set -e\n\
        cat > ./worker-result.json <<'EOF'\n\
        {\"status\":\"failure\",\"summary\":\"worker could not complete the task\",\"commit_message\":\"fix: repro 86\",\"pull_request_title\":\"Repro issue 86\",\"investigation\":false}\n\
        EOF\n\
        exit 1\n";
    write_executable(&path, body);
    path
}

/// Write a worker script that just `exit 1` with no result file —
/// the LOOP-03 original "worker failed mid-run" case.
fn write_worker_exit1_no_result(dir: &Path) -> PathBuf {
    let path = dir.join("worker-loop-no-result.sh");
    let body = "#!/bin/sh\nexit 1\n";
    write_executable(&path, body);
    path
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write worker.sh");
    let mut mode = fs::metadata(path).expect("stat worker").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).expect("chmod worker");
}

// ---------------------------------------------------------------------------
// Harness: config + seeded queue.
// ---------------------------------------------------------------------------

fn write_config(
    config_path: &Path,
    api_base: &str,
    state_dir: &Path,
    workdir_base: &Path,
    worker: &Path,
    max_retries: u32,
) {
    let mut yaml = String::new();
    yaml.push_str("caduceus:\n");
    yaml.push_str(&format!("  state_dir: \"{}\"\n", state_dir.display()));
    yaml.push_str(&format!(
        "  log_path: \"{}/processor.log\"\n",
        state_dir.display()
    ));
    yaml.push_str(&format!("  api_base: \"{}\"\n", api_base));
    yaml.push_str("  github_token: \"ghp_loop_token_value\"\n");
    yaml.push_str("  poll_interval_seconds: 1\n");
    yaml.push_str(&format!("  workdir_base: \"{}\"\n", workdir_base.display()));
    yaml.push_str(&format!("  watched_repos:\n    - \"{}/{}\"\n", OWNER, REPO));
    yaml.push_str(&format!(
        "  worker_command:\n    - \"{}\"\n",
        worker.display()
    ));
    yaml.push_str(&format!("  ticket_label_code: \"{}\"\n", CODE_LABEL));
    yaml.push_str("  ticket_label_investigation: \"autofix-investigate\"\n");
    yaml.push_str("  dry_run: false\n");
    yaml.push_str("  reduced_containment_acknowledged: true\n");
    yaml.push_str(&format!("  max_retries_per_issue: {max_retries}\n"));
    yaml.push_str("  retry_backoff_seconds: 1\n");
    fs::write(config_path, yaml).expect("write loop config");
}

/// Build a `Queued` code-ticket entry, optionally with a pre-existing
/// `FinalizationCheckpoint` (the issue #118 observed end-state).
fn build_entry(finalization: Option<FinalizationCheckpoint>) -> QueueEntry {
    QueueEntry {
        key: IssueKey {
            owner: OWNER.to_string(),
            repo: REPO.to_string(),
            number: ISSUE_NUMBER,
        },
        phase: Phase::Queued,
        ticket_type: TicketType::Code,
        attempts: 0,
        last_error: None,
        last_run_id: None,
        next_attempt_at: None,
        finalization,
        queued_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        blocked_source: None,
        blocked_recovery_hint: None,
        generation: 1,
    }
}

fn seed_state(state_dir: &Path, entry: QueueEntry) {
    let mut entries = BTreeMap::new();
    entries.insert(entry.key.display_key(), entry);
    let body = serialize_queue_state(&QueueState {
        version: QUEUE_FILE_VERSION,
        entries,
    })
    .expect("serialize queue");
    fs::write(state_dir.join("state.json"), body).expect("write state.json");
}

fn pr_created_checkpoint(state_dir: &Path) -> FinalizationCheckpoint {
    FinalizationCheckpoint {
        run_id: "RUN-PREV".to_string(),
        branch_name: "automation/issue-86-run-prev".to_string(),
        result_path: state_dir.join("runs").join("RUN-PREV.result.json"),
        stage: FinalizationStage::PrCreated,
        commit_oid: Some("abc123".to_string()),
        pr_number: Some(EXPECTED_PR_NUMBER),
        pr_url: Some(format!(
            "https://github.com/{OWNER}/{REPO}/pull/{EXPECTED_PR_NUMBER}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Harness: GitHub surface (matches the daemon's per-claim calls).
// ---------------------------------------------------------------------------

async fn mount_github_surface(gh: &MockGitHub) {
    gh.mount_status(
        "GET",
        "/repos/owner/repo/issues",
        200,
        serde_json::json!([]),
    )
    .await;
    let issue_detail = serde_json::json!({
        "number": ISSUE_NUMBER,
        "title": "docs: update architecture.md and state-recovery.md",
        "body": "Reproducible #86 body",
        "labels": [{"name": CODE_LABEL}],
        "state": "open",
        "user": {"login": "octocat"},
        "updated_at": "2026-07-29T00:00:00Z",
    });
    gh.mount_status(
        "GET",
        &format!("/repos/owner/repo/issues/{ISSUE_NUMBER}"),
        200,
        issue_detail,
    )
    .await;
    gh.mount_status(
        "GET",
        &format!("/repos/owner/repo/issues/{ISSUE_NUMBER}/comments"),
        200,
        serde_json::json!([]),
    )
    .await;
    gh.mount_status(
        "GET",
        &format!("/repos/owner/repo/issues/{ISSUE_NUMBER}/events"),
        200,
        serde_json::json!([]),
    )
    .await;
    gh.mount_status("GET", "/repos/owner/repo/pulls", 200, serde_json::json!([]))
        .await;
    let pr_create = serde_json::json!({
        "number": EXPECTED_PR_NUMBER,
        "html_url": format!("https://github.com/{OWNER}/{REPO}/pull/{EXPECTED_PR_NUMBER}"),
    });
    gh.mount_status("POST", "/repos/owner/repo/pulls", 201, pr_create)
        .await;
    gh.mount_status(
        "POST",
        &format!("/repos/owner/repo/issues/{ISSUE_NUMBER}/comments"),
        201,
        serde_json::json!({ "id": 99 }),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Shared driver: spin up the fixtures + run one caduceus tick.
// ---------------------------------------------------------------------------

struct LoopHarness {
    _root: TempDir,
    state_dir: PathBuf,
    _workdir_base: PathBuf,
    config_path: PathBuf,
    gh: MockGitHub,
    _origin: GitDaemon,
}

async fn harness(label: &str, worker: &Path, max_retries: u32) -> LoopHarness {
    let gh = MockGitHub::start().await;
    let api_base = gh.uri();
    let origin = GitDaemon::start(label, OWNER, REPO);
    let origin_uri = origin.uri();
    let root = TempDir::with_prefix(format!("caduceus-loop-{label}-")).expect("root tempdir");
    let state_dir = root.path().join("state");
    let workdir_base = root.path().join("wd");
    fs::create_dir_all(&state_dir).expect("mkdir state_dir");
    fs::create_dir_all(&workdir_base).expect("mkdir workdir_base");
    clone_main(&workdir_base, &origin_uri, OWNER, REPO);
    let config_path = state_dir.join("config.yaml");
    write_config(
        &config_path,
        &api_base,
        &state_dir,
        &workdir_base,
        worker,
        max_retries,
    );
    mount_github_surface(&gh).await;
    LoopHarness {
        _root: root,
        state_dir,
        _workdir_base: workdir_base,
        config_path,
        gh,
        _origin: origin,
    }
}

fn run_one_tick(h: &LoopHarness, label: &str) -> (i32, String, String) {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_caduceus"));
    run_with_timeout(
        Command::new(&bin)
            .env("CADUCEUS_CONFIG", &h.config_path)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        TICK_TIMEOUT,
        label,
    )
}

fn entry_key() -> String {
    format!("{OWNER}/{REPO}#{ISSUE_NUMBER}")
}

fn load_entry(h: &LoopHarness) -> QueueEntry {
    let store = caduceus::queue::StateStore::open(&h.state_dir).expect("open store");
    let snap = store.snapshot().expect("snapshot");
    let key = caduceus::issue::IssueKey::parse(&entry_key()).expect("parse key");
    snap.entry(&key)
        .unwrap_or_else(|| panic!("entry {} missing", entry_key()))
        .clone()
}

// ---------------------------------------------------------------------------
// LOOP-01: exit-1 with a parseable success result advances the
// entry to AwaitingReview, NOT Queued.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop01_exit1_with_success_result_advances_to_awaiting_review() {
    let worker_dir = TempDir::with_prefix("caduceus-loop01-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_success(worker_dir.path());
    let h = harness("loop01", &worker, 3).await;
    seed_state(&h.state_dir, build_entry(None));

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop01)");
    assert_eq!(
        code, 0,
        "loop01: tick must exit 0; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::AwaitingReview,
        "LOOP-01: phase must be AwaitingReview; got {:?}\nlast_error={:?}",
        entry.phase,
        entry.last_error
    );
    assert!(
        entry.last_error.is_none()
            || !entry
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("worker exited 1"),
        "LOOP-01: last_error must not mention worker exit; got {:?}",
        entry.last_error
    );
    assert_eq!(
        entry.attempts, 0,
        "LOOP-01: attempts must not increment; got {}",
        entry.attempts
    );
    // The daemon must persist the host-side result path it actually
    // read (TrustedHost: `<worktree>/worker-result.json`) into the
    // finalization checkpoint so resume never re-derives it.
    let fin = entry
        .finalization
        .as_ref()
        .expect("LOOP-01: finalization checkpoint must exist at AwaitingReview");
    assert!(
        fin.result_path
            .file_name()
            .map(|n| n == "worker-result.json")
            .unwrap_or(false),
        "LOOP-01: checkpoint result_path must be the host worker-result.json; got {:?}",
        fin.result_path
    );
    let fin = entry.finalization.expect("finalization present");
    assert_eq!(fin.stage, FinalizationStage::PrCreated);
    assert_eq!(fin.pr_number, Some(EXPECTED_PR_NUMBER));

    // External side effects: exactly one PR creation and one completion
    // comment — no duplicate dispatch despite the worker's exit 1.
    assert_eq!(
        count_pr_posts(&h),
        1,
        "LOOP-01: exactly one PR creation POST expected"
    );
    assert_eq!(
        count_comment_posts(&h),
        1,
        "LOOP-01: exactly one completion comment POST expected"
    );
}

// ---------------------------------------------------------------------------
// LOOP-01b: exit-1 with a parseable worker-result.json whose own
// `status` is `failure` is a worker-attributable failure — it must
// route through the retry budget, NOT finalize. This pins the worker
// contract (`prompt.rs`: "status: failure means the daemon should
// record the failure and retry").
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop01b_exit1_with_failure_result_routes_through_retry_budget() {
    let worker_dir = TempDir::with_prefix("caduceus-loop01b-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_failure(worker_dir.path());
    let h = harness("loop01b", &worker, 2).await;
    seed_state(&h.state_dir, build_entry(None));

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop01b)");
    assert_eq!(
        code, 0,
        "loop01b: tick must exit 0; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::Queued,
        "LOOP-01b: phase must be Queued (declared failure, retry budget); got {:?}\nlast_error={:?}",
        entry.phase,
        entry.last_error
    );
    assert_eq!(
        entry.attempts, 1,
        "LOOP-01b: attempts must increment to 1; got {}",
        entry.attempts
    );
    let last_error = entry.last_error.expect("last_error present");
    assert!(
        last_error.contains("failure"),
        "LOOP-01b: last_error must record the declared failure; got {last_error:?}"
    );
    assert!(
        entry.finalization.is_none(),
        "LOOP-01b: no finalization checkpoint may be written for a failed result"
    );
}

// ---------------------------------------------------------------------------
// LOOP-02: a worker that exits 1 with a success result on an
// entry that ALREADY has a pr_created checkpoint does not
// re-dispatch on the next tick; phase stays terminal.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop02_exit1_with_existing_checkpoint_stays_terminal_no_redispatch() {
    let worker_dir = TempDir::with_prefix("caduceus-loop02-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_success(worker_dir.path());
    let h = harness("loop02", &worker, 3).await;
    seed_state(
        &h.state_dir,
        build_entry(Some(pr_created_checkpoint(&h.state_dir))),
    );

    let started = Instant::now();
    let pr_posts_before = count_pr_posts(&h);

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop02 tick1)");
    assert_eq!(
        code, 0,
        "loop02 tick1: exit 0 expected; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // The durable PrCreated checkpoint with PR #117 must short-circuit
    // to AwaitingReview WITHOUT dispatching the worker or opening a
    // new PR. This is the core issue #118 recovery guarantee.
    let pr_posts_after_tick1 = count_pr_posts(&h);
    assert_eq!(
        pr_posts_after_tick1, pr_posts_before,
        "LOOP-02 tick1: must not open any PR; \
         POSTs to /pulls went {pr_posts_before} -> {pr_posts_after_tick1}"
    );

    let entry_after_tick1 = load_entry(&h);
    assert_eq!(
        entry_after_tick1.phase,
        Phase::AwaitingReview,
        "LOOP-02 tick1: phase must reconcile to AwaitingReview; got {:?}\nlast_error={:?}",
        entry_after_tick1.phase,
        entry_after_tick1.last_error
    );
    assert!(
        entry_after_tick1
            .last_error
            .as_deref()
            .unwrap_or("")
            .is_empty()
            || !entry_after_tick1
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("worker exited"),
        "LOOP-02 tick1: last_error must not mention worker exit; got {:?}",
        entry_after_tick1.last_error
    );
    // The durable checkpoint must be preserved with the original PR.
    let fin = entry_after_tick1
        .finalization
        .as_ref()
        .expect("finalization preserved");
    assert_eq!(fin.stage, FinalizationStage::PrCreated);
    assert_eq!(fin.pr_number, Some(EXPECTED_PR_NUMBER));

    // Second tick: should be a no-op for this entry — phase is
    // terminal, no fresh dispatch, no new PR.
    let (code2, stdout2, stderr2) = run_one_tick(&h, "caduceus run (loop02 tick2)");
    assert_eq!(
        code2, 0,
        "loop02 tick2: exit 0 expected; got {code2}\n--- stdout ---\n{stdout2}\n--- stderr ---\n{stderr2}"
    );

    let entry_after_tick2 = load_entry(&h);
    assert_eq!(
        entry_after_tick2.phase,
        Phase::AwaitingReview,
        "LOOP-02 tick2: phase must stay AwaitingReview; got {:?}",
        entry_after_tick2.phase
    );

    let pr_posts_after_tick2 = count_pr_posts(&h);
    assert_eq!(
        pr_posts_after_tick2, pr_posts_after_tick1,
        "LOOP-02: second tick must not open a new PR; \
         POSTs to /pulls went {pr_posts_after_tick1} -> {pr_posts_after_tick2}"
    );

    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "LOOP-04: full reproduction must finish < 30s; took {elapsed:?}"
    );
}

// A queued entry with a durable Done checkpoint is inconsistent, but
// reconciliation must preserve the terminal checkpoint rather than
// downgrading it to AwaitingReview.
#[tokio::test]
async fn done_checkpoint_is_not_downgraded_to_awaiting_review() {
    let worker_dir = TempDir::with_prefix("caduceus-done-checkpoint-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_success(worker_dir.path());
    let h = harness("done-checkpoint", &worker, 3).await;
    let mut checkpoint = pr_created_checkpoint(&h.state_dir);
    checkpoint.stage = FinalizationStage::Done;
    seed_state(&h.state_dir, build_entry(Some(checkpoint)));

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (done checkpoint)");
    assert_eq!(
        code, 0,
        "done checkpoint: tick must exit 0; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::Done,
        "Done checkpoint must remain terminal; got {:?}",
        entry.phase
    );
    assert_eq!(
        count_pr_posts(&h),
        0,
        "Done reconciliation must not create a PR"
    );
}

// ---------------------------------------------------------------------------
// LOOP-03: exit-1 without a worker-result.json still routes
// through handle_infra_or_retry and consumes the retry budget.
// (Regression guard so the reorder doesn't break the original
// worker-failed-mid-run path.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop03_exit1_no_result_routes_through_retry_budget() {
    let worker_dir = TempDir::with_prefix("caduceus-loop03-worker-").expect("worker dir");
    let worker = write_worker_exit1_no_result(worker_dir.path());
    // Budget of 2 so the first failure stays under budget (Queued).
    let h = harness("loop03", &worker, 2).await;
    seed_state(&h.state_dir, build_entry(None));

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop03)");
    assert_eq!(
        code, 0,
        "loop03: tick must exit 0 (Processed); got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::Queued,
        "LOOP-03: phase must be Queued (under-budget retry); got {:?}",
        entry.phase
    );
    assert_eq!(
        entry.attempts, 1,
        "LOOP-03: attempts must increment to 1; got {}",
        entry.attempts
    );
    let last_error = entry.last_error.expect("last_error present");
    assert!(
        last_error.contains("worker exited 1"),
        "LOOP-03: last_error must mention exit code; got {last_error:?}"
    );
}

// ---------------------------------------------------------------------------
// LOOP-04: the issue #86 reproduction completes in < 30s.
// (Covered by the timed assertion inside loop02; this test exists
// as the explicit, named acceptance check so a future refactor
// can't quietly drop the budget guarantee.)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop04_issue86_reproduction_completes_under_30s() {
    let worker_dir = TempDir::with_prefix("caduceus-loop04-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_success(worker_dir.path());
    let h = harness("loop04", &worker, 3).await;
    // Seed the exact observed end-state from issue #118: Queued
    // entry with a pr_created finalization checkpoint and a nulled
    // last_run_id.
    seed_state(
        &h.state_dir,
        build_entry(Some(pr_created_checkpoint(&h.state_dir))),
    );

    // The <30s budget (issue #118 LOOP-04) covers the tick latency —
    // the reconciliation path that runs once the harness is set up.
    // Harness construction (git-daemon spawn, clone, wiremock) is
    // excluded; the meaningful budget is the per-claim tick itself.
    let started = Instant::now();

    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop04)");
    assert_eq!(
        code, 0,
        "loop04: tick must exit 0; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::AwaitingReview,
        "LOOP-04: phase must reconcile to AwaitingReview; got {:?}",
        entry.phase
    );

    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "LOOP-04: reproduction must finish < 30s; took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// LOOP-05 (issue #118): `caduceus status --json` against the state
// produced by a real LOOP-01 run (worker exits 1 after writing a
// success result) must show `phase: awaiting_review`, NOT `phase:
// queued` with `last_error: "worker exited N..."`. This runs the
// actual daemon, then invokes the CLI status surface.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn loop05_status_json_after_exit1_success_shows_awaiting_review() {
    let worker_dir = TempDir::with_prefix("caduceus-loop05-worker-").expect("worker dir");
    let worker = write_worker_exit1_with_success(worker_dir.path());
    let h = harness("loop05", &worker, 3).await;
    seed_state(&h.state_dir, build_entry(None));

    // Run the daemon: worker exits 1 after writing a success result;
    // the per-claim tick must advance the entry to AwaitingReview.
    let (code, stdout, stderr) = run_one_tick(&h, "caduceus run (loop05 setup)");
    assert_eq!(
        code, 0,
        "loop05 setup: tick must exit 0; got {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let entry = load_entry(&h);
    assert_eq!(
        entry.phase,
        Phase::AwaitingReview,
        "LOOP-05 precondition: entry must be AwaitingReview after the run"
    );

    // Now invoke `caduceus status --json` against the resulting state.
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_caduceus"));
    let output = Command::new(&bin)
        .env("CADUCEUS_CONFIG", &h.config_path)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn caduceus status --json");
    assert!(
        output.status.success(),
        "LOOP-05: caduceus status --json must exit 0; got {:?}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid json on stdout");
    let phases = json["report"]["phases"]
        .as_object()
        .expect("report.phases object present");
    let awaiting = phases
        .get("awaiting_review")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let queued = phases.get("queued").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(
        awaiting, 1,
        "LOOP-05: phases.awaiting_review must be 1; got {awaiting}\nfull json: {json}"
    );
    assert_eq!(
        queued, 0,
        "LOOP-05: phases.queued must be 0 (no re-dispatch loop); got {queued}\nfull json: {json}"
    );
    // The stale worker-exit error must not surface.
    let recent_errors = json["report"]["recent_errors"]
        .as_array()
        .expect("recent_errors array present");
    for err in recent_errors {
        let s = err.as_str().unwrap_or("");
        assert!(
            !s.contains("worker exited"),
            "LOOP-05: recent_errors must not mention worker exit; found {s:?}\nfull json: {json}"
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn count_pr_posts(h: &LoopHarness) -> usize {
    h.gh.received_requests()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/repos/owner/repo/pulls")
        .count()
}

fn count_comment_posts(h: &LoopHarness) -> usize {
    h.gh.received_requests()
        .into_iter()
        .filter(|r| {
            r.method.as_str() == "POST"
                && r.url.path() == format!("/repos/owner/repo/issues/{ISSUE_NUMBER}/comments")
        })
        .count()
}
