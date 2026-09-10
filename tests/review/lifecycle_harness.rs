//! Shared harness for the E2E PR lifecycle test (issue #322, DAR §15
//! "End-to-end" row).
//!
//! No-stub harness (plan D1): a wiremock fake GitHub API, a real local
//! bare git remote, the real daemon dispatch/finalizer/store code, and
//! the REAL `review_harness.py` worker running as a real subprocess
//! under the REAL `caduceus __worker-supervisor` supervision protocol.
//!
//! The only deviation from `Services::production` is the executor's
//! `self_exe`: `run_review_claim` pins `self_exe = current_exe()`,
//! which in a test binary is the libtest harness (it does not
//! implement the hidden `__worker-supervisor` command). This harness
//! therefore drives the real `supervise()` production function with
//! `ReleaseBinary::locate()` (the real `caduceus` binary) as
//! `self_exe` — the same binary production re-execs — so the
//! supervisor subprocess, the sanitized worker env, and the
//! `review_harness.py` subprocess all run for real.
//!
//! `FAKE_REVIEW_RESULT` is delivered to the harness via the worker
//! command argv (`env FAKE_REVIEW_RESULT=<kind> python3 ...`): the
//! production supervisor builds the worker env with an EMPTY
//! allowlist (`src/main.rs` `run_supervisor_mode`), so a
//! `worker_env_allowlist` entry would be dropped by `env_clear()`.
//! The `env` argv is not a stub — `review_harness.py` still runs as a
//! real subprocess and still asserts the real prompt's §1–§6 order
//! and schema version before writing the real result file.

// Shared harness module: different consumers use different subsets
// of the helpers (the same pattern as `tests/fixtures/git_daemon.rs`).
#![allow(dead_code)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{AutoReviewConfig, Config};
use caduceus::executor::{Executor, ExecutorOutcome, ExecutorSpec};
use caduceus::github::{Client, HttpCache};
use caduceus::meta::TickOutcome;
use caduceus::orchestration::{
    GitRunnerAdapter, GithubClientAdapter, ReviewRunGuard, Services, SystemClock,
};
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::state::review::ReviewStore;
use caduceus::worktree::GitRunner;
use tokio_util::sync::CancellationToken;

use crate::fixtures::{tempdir, MockGitHub, ReleaseBinary};

/// The PR number every wire row uses (mirrors the #327 fixtures).
pub const PR: u64 = 7;
/// The sticky comment id the mock returns for a successful create.
pub const STICKY_COMMENT_ID: u64 = 4242;

/// State backend the harness opens.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Json,
    Sqlite,
}

impl Backend {
    fn label(self, name: &str) -> String {
        match self {
            Backend::Json => format!("{name}-json"),
            Backend::Sqlite => format!("{name}-sqlite"),
        }
    }

    fn state_backend(self) -> &'static str {
        match self {
            Backend::Json => "json",
            Backend::Sqlite => "sqlite",
        }
    }
}

