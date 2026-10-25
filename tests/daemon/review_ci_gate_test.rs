//! Auto Review CI gate at discovery (issue #435).
//!
//! Existing discovery tests keep `require_ci_pass: false`. These
//! cases opt in and mount combined-status + check-runs responses.

use caduceus::config::{AutoReviewConfig, Config, ForkPolicy, PublicationMode};
use caduceus::daemon::tick::review_discovery::{
    emit_skipped_ci_failed_for_tests, emit_skipped_ci_pending_for_tests,
    poll_review_step_for_tests, SKIPPED_CI_FAILED_EVENT, SKIPPED_CI_PENDING_EVENT,
};
use caduceus::error::CaduceusError;
use caduceus::github::{Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::repo::fork_quarantine::fork_quarantine_root;
use caduceus::review::RepositoryId;
use caduceus::state::review::ReviewStore;
use caduceus::worktree::GitRunner;
use serial_test::serial;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Command;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

fn ar_gated(enabled: bool) -> AutoReviewConfig {
    AutoReviewConfig {
        enabled,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
        publication_mode: PublicationMode::Update,
        require_ci_pass: true,
    }
}

fn discovery_config(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.watched_repos = vec!["owner/r".to_string()];
    cfg.auto_review = Some(ar_gated(true));
    cfg
}

fn review_store(state_dir: &Path) -> ReviewStore {
    ReviewStore::open(state_dir).expect("review store opens")
}

fn init_bare_remote(remote: &Path) -> (String, String) {
    std::fs::create_dir_all(remote).expect("mkdir");
    let status = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(remote)
        .status()
        .expect("git init");
    assert!(status.success());
    let work = remote.parent().unwrap().join("seed");
    std::fs::create_dir_all(&work).expect("workdir");
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&work)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "ci@example.com"]);
    run(&["config", "user.name", "CI"]);
    std::fs::write(work.join("a.txt"), "a").unwrap();
    run(&["add", "a.txt"]);
    run(&["commit", "-m", "A"]);
    let base = run(&["rev-parse", "HEAD"]);
    run(&["checkout", "-b", "feature"]);
    std::fs::write(work.join("b.txt"), "b").unwrap();
    run(&["add", "b.txt"]);
    run(&["commit", "-m", "B"]);
    let tip = run(&["rev-parse", "HEAD"]);
    run(&["remote", "add", "origin", &remote.display().to_string()]);
    run(&["push", "origin", "main"]);
    run(&["push", "origin", "feature"]);
    (base, tip)
}

fn pr_row(pr: u64, head: &str, base: &str) -> serde_json::Value {
    serde_json::json!({
        "number": pr,
        "title": "CI gate",
        "state": "open",
        "draft": false,
        "base": {"ref": "main", "sha": base, "repo": {"full_name": "owner/r"}},
        "head": {"ref": "feature", "sha": head, "repo": {"full_name": "owner/r"}},
    })
}

fn fork_pr_row(pr: u64, head: &str, base: &str) -> serde_json::Value {
    let mut row = pr_row(pr, head, base);
    row["head"]["repo"] = serde_json::json!({"full_name": "forkuser/r"});
    row
}

fn draft_row(pr: u64, head: &str, base: &str) -> serde_json::Value {
    let mut row = pr_row(pr, head, base);
    row["draft"] = serde_json::json!(true);
    row
}

fn empty_ci() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({"state": "pending", "total_count": 0, "statuses": []}),
        serde_json::json!({"total_count": 0, "check_runs": []}),
    )
}

fn pending_ci() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({"state": "pending", "total_count": 0, "statuses": []}),
        serde_json::json!({
            "total_count": 1,
            "check_runs": [{"status": "in_progress", "conclusion": null}]
        }),
    )
}

fn failed_ci() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({"state": "pending", "total_count": 0, "statuses": []}),
        serde_json::json!({
            "total_count": 1,
            "check_runs": [{"status": "completed", "conclusion": "failure"}]
        }),
    )
}

