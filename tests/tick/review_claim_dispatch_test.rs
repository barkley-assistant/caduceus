//! Claim-side review dispatch tests (issue #339, DAR §6.1–6.2, §8.1):
//! `run_review_claim` driven through the public `*_for_tests` seam
//! against a real local git remote, a wiremock GitHub, and a
//! closure-driven mock executor (the `Services::for_tests` precedent).
//!
//! Coverage per the plan's required-tests matrix:
//!
//! - happy pass → Done + history row + `review_passed`;
//! - happy fail-verdict → Done + history row + `review_failed_verdict`;
//! - TrustedHost result path (`<worktree>/worker-result.json`);
//! - OCI result path (`<state_dir>/oci-runs/<run_id>/output/...`),
//!   proving the daemon reads `ExecutorOutcome.result_path`
//!   exclusively (DAR §6.2);
//! - missing result → retry (Queued + attempts+1);
//! - invalid result → retry;
//! - control-file mutation → Terminal NeedsAttention with the
//!   worktree KEPT (DAR §10);
//! - `HeadShaUnavailable` → quiet skip;
//! - oversized diff → direct skip (deterministically unreviewable,
//!   never the retry path).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{AutoReviewConfig, Config};
use caduceus::daemon::tick::per_review::{
    REVIEW_EXECUTION_FAILED_EVENT, REVIEW_FAILED_VERDICT_EVENT, REVIEW_PASSED_EVENT,
    REVIEW_RETRY_SCHEDULED_EVENT, REVIEW_STARTED_EVENT, REVIEW_WORKER_COMPLETED_EVENT,
};
use caduceus::error::{CaduceusError, CaduceusResult};
use caduceus::executor::{Executor, ExecutorOutcome, ExecutorSpec};
use caduceus::github::{Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::meta::TickOutcome;
use caduceus::orchestration::{
    GitRunnerAdapter, GithubClientAdapter, ReviewRunGuard, Services, SystemClock,
};
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::state::review::{
    ClaimedReview, ReviewHistoryRow, ReviewPhase, ReviewQueueEntry, ReviewStore,
};
use caduceus::worker::prompt::PROMPT_FILENAME;
use caduceus::worker::supervisor::SupervisorOutcome;
use caduceus::worktree::GitRunner;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

// ---------------------------------------------------------------------------
// Git fixtures
// ---------------------------------------------------------------------------

/// Bare remote with `main` (A), `feature` (B → C). Returns
/// `(A, B, C)`; commits use empty trees (mirror of
/// review_discovery_test.rs).
fn init_bare_remote_with_feature(path: &Path) -> (String, String, String) {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(Command::new("git").arg("init").arg("--bare").arg(path));
    run(Command::new("git")
        .current_dir(path)
        .args(["symbolic-ref", "HEAD", "refs/heads/main"]));
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = |message: &str, parent: Option<&str>| -> String {
        let mut args = vec!["commit-tree".to_string(), tree.clone()];
        if let Some(p) = parent {
            args.push("-p".to_string());
            args.push(p.to_string());
        }
        args.push("-m".to_string());
        args.push(message.to_string());
        let output = Command::new("git")
            .current_dir(path)
            .args(&args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit_a = commit("base", None);
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/main", &commit_a]));
    let commit_b = commit("feature", Some(&commit_a));
    let commit_c = commit("feature tip", Some(&commit_b));
    run(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &commit_c,
    ]));
    (commit_a, commit_b, commit_c)
}

/// Add a child commit of `parent` whose tree carries one file of
/// `bytes` bytes, and point `refs/heads/feature` at it. Returns the
/// new OID. The resulting `git diff <merge_base> <head>` exceeds
/// `MAX_REVIEW_DIFF_BYTES` (1 MiB), so the prompt builder skips it.
fn add_big_blob_commit(path: &Path, parent: &str, bytes: usize) -> String {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let blob_file =
        std::env::temp_dir().join(format!("caduceus-bigblob-{}-{}", std::process::id(), bytes));
    let content = "a".repeat(bytes);
    std::fs::write(&blob_file, &content).expect("write big blob file");
    let blob = {
        let output = Command::new("git")
            .current_dir(path)
            .args([
                "hash-object",
                "-w",
                "-t",
                "blob",
                blob_file.to_str().expect("blob path is utf8"),
            ])
            .output()
            .expect("hash-object blob");
        assert!(
            output.status.success(),
            "hash-object blob failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let _ = std::fs::remove_file(&blob_file);
    let tree_input = format!("100644 blob {blob}\tbig.txt\n");
    let tree = {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .current_dir(path)
            .arg("mktree")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("mktree spawn");
        child
            .stdin
            .as_mut()
            .expect("mktree stdin")
            .write_all(tree_input.as_bytes())
            .expect("write mktree input");
        let output = child.wait_with_output().expect("mktree output");
        assert!(
            output.status.success(),
            "mktree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["commit-tree", &tree, "-p", parent, "-m", "big blob commit"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree big");
        assert!(
            output.status.success(),
            "commit-tree big failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/feature", &commit]));
    commit
}

// ---------------------------------------------------------------------------
// Mock executor (closure-driven, `Services::for_tests` seam)
// ---------------------------------------------------------------------------

struct MockExecutor<F> {
    f: F,
}

impl<F> Executor for MockExecutor<F>
where
    F: Fn(&ExecutorSpec) -> ExecutorOutcome + Send + Sync,
{
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> Pin<Box<dyn Future<Output = CaduceusResult<ExecutorOutcome>> + Send + 'a>> {
        let outcome = (self.f)(spec);
        Box::pin(async move { Ok(outcome) })
    }
}

fn ok_outcome(result_path: PathBuf) -> ExecutorOutcome {
    ExecutorOutcome {
        outcome: SupervisorOutcome {
            status: 0,
            signaled: false,
            timed_out: false,
            cancelled: false,
            disk_pressure: false,
        },
        result_path,
    }
}

// ---------------------------------------------------------------------------
// Claim fixture: real local remote + mirror + seeded review entry
// ---------------------------------------------------------------------------

struct ClaimFixture {
    _server: MockServer,
    cfg: Config,
    client: Arc<Client>,
    store: Arc<ReviewStore>,
    claimed: ClaimedReview,
    runner: GitRunner,
    pool: Arc<Pool>,
    remote_dir: PathBuf,
}

impl ClaimFixture {
    fn new_guard(&self) -> ReviewRunGuard {
        ReviewRunGuard::new(
            self.claimed.claim.clone(),
            Arc::clone(&self.store),
            self.cfg.state_dir.join("processor.log"),
            self.claimed.entry.target.clone(),
            self.runner.clone(),
        )
    }

    fn resolve_remote(&self) -> caduceus::daemon::tick::per_review::RemoteResolver {
        let url = format!("file://{}", self.remote_dir.display());
        Arc::new(move |_owner: &str, _repo: &str| Ok(url.clone()))
    }
}

async fn claim_fixture(
    label: &str,
    remote_dir: PathBuf,
    base_sha: String,
    head_sha: String,
    pr_row: serde_json::Value,
) -> ClaimFixture {
    let root = tempdir(label);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_row))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let mut cfg = Config::test_defaults(&root);
    cfg.api_base = server.uri();
    cfg.watched_repos = vec!["owner/r".to_string()];
    cfg.worker_command = vec!["/bin/true".to_string()];
    cfg.auto_review = Some(AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
    });
    cfg.git_timeout_seconds = 30;

    let cache = HttpCache::open(&cfg.state_dir).expect("http cache opens");
    let client = Arc::new(Client::with_cache(&cfg, cache).expect("client builds"));

    let store = Arc::new(ReviewStore::open(&cfg.state_dir).expect("review store opens"));
    store
        .enqueue_review(&ReviewTarget {
            repository: repo_id(),
            pull_request: 7,
            head_sha: head_sha.clone(),
            base_sha: base_sha.clone(),
            base_ref: "main".to_string(),
            merge_base: base_sha.clone(),
        })
        .expect("enqueue review target");
    let claimed = store
        .acquire_next_review("RUN-1", std::process::id(), chrono::Utc::now())
        .expect("acquire")
        .expect("eligible review entry");

    let runner = GitRunner::new(&cfg);
    let pool = Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    );

    ClaimFixture {
        _server: server,
        cfg,
        client,
        store,
        claimed,
        runner,
        pool,
        remote_dir,
    }
}