/// Bare remote with `main` (A), `feature` (B → C). Returns
/// `(A, B, C)`; commits use empty trees (mirror of
/// `review_discovery_test.rs`). B and C are both reachable from
/// `refs/heads/feature`, so the SHA-anchored fetches inside admission
/// and `ReviewWorktree::create_review` succeed against the `file://`
/// remote.
fn init_bare_remote_with_feature(path: &std::path::Path) -> (String, String, String) {
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

/// The fork remote (issue #337 Phase 2): a bare clone of the base
/// remote. It can serve the fork PR's head SHA (the fork's objects)
/// while the base repo stays the trusted origin the quarantine
/// clones from.
fn fork_remote_fixture(base_dir: &std::path::Path, fork_dir: &std::path::Path) {
    let output = Command::new("git")
        .args(["clone", "--bare"])
        .arg(base_dir)
        .arg(fork_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("git clone fork remote");
    assert!(
        output.status.success(),
        "git clone fork remote failed ({}); stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A `/pulls` row (also served as the single-PR fetch row) whose
/// SHAs are the REAL commits of the local remote — never the literal
/// fixture SHAs (`bbbb2222…`, `cccc3333…`), which do not exist in the
/// remote (plan R3).
fn pr_wire_row(base_sha: &str, head_sha: &str) -> serde_json::Value {
    serde_json::json!({
        "number": PR,
        "title": "E2E lifecycle PR",
        "body": "Lifecycle fixture for issue #322.",
        "state": "open",
        "draft": false,
        "merged": false,
        "merged_at": null,
        "user": { "login": "octocat" },
        "base": { "ref": "main", "sha": base_sha, "repo": { "full_name": "owner/r" } },
        "head": { "ref": "feature-x", "sha": head_sha, "repo": { "full_name": "owner/r" } }
    })
}

/// A FORK `/pulls` row (issue #337 Phase 2): the base repo is the
/// trusted `owner/r`; the head repo is the attacker-controlled
/// `forkuser/r` fork carrying the PR head SHA.
fn fork_pr_wire_row(base_sha: &str, head_sha: &str) -> serde_json::Value {
    serde_json::json!({
        "number": PR,
        "title": "fork PR",
        "body": "Fork lifecycle fixture for issue #337.",
        "state": "open",
        "draft": false,
        "merged": false,
        "merged_at": null,
        "user": { "login": "octocat" },
        "base": { "ref": "main", "sha": base_sha, "repo": { "full_name": "owner/r" } },
        "head": { "ref": "feature-x", "sha": head_sha, "repo": { "full_name": "forkuser/r" } }
    })
}

/// Executor that runs the REAL `supervise()` production supervisor
/// with the REAL `caduceus` binary as `self_exe` (see module docs).
struct SupervisorExecutor {
    self_exe: PathBuf,
    cfg: Config,
}

impl Executor for SupervisorExecutor {
    fn run<'a>(
        &'a self,
        spec: &'a ExecutorSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = caduceus::error::CaduceusResult<ExecutorOutcome>>
                + Send
                + 'a,
        >,
    > {
        let self_exe = self.self_exe.clone();
        let cfg = self.cfg.clone();
        let spec = spec.clone();
        Box::pin(async move {
            let outcome = caduceus::worker::supervisor::supervise(
                &self_exe,
                &cfg,
                &spec.target,
                &spec.worktree,
                &spec.run_id,
                &spec.context_json,
                &spec.worker_command,
                spec.cancellation.clone(),
            )
            .await?;
            Ok(ExecutorOutcome {
                outcome,
                result_path: spec
                    .worktree
                    .join(caduceus::worker::worker_contract::WORKER_RESULT_FILE),
            })
        })
    }
}

/// The E2E lifecycle harness: wiremock GitHub + real local bare
/// remote + real store + real supervisor-backed executor, per
/// backend.
pub struct LifecycleHarness {
    /// Scratch root (tempdir) — kept for the harness lifetime.
    pub _root: PathBuf,
    pub gh: MockGitHub,
    pub cfg: Config,
    pub store: Arc<ReviewStore>,
    pub client: Arc<Client>,
    pub runner: GitRunner,
    pub services: Services,
    pub remote_dir: PathBuf,
    /// The fork remote (issue #337 Phase 2): a bare clone of the base
    /// remote serving the fork PR head SHA under `forkuser/r`.
    pub fork_dir: PathBuf,
    /// Real fixture SHAs: base (A, on main), mid (B, feature~1 — the
    /// FAIL revision head), tip (C, the feature tip — the PASS
    /// revision head).
    pub base_sha: String,
    pub mid_sha: String,
    pub tip_sha: String,
    pub harness_py: PathBuf,
}

impl LifecycleHarness {
    /// Build the harness for one backend.
    pub async fn start(label: &str, backend: Backend) -> Self {
        let root = tempdir(&backend.label(label));
        let gh = MockGitHub::start().await;

        let remote_dir = root.join("origin.git");
        let (base_sha, mid_sha, tip_sha) = init_bare_remote_with_feature(&remote_dir);

        // The fork remote (issue #337 Phase 2): a bare clone of the
        // base so it can serve the fork PR head SHA, while the base
        // repo stays the trusted origin the quarantine clones from.
        let fork_dir = root.join("fork.git");
        fork_remote_fixture(&remote_dir, &fork_dir);

        let harness_py = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/review_harness.py");
        assert!(
            harness_py.is_file(),
            "review harness must exist at {}",
            harness_py.display()
        );

        let mut cfg = Config::test_defaults(&root);
        cfg.api_base = gh.uri();
        cfg.watched_repos = vec!["owner/r".to_string()];
        cfg.auto_review = Some(AutoReviewConfig {
            enabled: true,
            draft_pull_requests: false,
            rerun_command: "/caduceus review".to_string(),
            fork_policy: None,
        });
        cfg.state_backend = backend.state_backend().to_string();
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;
        // Documented intent only: the real supervisor builds the
        // worker env with an empty allowlist (main.rs), so the harness
        // actually receives FAKE_REVIEW_RESULT via the worker argv
        // (see `worker_cfg`).
        cfg.worker_env_allowlist = vec!["FAKE_REVIEW_RESULT".to_string()];
        cfg.worker_command = worker_command("pass", &harness_py);

        let store = Arc::new(match backend {
            Backend::Json => ReviewStore::open(&cfg.state_dir).expect("review store opens (json)"),
            Backend::Sqlite => {
                ReviewStore::open_sqlite(&cfg.state_dir).expect("review store opens (sqlite)")
            }
        });

        let cache = HttpCache::open(&cfg.state_dir).expect("http cache opens");
        let client = Arc::new(Client::with_cache(&cfg, cache).expect("client builds"));

        let runner = GitRunner::new(&cfg);
        let pool = Arc::new(
            Pool::new(
                cfg.worker_parallelism,
                DrainConfig::from_seconds_and_ms(
                    cfg.drain_timeout_seconds,
                    cfg.backpressure_budget_ms,
                ),
            )
            .with_lease_store_dir(
                cfg.state_dir.clone(),
                std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
            ),
        );

        let self_exe = ReleaseBinary::locate();
        assert!(
            self_exe.is_file(),
            "the real caduceus binary must be built (cargo build --bins) \
             for the lifecycle test — ReleaseBinary::locate() returned {}",
            self_exe.display()
        );
        let executor = Arc::new(SupervisorExecutor {
            self_exe,
            cfg: cfg.clone(),
        });
        let services = Services::for_tests(
            Arc::new(SystemClock),
            Arc::new(GithubClientAdapter::new(Arc::clone(&client))),
            Arc::new(GitRunnerAdapter::new(runner.clone())),
            executor,
            Arc::clone(&pool),
        );

        Self {
            _root: root,
            gh,
            cfg,
            store,
            client,
            runner,
            services,
            remote_dir,
            fork_dir,
            base_sha,
            mid_sha,
            tip_sha,
            harness_py,
        }
    }

    /// A config clone whose worker command selects the harness's
    /// scripted result via the worker argv (`env FAKE_REVIEW_RESULT=<kind>`).
    pub fn worker_cfg(&self, kind: &str) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.worker_command = worker_command(kind, &self.harness_py);
        cfg
    }

    /// The `file://` remote URL every resolver returns.
    pub fn remote_url(&self) -> String {
        format!("file://{}", self.remote_dir.display())
    }

    /// The fork remote's `file://` URL (issue #337 Phase 2) — the
    /// URL the fork-URL resolver returns for `forkuser/r`.
    pub fn fork_url(&self) -> String {
        format!("file://{}", self.fork_dir.display())
    }

    // --- wiremock mounts ---------------------------------------------------

    /// Mount `/repos/owner/r/pulls` to serve one open PR whose head is
    /// `head_sha` (re-mounting with a new head simulates the force
    /// push; wiremock serves the most recently mounted match).
    pub async fn mount_pulls(&self, head_sha: &str) {
        self.gh
            .mount(
                "GET",
                "/repos/owner/r/pulls",
                serde_json::json!([pr_wire_row(&self.base_sha, head_sha)]),
            )
            .await;
    }

    /// Mount `/repos/owner/r/pulls` to serve one open FORK PR (head
    /// repo `forkuser/r`, issue #337 Phase 2).
    pub async fn mount_pulls_fork(&self, head_sha: &str) {
        self.gh
            .mount(
                "GET",
                "/repos/owner/r/pulls",
                serde_json::json!([fork_pr_wire_row(&self.base_sha, head_sha)]),
            )
            .await;
    }

    /// Mount the fork-URL resolver endpoint (`GET /repos/forkuser/r`)
    /// the tick's quarantine seam uses to map the fork's `full_name`
    /// to its `clone_url`.
    pub async fn mount_fork_remote_lookup(&self, fork_url: &str) {
        self.gh
            .mount(
                "GET",
                "/repos/forkuser/r",
                serde_json::json!({ "clone_url": fork_url }),
            )
            .await;
    }

    /// Re-mount `/pulls` with a NEW head at wiremock priority `1` so
    /// it beats the original default-priority mount (same matcher;
    /// wiremock falls back to insertion order only when priorities
    /// tie).
    pub async fn mount_pulls_revision(&self, head_sha: &str) {
        self.gh
            .mount_status_priority(
                "GET",
                "/repos/owner/r/pulls",
                200,
                1,
                serde_json::json!([pr_wire_row(&self.base_sha, head_sha)]),
            )
            .await;
    }

    /// Mount the single-PR fetch (`GET /pulls/7`) + the PR discussion
    /// page (`GET /issues/7/comments`), both required by
    /// `run_review_claim`.
    pub async fn mount_pr_fetch_and_discussion(&self) {
        self.gh
            .mount(
                "GET",
                &format!("/repos/owner/r/pulls/{PR}"),
                pr_wire_row(&self.base_sha, &self.mid_sha),
            )
            .await;
        self.gh
            .mount_paged(
                &format!("/repos/owner/r/issues/{PR}/comments"),
                vec![serde_json::json!([])],
            )
            .await;
    }

    /// Mount the sticky-comment create endpoint (`POST
    /// /issues/{pr}/comments`) with `status` and the returned comment
    /// id. `500` exercises the publish-failure resume path.
    pub async fn mount_comment_create(&self, status: u16, id: u64) {
        self.gh
            .mount_status(
                "POST",
                &format!("/repos/owner/r/issues/{PR}/comments"),
                status,
                serde_json::json!({ "id": id, "body": "" }),
            )
            .await;
    }

    /// Re-mount the sticky-comment create endpoint at wiremock
    /// priority `1` so a recovering endpoint beats an earlier
    /// default-priority failure mount for the same matcher.
    pub async fn mount_comment_create_resume(&self, id: u64) {
        self.gh
            .mount_status_priority(
                "POST",
                &format!("/repos/owner/r/issues/{PR}/comments"),
                201,
                1,
                serde_json::json!({ "id": id, "body": "" }),
            )
            .await;
    }

    /// Mount the sticky-comment update endpoint (`PATCH
    /// /issues/comments/{id}`).
    pub async fn mount_comment_patch(&self, id: u64) {
        self.gh
            .mount(
                "PATCH",
                &format!("/repos/owner/r/issues/comments/{id}"),
                serde_json::json!({ "id": id, "body": "" }),
            )
            .await;
    }

    /// Mount the idempotency compare (`GET /issues/comments/{id}`)
    /// with a body different from the PASS render so the PASS
    /// revision takes the PATCH path.
    pub async fn mount_comment_get(&self, id: u64) {
        self.gh
            .mount(
                "GET",
                &format!("/repos/owner/r/issues/comments/{id}"),
                serde_json::json!({ "id": id, "body": "stale fail body" }),
            )
            .await;
    }

    // --- phase drivers (the D2 seams) ---------------------------------------

    /// Phase 1/4: discovery + admission via `poll_review_step_for_tests`.
    pub async fn drive_discovery(
        &self,
    ) -> caduceus::daemon::tick::review_discovery::ReviewDiscoveryStats {
        let remote_url = self.remote_url();
        caduceus::daemon::tick::review_discovery::poll_review_step_for_tests(
            &["owner/r".to_string()],
            self.client.as_ref(),
            &self.cfg,
            self.store.as_ref(),
            &self.runner,
            &move |_owner: &str, _repo: &str| Ok(remote_url.clone()),
            &|_repository: &caduceus::review::RepositoryId, _head_repo: &str| {
                Box::pin(async { None })
                    as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
            },
        )
        .await
        .expect("discovery step succeeds")
    }

    /// Fork discovery + admission (issue #337 Phase 2): the same step
    /// driven with the quarantine resolvers — the base remote for the
    /// trusted-origin clone and the REAL GitHub REST
    /// `repos/{owner}/{repo}` lookup (via the wiremock mount) for the
    /// fork URL — exactly the seam the production tick wires.
    pub async fn drive_discovery_fork(
        &self,
    ) -> caduceus::daemon::tick::review_discovery::ReviewDiscoveryStats {
        let remote_url = self.remote_url();
        let client = Arc::clone(&self.client);
        caduceus::daemon::tick::review_discovery::poll_review_step_for_tests(
            &["owner/r".to_string()],
            self.client.as_ref(),
            &self.cfg,
            self.store.as_ref(),
            &self.runner,
            &move |_owner: &str, _repo: &str| Ok(remote_url.clone()),
            &move |_repository: &caduceus::review::RepositoryId, head_repo: &str| {
                let client = Arc::clone(&client);
                let head_repo = head_repo.to_string();
                Box::pin(async move {
                    let response = client
                        .get(
                            &format!("/repos/{head_repo}"),
                            caduceus::github::ACCEPT_VALUE,
                        )
                        .await
                        .ok()?;
                    let repo: serde_json::Value = serde_json::from_slice(&response.body).ok()?;
                    repo.get("clone_url")?.as_str().map(str::to_string)
                }) as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
            },
        )
        .await
        .expect("fork discovery step succeeds")
    }

    /// Phase 2/5: claim + full dispatch through the real supervisor +
    /// `review_harness.py` subprocess. Returns the tick outcome and
    /// the run id used for the history-row assertions.
    pub async fn drive_claim(&self, run_id: &str, kind: &str) -> TickOutcome {
        let claimed = self
            .store
            .acquire_next_review(run_id, std::process::id(), chrono::Utc::now())
            .expect("acquire succeeds")
            .expect("an eligible review entry exists");
        assert_eq!(
            claimed.entry.target.pull_request, PR,
            "the claimed entry is PR {PR}"
        );

        let guard = ReviewRunGuard::new(
            claimed.claim.clone(),
            Arc::clone(&self.store),
            self.cfg.state_dir.join("processor.log"),
            claimed.entry.target.clone(),
            self.runner.clone(),
        );
        let mut guard = guard;

        let admit = self
            .services
            .pool
            .admit("repo:owner/r", "owner/r")
            .await
            .expect("pool admits under cap");

        let remote_url = self.remote_url();
        let resolve_remote: caduceus::daemon::tick::per_review::RemoteResolver =
            Arc::new(move |_owner: &str, _repo: &str| Ok(remote_url.clone()));

        let outcome = caduceus::daemon::tick::per_review::run_review_claim_for_tests(
            self.worker_cfg(kind),
            &self.services,
            Arc::clone(&self.client),
            self.store.as_ref(),
            claimed,
            &mut guard,
            CancellationToken::new(),
            &mut None,
            admit,
            resolve_remote,
        )
        .await
        .expect("claim-side dispatch succeeds");
        outcome
    }

    /// Phase 3/6: finalizer poll via `poll_publication_step_for_tests`.
    pub async fn drive_finalize(
        &self,
    ) -> caduceus::daemon::tick::review_finalize_step::PublicationStats {
        caduceus::daemon::tick::review_finalize_step::poll_publication_step_for_tests(
            self.client.as_ref(),
            &self.cfg,
            self.store.as_ref(),
        )
        .await
        .expect("finalizer poll succeeds")
    }

    /// The canonical (repo, pr) identity.
    pub fn repository(&self) -> RepositoryId {
        RepositoryId {
            owner: "owner".to_string(),
            repo: "r".to_string(),
        }
    }
}

/// Build the worker argv that runs the review harness with a scripted
/// result kind via the `env` utility (survives the supervisor's
/// `env_clear()` sanitized worker environment).
fn worker_command(kind: &str, harness_py: &std::path::Path) -> Vec<String> {
    vec![
        "env".to_string(),
        format!("FAKE_REVIEW_RESULT={kind}"),
        "python3".to_string(),
        harness_py.display().to_string(),
    ]
}

/// Convenience re-export for tests that drive the store directly.
pub fn review_target(repository: &RepositoryId, head_sha: &str, base_sha: &str) -> ReviewTarget {
    ReviewTarget {
        repository: repository.clone(),
        pull_request: PR,
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
        base_ref: "main".to_string(),
        merge_base: base_sha.to_string(),
    }
}

/// Decode a history row's verdict for assertions.
pub fn verdict_of_row(result_json: &str) -> String {
    let doc: serde_json::Value = serde_json::from_str(result_json).expect("history row parses");
    doc["review"]["verdict"]
        .as_str()
        .expect("verdict present")
        .to_string()
}

/// Assert the store holds exactly the expected history rows
/// `(run_id, generation, head_sha, verdict)` in order.
pub fn assert_history(
    store: &ReviewStore,
    repository: &RepositoryId,
    expected: &[(&str, u64, &str, &str)],
) {
    let rows = store
        .history_for_pull_request(repository, PR)
        .expect("history read");
    assert_eq!(
        rows.len(),
        expected.len(),
        "history row count — got {:?}",
        rows.iter()
            .map(|r| (r.review_run_id.as_str(), r.review_generation))
            .collect::<Vec<_>>()
    );
    for (row, (run_id, generation, head_sha, verdict)) in rows.iter().zip(expected) {
        assert_eq!(row.review_run_id, *run_id, "run id");
        assert_eq!(
            row.review_generation, *generation,
            "generation for {run_id}"
        );
        assert_eq!(row.head_sha, *head_sha, "head sha for {run_id}");
        assert_eq!(
            verdict_of_row(&row.result_json),
            *verdict,
            "verdict for {run_id}"
        );
    }
}

/// A validator-correct fabricated `ReviewResult` document for the
/// given verdict (the finalizer is history-driven; the row is the
/// durable result, DAR §9.1).
pub fn fabricated_row(
    repository: &RepositoryId,
    run_id: &str,
    head_sha: &str,
    generation: u64,
    verdict: &str,
) -> caduceus::state::review::ReviewHistoryRow {
    let verdict_enum = if verdict == "fail" {
        caduceus::review::Verdict::Fail
    } else {
        caduceus::review::Verdict::Pass
    };
    let findings = match verdict_enum {
        caduceus::review::Verdict::Fail => vec![caduceus::review::Finding {
            severity: caduceus::review::Severity::Blocking,
            title: "Blocking finding".to_string(),
            body: "Deterministic FAIL finding.".to_string(),
            path: Some("src/lib.rs".to_string()),
            line: Some(1),
            remediation: Some("Fix it.".to_string()),
        }],
        caduceus::review::Verdict::Pass => vec![],
    };
    let result = caduceus::review::ReviewResult {
        schema_version: caduceus::review::REVIEW_SCHEMA_VERSION,
        status: caduceus::review::ExecutionStatus::Success,
        review: Some(caduceus::review::Review {
            verdict: verdict_enum,
            summary: format!("Scripted {verdict} summary."),
            findings,
        }),
    };
    caduceus::state::review::ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: repository.clone(),
        pull_request: PR,
        head_sha: head_sha.to_string(),
        review_generation: generation,
        completed_at: chrono::Utc::now(),
        result_json: serde_json::to_string(&result).expect("result serializes"),
    }
}