fn green_ci() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({"state": "success", "total_count": 1, "statuses": []}),
        serde_json::json!({
            "total_count": 1,
            "check_runs": [{"status": "completed", "conclusion": "success"}]
        }),
    )
}

struct GateHarness {
    cfg: Config,
    server: MockServer,
    store: ReviewStore,
    remote_dir: std::path::PathBuf,
    base_sha: String,
    tip_sha: String,
}

impl GateHarness {
    async fn start(label: &str) -> Self {
        let root = tempdir(label);
        let server = MockServer::start().await;
        let mut cfg = discovery_config(&root, &server.uri());
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;
        let remote_dir = root.join("origin.git");
        let (base_sha, tip_sha) = init_bare_remote(&remote_dir);
        let store = review_store(&root.join("state"));
        Self {
            cfg,
            server,
            store,
            remote_dir,
            base_sha,
            tip_sha,
        }
    }

    fn client(&self) -> Client {
        let cache = HttpCache::open(&self.cfg.state_dir).expect("cache");
        Client::with_cache(&self.cfg, cache).expect("client")
    }

    fn remote_url(&self) -> String {
        format!("file://{}", self.remote_dir.display())
    }

    async fn mount_pulls(&self, rows: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .mount(&self.server)
            .await;
    }

    async fn mount_ci(&self, sha: &str, status: serde_json::Value, checks: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/owner/r/commits/{sha}/status")))
            .respond_with(ResponseTemplate::new(200).set_body_json(status))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(format!(
                r"^/repos/owner/r/commits/{sha}/check-runs"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(checks))
            .mount(&self.server)
            .await;
    }

    async fn poll(&self) -> caduceus::daemon::tick::review_discovery::ReviewDiscoveryStats {
        let remote = self.remote_url();
        poll_review_step_for_tests(
            &["owner/r".to_string()],
            &self.client(),
            &self.cfg,
            &self.store,
            &GitRunner::new(&self.cfg),
            &move |_o, _r| Ok(remote.clone()),
            &|_repository: &RepositoryId, _head_repo: &str| {
                Box::pin(async { None })
                    as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
            },
        )
        .await
        .expect("step ok")
    }
}

#[tokio::test]
async fn no_checks_admits() {
    let h = GateHarness::start("ci-absent").await;
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = empty_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;
    let stats = h.poll().await;
    assert_eq!(stats.admitted, 1);
    assert_eq!(stats.skipped_ci_pending, 0);
    assert_eq!(stats.skipped_ci_failed, 0);
}

#[test]
#[serial]
fn ci_pending_event_emits_structured_line() {
    let root = tempdir("ci-pending-event");
    let log_path = root.join("e.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    tracing::subscriber::with_default(subscriber, || {
        emit_skipped_ci_pending_for_tests("o/r", 7, "abc");
    });
    drop(guard);
    let body = std::fs::read_to_string(&log_path).expect("read");
    assert!(
        body.contains(&format!("\"event\":\"{SKIPPED_CI_PENDING_EVENT}\"")),
        "{body}"
    );
}

#[tokio::test]
async fn pending_skips_without_enqueue_and_retries() {
    let h = GateHarness::start("ci-pending").await;
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = pending_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;
    let stats = h.poll().await;
    assert_eq!(stats.admitted, 0);
    assert_eq!(stats.discovered, 0);
    assert_eq!(stats.skipped_ci_pending, 1);
    assert!(h.store.review_queue_snapshot().unwrap().entries.is_empty());
    let stats2 = h.poll().await;
    assert_eq!(stats2.skipped_ci_pending, 1);
    assert_eq!(stats2.discovered, 0);
    assert_eq!(stats2.admitted, 0);
}

#[test]
#[serial]
fn ci_failed_event_emits_structured_line() {
    let root = tempdir("ci-failed-event");
    let log_path = root.join("e.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    tracing::subscriber::with_default(subscriber, || {
        emit_skipped_ci_failed_for_tests("o/r", 7, "abc");
    });
    drop(guard);
    let body = std::fs::read_to_string(&log_path).expect("read");
    assert!(
        body.contains(&format!("\"event\":\"{SKIPPED_CI_FAILED_EVENT}\"")),
        "{body}"
    );
}