fn repo_id() -> RepositoryId {
    RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    }
}

fn pr_row_open(head_sha: &str) -> serde_json::Value {
    json!({
        "number": 7,
        "title": "Feature",
        "state": "open",
        "merged": false,
        "draft": false,
        "user": {"login": "author"},
        "base": {"ref": "main", "sha": "a", "repo": {"full_name": "owner/r"}},
        "head": {"ref": "feature", "sha": head_sha, "repo": {"full_name": "owner/r"}}
    })
}

fn queue_entry(store: &ReviewStore) -> ReviewQueueEntry {
    store
        .review_queue_snapshot()
        .expect("review queue snapshot")
        .entries
        .values()
        .find(|e| e.target.pull_request == 7)
        .expect("review entry for PR 7")
        .clone()
}

fn history_rows(store: &ReviewStore) -> Vec<ReviewHistoryRow> {
    store
        .history_for_pull_request(&repo_id(), 7)
        .expect("history rows")
}

fn result_json_pass() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "status": "success",
        "review": {
            "verdict": "pass",
            "summary": "Looks good",
            "findings": []
        }
    })
}

fn result_json_fail() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "status": "success",
        "review": {
            "verdict": "fail",
            "summary": "Blocking issue",
            "findings": [
                {
                    "severity": "blocking",
                    "title": "t",
                    "body": "b",
                    "path": null,
                    "line": null,
                    "remediation": null
                }
            ]
        }
    })
}