#[tokio::test]
async fn failure_persists_then_already_complete() {
    let h = GateHarness::start("ci-fail").await;
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = failed_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;
    let stats = h.poll().await;
    assert_eq!(stats.skipped_ci_failed, 1);
    assert_eq!(stats.discovered, 0);
    assert_eq!(stats.admitted, 0);
    let repo = RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    };
    let rejected = h.store.ci_rejected_head_shas(&repo, 7).unwrap();
    assert_eq!(rejected, vec![h.tip_sha.clone()]);

    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{}/status", h.tip_sha)))
        .respond_with(ResponseTemplate::new(500).set_body_string("should not be called"))
        .expect(0)
        .mount(&h.server)
        .await;

    let stats2 = h.poll().await;
    assert_eq!(stats2.skipped_already_complete, 1);
    assert_eq!(stats2.discovered, 0);
    assert_eq!(stats2.admitted, 0);
    assert_eq!(stats2.ci_lookup_failed, 0);
}

#[tokio::test]
async fn later_green_sha_admits() {
    let h = GateHarness::start("ci-new-green").await;
    // Seed a prior red SHA rejection for a different head.
    let repo = RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    };
    h.store
        .record_ci_rejection(&repo, 7, &h.base_sha)
        .expect("seed red");
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = green_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;
    let stats = h.poll().await;
    assert_eq!(stats.admitted, 1);
}

#[tokio::test]
async fn require_ci_pass_false_skips_commits_api() {
    let h = GateHarness::start("ci-off").await;
    let mut cfg = h.cfg.clone();
    cfg.auto_review = Some(AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
        publication_mode: PublicationMode::Update,
        require_ci_pass: false,
    });
    // Even with a prior rejection, flag-off must admit without CI GETs.
    let repo = RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    };
    h.store
        .record_ci_rejection(&repo, 7, &h.tip_sha)
        .expect("seed");
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/owner/r/commits/"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&h.server)
        .await;
    let remote = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &move |_o, _r| Ok(remote.clone()),
        &|_repository: &RepositoryId, _head_repo: &str| {
            Box::pin(async { None })
                as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
        },
    )
    .await
    .expect("ok");
    assert_eq!(stats.admitted, 1);
}