/// Run the claim seam against the fixture with the given executor.
async fn run_claim_for(
    fixture: &ClaimFixture,
    guard: &mut ReviewRunGuard,
    executor: Arc<dyn Executor>,
) -> CaduceusResult<TickOutcome> {
    let services = Services::for_tests(
        Arc::new(SystemClock),
        Arc::new(GithubClientAdapter::new(Arc::clone(&fixture.client))),
        Arc::new(GitRunnerAdapter::new(fixture.runner.clone())),
        executor,
        Arc::clone(&fixture.pool),
    );
    let admit = fixture
        .pool
        .admit("repo:owner/r", "owner/r")
        .await
        .expect("pool admits under cap");
    caduceus::daemon::tick::per_review::run_review_claim_for_tests(
        fixture.cfg.clone(),
        &services,
        Arc::clone(&fixture.client),
        fixture.store.as_ref(),
        fixture.claimed.clone(),
        guard,
        CancellationToken::new(),
        &mut None,
        admit,
        fixture.resolve_remote(),
    )
    .await
}

/// Build a fresh local remote and return its dir + `(base, mid, tip)`.
fn fresh_remote(label: &str) -> (PathBuf, String, String, String) {
    let root = tempdir(label);
    let remote_dir = root.join("remote.git");
    let (a, b, c) = init_bare_remote_with_feature(&remote_dir);
    (remote_dir, a, b, c)
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn happy_pass_completes_done_with_history() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-pass-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            // TrustedHost result path: `<worktree>/worker-result.json`.
            let result_path = spec.worktree.join("worker-result.json");
            std::fs::write(&result_path, result_json_pass().to_string()).expect("write result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-pass",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Done);
    assert_eq!(entry.attempts, 0);

    let rows = history_rows(&fixture.store);
    assert_eq!(
        rows.len(),
        1,
        "exactly one history row for the completed run"
    );
    let row = &rows[0];
    assert_eq!(row.head_sha, head_sha);
    assert_eq!(row.pull_request, 7);
    let doc: serde_json::Value = serde_json::from_str(&row.result_json).expect("result json");
    assert_eq!(doc["review"]["verdict"], "pass");
}

#[tokio::test]
#[serial_test::serial]
async fn happy_fail_verdict_completes_done_with_history() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-fail-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            let result_path = spec.worktree.join("worker-result.json");
            std::fs::write(&result_path, result_json_fail().to_string()).expect("write result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-fail",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(
        entry.phase,
        ReviewPhase::Done,
        "fail verdict is still Done (execution succeeded)"
    );

    let rows = history_rows(&fixture.store);
    assert_eq!(rows.len(), 1);
    let doc: serde_json::Value = serde_json::from_str(&rows[0].result_json).expect("result json");
    assert_eq!(doc["review"]["verdict"], "fail");
}

#[tokio::test]
#[serial_test::serial]
async fn oci_result_path_variant_reads_mode_correct_path() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-oci-git");
    let state_dir =
        std::env::temp_dir().join(format!("caduceus-oci-result-{}", std::process::id()));
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            // OCI result path: `<state_dir>/oci-runs/<run_id>/output/...`.
            let result_path = state_dir
                .join("oci-runs")
                .join(&spec.run_id)
                .join("output")
                .join("worker-result.json");
            if let Some(parent) = result_path.parent() {
                std::fs::create_dir_all(parent).expect("create oci output dir");
            }
            std::fs::write(&result_path, result_json_pass().to_string()).expect("write result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-oci",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Done);
    assert_eq!(history_rows(&fixture.store).len(), 1);
}

// ---------------------------------------------------------------------------
// Retry paths (DAR §8.1 Worker row)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn missing_result_retries_with_budget() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-missing-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |_spec: &ExecutorSpec| {
            // No result file is written: the daemon must treat a
            // missing result as an execution failure (DAR §6.2).
            ok_outcome(PathBuf::from("/dev/null/does-not-exist.result.json"))
        },
    });
    let fixture = claim_fixture(
        "claim-missing",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(entry.attempts, 1);
    assert!(entry.next_attempt_at.is_some());
    assert!(entry
        .last_error
        .as_deref()
        .unwrap_or_default()
        .contains("result"));
    assert_eq!(
        history_rows(&fixture.store).len(),
        0,
        "no history for a failed execution"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_result_retries_with_budget() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-invalid-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            let result_path = spec.worktree.join("worker-result.json");
            std::fs::write(&result_path, "not valid json").expect("write garbage result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-invalid",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(entry.attempts, 1);
    assert_eq!(history_rows(&fixture.store).len(), 0);
}

// ---------------------------------------------------------------------------
// Terminal mutation path (DAR §10)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn mutation_is_terminal_and_worktree_kept() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-mutation-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            // The worker tampers with a daemon control file: the
            // pre-run digest no longer matches (DAR §10.2).
            let prompt_path = spec.worktree.join(PROMPT_FILENAME);
            std::fs::write(&prompt_path, "tampered prompt").expect("tamper with prompt");
            ok_outcome(PathBuf::from("/dev/null/none"))
        },
    });
    let fixture = claim_fixture(
        "claim-mutation",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Failed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
    assert_eq!(entry.attempts, 0);
    assert_eq!(
        entry.blocked_source.as_deref(),
        Some("review/mutation_violation")
    );

    // Forensic evidence: the worktree survives (DAR §8.1 Terminal
    // row). Its path is `<storage>/worktrees/review/owner/r/<run_id>`.
    let storage = fixture
        .cfg
        .repo_storage_root
        .join("worktrees/review/owner/r");
    let survivors: Vec<PathBuf> = std::fs::read_dir(&storage)
        .expect("review worktree storage readable")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        survivors.len(),
        1,
        "exactly one preserved forensic worktree"
    );
}

// ---------------------------------------------------------------------------
// Quiet-skip paths (DAR §8.1 fourth route)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn head_sha_unavailable_is_quiet_skip() {
    // The PR row is open and reachable, but the seeded head SHA is
    // NOT in the mirror/remote (force-push + GC): the SHA-anchored
    // fetch inside `create_review` fails.
    let (remote_dir, base_sha, _mid, _tip) = fresh_remote("claim-head-sha-git");
    let fake_sha = "d".repeat(40);
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |_spec: &ExecutorSpec| unreachable!("executor must never run on a head-SHA skip"),
    });
    let fixture = claim_fixture(
        "claim-head-sha",
        remote_dir,
        base_sha,
        fake_sha.clone(),
        pr_row_open(&fake_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0, "skip must not burn the retry budget");
}

#[tokio::test]
#[serial_test::serial]
async fn oversized_diff_is_direct_skip() {
    // The remote's head commit carries a >1 MiB file, so the
    // merge-base diff exceeds the review budget: deterministically
    // unreviewable → direct skip, never the retry path.
    let (remote_dir, base_sha, _mid, tip) = fresh_remote("claim-oversized-git");
    let big_head = add_big_blob_commit(&remote_dir, &tip, 2 * 1024 * 1024);
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |_spec: &ExecutorSpec| {
            unreachable!("executor must never run on an oversized-diff skip")
        },
    });
    let fixture = claim_fixture(
        "claim-oversized",
        remote_dir,
        base_sha,
        big_head.clone(),
        pr_row_open(&big_head),
    )
    .await;
    let mut guard = fixture.new_guard();

    let outcome = run_claim_for(&fixture, &mut guard, mock)
        .await
        .expect("claim runs");
    assert_eq!(outcome, TickOutcome::Processed);

    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Skipped);
    assert_eq!(entry.attempts, 0);
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("oversized"),
        "skip reason must record the oversized diff"
    );
}