#[tokio::test]
async fn draft_skips_before_ci_call() {
    let h = GateHarness::start("ci-draft").await;
    h.mount_pulls(vec![draft_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/owner/r/commits/"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&h.server)
        .await;
    let stats = h.poll().await;
    assert_eq!(stats.skipped_draft, 1);
    assert_eq!(stats.admitted, 0);
}

#[tokio::test]
async fn allowed_fork_failed_ci_skips_before_quarantine() {
    let h = GateHarness::start("ci-fork-fail").await;
    let mut cfg = h.cfg.clone();
    cfg.auto_review = Some(AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: Some(ForkPolicy {
            allow_fork_prs: vec!["owner/r".to_string()],
        }),
        publication_mode: PublicationMode::Update,
        require_ci_pass: true,
    });
    h.mount_pulls(vec![fork_pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = failed_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;

    let remote = h.remote_url();
    let fork_remote = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &move |_o, _r| Ok(remote.clone()),
        &move |_repository: &RepositoryId, _head_repo: &str| {
            let url = fork_remote.clone();
            Box::pin(async move { Some(url) })
                as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
        },
    )
    .await
    .expect("ok");
    assert_eq!(stats.skipped_ci_failed, 1);
    assert_eq!(stats.discovered, 0);
    assert_eq!(stats.admitted, 0);
    assert!(h.store.review_queue_snapshot().unwrap().entries.is_empty());
    assert!(
        !fork_quarantine_root(&cfg.state_dir).exists(),
        "CI failure must not create a quarantine directory"
    );
}

#[tokio::test]
async fn pending_does_not_consume_budget() {
    let h = GateHarness::start("ci-budget").await;
    let mut cfg = h.cfg.clone();
    cfg.max_reviews_per_tick = 1;
    // One pending PR must leave the budget unused so a later green
    // candidate in the same tick could still admit.
    h.mount_pulls(vec![pr_row(1, &h.tip_sha, &h.base_sha)])
        .await;
    let (st, ck) = pending_ci();
    h.mount_ci(&h.tip_sha, st, ck).await;

    let remote = h.remote_url();
    let stats = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &cfg,
        &h.store,
        &GitRunner::new(&cfg),
        &move |_o, _r| Ok(remote.clone()),
        &|_repository: &RepositoryId, _head_repo: &str| {
            Box::pin(async { None })
                as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
        },
    )
    .await
    .expect("ok");
    assert_eq!(stats.skipped_ci_pending, 1);
    assert_eq!(stats.admitted, 0);
    assert!(
        !stats.budget_exhausted,
        "pending must not count toward max_reviews_per_tick"
    );
}

#[tokio::test]
async fn check_run_500_skips_row_and_admits_sibling() {
    let h = GateHarness::start("ci-500").await;
    // Need two distinct SHAs — tip and mid. Create mid by amending.
    // Simpler: two PRs same tip; first CI 500, second absent.
    // Status for tip: first call 500 on check-runs only after status ok.
    h.mount_pulls(vec![
        pr_row(1, &h.tip_sha, &h.base_sha),
        pr_row(2, &h.base_sha, &h.base_sha),
    ])
    .await;
    // tip: status ok, check-runs 500
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{}/status", h.tip_sha)))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "pending", "total_count": 0, "statuses": []
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/repos/owner/r/commits/{}/check-runs",
            h.tip_sha
        )))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&h.server)
        .await;
    // base_sha used as second head: absent CI
    let (st, ck) = empty_ci();
    h.mount_ci(&h.base_sha, st, ck).await;

    let stats = h.poll().await;
    assert_eq!(stats.ci_lookup_failed, 1);
    assert_eq!(stats.admitted, 1);
}

#[tokio::test]
async fn http_403_aborts_step() {
    let h = GateHarness::start("ci-403").await;
    h.mount_pulls(vec![
        pr_row(1, &h.tip_sha, &h.base_sha),
        pr_row(2, &h.base_sha, &h.base_sha),
    ])
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{}/status", h.tip_sha)))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&h.server)
        .await;

    let remote = h.remote_url();
    let err = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |_o, _r| Ok(remote.clone()),
        &|_repository: &RepositoryId, _head_repo: &str| {
            Box::pin(async { None })
                as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
        },
    )
    .await
    .expect_err("403 aborts");
    assert!(
        matches!(err, CaduceusError::GitHubApi { status: 403, .. }),
        "got {err:?}"
    );
    assert!(h.store.review_queue_snapshot().unwrap().entries.is_empty());
    let repo = RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    };
    assert!(h.store.ci_rejected_head_shas(&repo, 1).unwrap().is_empty());
}

#[tokio::test]
async fn rate_limit_on_ci_aborts_without_write() {
    let h = GateHarness::start("ci-rl").await;
    h.mount_pulls(vec![pr_row(7, &h.tip_sha, &h.base_sha)])
        .await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{}/status", h.tip_sha)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-RateLimit-Remaining", "0")
                .insert_header("X-RateLimit-Reset", reset.to_string())
                .insert_header("X-RateLimit-Limit", "5000")
                .set_body_json(serde_json::json!({"state":"pending","total_count":0})),
        )
        .mount(&h.server)
        .await;

    let remote = h.remote_url();
    let err = poll_review_step_for_tests(
        &["owner/r".to_string()],
        &h.client(),
        &h.cfg,
        &h.store,
        &GitRunner::new(&h.cfg),
        &move |_o, _r| Ok(remote.clone()),
        &|_repository: &RepositoryId, _head_repo: &str| {
            Box::pin(async { None })
                as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
        },
    )
    .await
    .expect_err("rate limit");
    assert!(matches!(err, CaduceusError::RateLimited { .. }));
}