// ---------------------------------------------------------------------------
// DAR §13 dispatch-event capture (issue #318: per-transition emission
// tests). AC3's never-interchange is enforced at PATH level here: each
// capture asserts the transition's own event is present AND the other
// verdict/execution event is absent, so a future swap of the two emit
// sites cannot pass silently.
//
// EVERY test in this binary that runs `run_review_claim` is serial
// (the #167 finding): `tracing_core` caches callsite interest
// process-wide, and a sibling running the same event callsites without
// a subscriber installed would register them as never-enabled and
// silently drop the capture (same discipline as
// orchestration_active_run_inline_test.rs:723-735).
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn happy_pass_captures_dispatch_events() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-pass-event-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            let result_path = spec.worktree.join("worker-result.json");
            std::fs::write(&result_path, result_json_pass().to_string()).expect("write result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-pass-event",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let capture = fixture.cfg.state_dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        run_claim_for(&fixture, &mut guard, mock)
            .await
            .expect("claim runs")
    };
    drop(appender_guard);

    assert_eq!(outcome, TickOutcome::Processed);
    assert_eq!(queue_entry(&fixture.store).phase, ReviewPhase::Done);

    let body = std::fs::read_to_string(&capture).expect("read capture file");
    // The pass transition emits started → worker_completed → passed.
    for expected in [
        REVIEW_STARTED_EVENT,
        REVIEW_WORKER_COMPLETED_EVENT,
        REVIEW_PASSED_EVENT,
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    // AC3: the verdict and execution-failure events never leak into
    // the pass path.
    for absent in [REVIEW_FAILED_VERDICT_EVENT, REVIEW_EXECUTION_FAILED_EVENT] {
        assert!(!body.contains(absent), "unexpected {absent}: {body}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn happy_fail_verdict_captures_verdict_not_execution_failed() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-fail-event-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |spec: &ExecutorSpec| {
            let result_path = spec.worktree.join("worker-result.json");
            std::fs::write(&result_path, result_json_fail().to_string()).expect("write result");
            ok_outcome(result_path)
        },
    });
    let fixture = claim_fixture(
        "claim-fail-event",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let capture = fixture.cfg.state_dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        run_claim_for(&fixture, &mut guard, mock)
            .await
            .expect("claim runs")
    };
    drop(appender_guard);

    assert_eq!(outcome, TickOutcome::Processed);
    assert_eq!(queue_entry(&fixture.store).phase, ReviewPhase::Done);

    let body = std::fs::read_to_string(&capture).expect("read capture file");
    // The fail-verdict transition emits started → worker_completed →
    // failed_verdict (execution succeeded; only the verdict is Fail).
    for expected in [
        REVIEW_STARTED_EVENT,
        REVIEW_WORKER_COMPLETED_EVENT,
        REVIEW_FAILED_VERDICT_EVENT,
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    // AC3: `review_failed_verdict` must NEVER be interchangeable with
    // `review_execution_failed` — a valid run that fails the code is
    // not an execution failure.
    assert!(
        !body.contains(REVIEW_EXECUTION_FAILED_EVENT),
        "execution-failed leaked into the fail-verdict path: {body}"
    );
    assert!(
        !body.contains(REVIEW_PASSED_EVENT),
        "pass leaked into the fail-verdict path: {body}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn execution_failed_and_retry_scheduled_capture_on_retry() {
    let (remote_dir, base_sha, _mid, head_sha) = fresh_remote("claim-retry-event-git");
    let mock: Arc<dyn Executor> = Arc::new(MockExecutor {
        f: move |_spec: &ExecutorSpec| {
            // No result file: an execution failure through the retry
            // budget (DAR §6.2), NOT a failed verdict.
            ok_outcome(PathBuf::from("/dev/null/does-not-exist.result.json"))
        },
    });
    let fixture = claim_fixture(
        "claim-retry-event",
        remote_dir,
        base_sha,
        head_sha.clone(),
        pr_row_open(&head_sha),
    )
    .await;
    let mut guard = fixture.new_guard();

    let capture = fixture.cfg.state_dir.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        run_claim_for(&fixture, &mut guard, mock)
            .await
            .expect("claim runs")
    };
    drop(appender_guard);

    assert_eq!(outcome, TickOutcome::Processed);
    let entry = queue_entry(&fixture.store);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(entry.attempts, 1);

    let body = std::fs::read_to_string(&capture).expect("read capture file");
    // The retry route emits started → execution_failed →
    // retry_scheduled.
    for expected in [
        REVIEW_STARTED_EVENT,
        REVIEW_EXECUTION_FAILED_EVENT,
        REVIEW_RETRY_SCHEDULED_EVENT,
    ] {
        assert!(body.contains(expected), "missing {expected}: {body}");
    }
    // AC3: an execution failure is NOT a failed verdict, and no
    // terminal verdict was produced.
    assert!(
        !body.contains(REVIEW_FAILED_VERDICT_EVENT),
        "failed-verdict leaked into the execution-failed path: {body}"
    );
    assert!(
        !body.contains(REVIEW_PASSED_EVENT),
        "pass leaked into the execution-failed path: {body}"
    );
}

#[allow(dead_code)]
fn _unused(_: &CaduceusError) {}
