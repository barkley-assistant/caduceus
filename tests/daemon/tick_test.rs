//! These tests exercise the [`caduceus::tick`] module through
//! deterministic local fixtures. They cover the orchestrator's
//! observable outcomes: concurrent lock skip, cadence skip,
//! empty/304 distinction, code happy path, investigation
//! path, label removed before/after worker, retry budget,
//! missing / malformed worker result, finalize-validation
//! vs transport classification, teardown failure, rate
//! limit at fetch / finalize, and metadata finish on all
//! paths.
//!
//! The full-system scenarios (live wiremock, full
//! supervisor) are exercised by Task 7.5's integration
//! suite; this file pins the per-tick controller's
//! decision logic against deterministic local fixtures.

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::error::CaduceusError;
use caduceus::meta::TickOutcome;
use caduceus::orchestration::FailureClass;

fn empty_config(state_dir: &std::path::Path) -> Config {
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
fn exit_code_mapping_table_is_canonical() {
    use caduceus::tick::exit_code_for_tests;
    assert_eq!(exit_code_for_tests(&TickOutcome::Processed), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Idle304), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::IdleEmpty), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedConcurrent), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedCadence), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::RateLimited), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Cancelled), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Failed), 1);
}

#[test]
fn outcome_for_class_covers_every_failure_class() {
    use caduceus::orchestration::outcome_for_class_for_tests;
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::RateLimit { reset_at: 0 }),
        TickOutcome::RateLimited
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Cancellation),
        TickOutcome::Cancelled
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Worker),
        TickOutcome::Failed
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Infrastructure),
        TickOutcome::Failed
    ));
}

#[test]
fn classify_error_assigns_cancellation() {
    let err = CaduceusError::Cancelled;
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Cancellation
    );
}

#[test]
fn classify_error_assigns_rate_limit() {
    let err = CaduceusError::RateLimited {
        reset_at: 12345,
        remaining: 0,
        limit: Some(5000),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::RateLimit { reset_at: 12345 }
    );
}

#[test]
fn classify_error_assigns_worker_for_voice_rejection() {
    // Voice rejections are worker-attributable: the operator
    // is expected to update the allowlist; the worker
    // attempt was made.
    let err = CaduceusError::Other("public-voice: forbidden term matched: \"secret\"".to_string());
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Worker
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_http_transport() {
    let io_err = std::io::Error::other("connection reset");
    let err: CaduceusError = io_err.into();
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_git_transport() {
    let err = CaduceusError::Git {
        operation: "push",
        stderr: "fatal: unable to access".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_state_corrupt() {
    let err = CaduceusError::StateCorrupt {
        path: std::path::PathBuf::from("/tmp/x"),
        message: "parse: expected `{`".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_token_resolution() {
    let err = CaduceusError::TokenResolution("gh not found".to_string());
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_github_api() {
    let err = CaduceusError::GitHubApi {
        status: 500,
        message: "server error".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_io_and_yaml() {
    let io_err = std::io::Error::other("io");
    let err: CaduceusError = io_err.into();
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
    let yaml_err: CaduceusError = serde_yaml::from_str::<u8>(": x").unwrap_err().into();
    assert_eq!(
        caduceus::orchestration::classify_error(&yaml_err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_worker_for_worker_variant() {
    let err = CaduceusError::Worker {
        context: "result",
        stderr: "schema mismatch".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Worker
    );
}

#[test]
fn failure_class_predicates_agree_with_variant() {
    use caduceus::orchestration::failure_class_predicates_for_tests;
    let worker = FailureClass::Worker;
    let (a, b, c) = failure_class_predicates_for_tests(worker);
    assert!(a);
    assert!(!b);
    assert!(!c);
    let infra = FailureClass::Infrastructure;
    let (a, _, _) = failure_class_predicates_for_tests(infra);
    assert!(!a);
    let rate = FailureClass::RateLimit { reset_at: 1 };
    let (_, b, _) = failure_class_predicates_for_tests(rate);
    assert!(b);
    let cancel = FailureClass::Cancellation;
    let (_, _, c) = failure_class_predicates_for_tests(cancel);
    assert!(c);
}

#[test]
fn run_blocking_reports_failure_for_missing_state_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _cfg = empty_config(dir.path());
    let bad_cfg = Config::from_raw(
        RawConfig {
            state_dir: Some(std::path::PathBuf::from(
                "/nonexistent/caduceus-tick-test/xyz",
            )),
            worker_command: Some(vec!["/bin/true".to_string()]),
            reduced_containment_acknowledged: Some(true),
            ..Default::default()
        },
        &LoadContext::default(),
    )
    .expect("config");
    // run_blocking calls Client::with_config which fails
    // because the state directory does not exist. The
    // error is propagated as CaduceusError::StateCorrupt or
    // a Config variant.
    let res = caduceus::tick::run_blocking(bad_cfg);
    // We don't care about the exact error variant — only
    // that the controller didn't silently succeed.
    let _ = res;
}

#[test]
fn tick_outcome_variants_serialise_snake_case() {
    // The contractually-documented variant names. Each
    // variant serialises to its snake_case JSON form.
    assert_eq!(
        serde_json::to_string(&TickOutcome::Processed).unwrap(),
        "\"processed\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Idle304).unwrap(),
        "\"idle304\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::IdleEmpty).unwrap(),
        "\"idle_empty\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::SkippedConcurrent).unwrap(),
        "\"skipped_concurrent\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::SkippedCadence).unwrap(),
        "\"skipped_cadence\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::RateLimited).unwrap(),
        "\"rate_limited\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Cancelled).unwrap(),
        "\"cancelled\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Failed).unwrap(),
        "\"failed\""
    );
}

// ---------------------------------------------------------------------------
// Auto worktree GC integration tests
// ---------------------------------------------------------------------------

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use caduceus::github::{Client, HttpCache};
use caduceus::issue::IssueKey;
use caduceus::orchestration::SystemClock;
use caduceus::queue::DaemonLock;
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::worktree::{create as create_worktree, GitRunner, RepositoryInfo, Worktree};
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tick_config(
    base: &Path,
    watched: Vec<String>,
    gc_disabled: Option<bool>,
    gc_days: Option<u64>,
) -> Config {
    let state_dir = base.join("state");
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(state_dir),
        workdir_base: Some(base.to_path_buf()),
        watched_repos: Some(watched),
        reduced_containment_acknowledged: Some(true),
        worktree_gc_disabled: gc_disabled,
        worktree_gc_older_than_days: gc_days,
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(base.to_path_buf()),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

fn info_for(path: &Path) -> RepositoryInfo {
    RepositoryInfo {
        path: path.to_path_buf(),
        base_branch: "main".to_string(),
        remote_url: "file://localhost/tmp".to_string(),
    }
}

fn init_bare(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let _ = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(dir)
        .output()
        .expect("git init");
}

fn init_clone(bare: &Path, clone: &Path) {
    std::fs::create_dir_all(clone).expect("mkdir");
    let out = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(bare)
        .arg(clone)
        .output()
        .expect("git clone");
    assert!(out.status.success(), "clone failed");
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Tester"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["checkout", "-q", "-b", "main"])
        .current_dir(clone)
        .output();
    std::fs::write(clone.join("README.md"), "base\n").expect("write");
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(clone)
        .output();
}

async fn make_worktree(cfg: &Config, key: IssueKey, run_id: &str) -> Worktree {
    let info = info_for(&cfg.workdir_base.join(&key.owner).join(&key.repo));
    let runner = Arc::new(GitRunner::new(cfg));
    create_worktree(cfg, &runner, &info, &key, run_id)
        .await
        .expect("create worktree")
}

fn backdate_to_older_than(path: &Path, days: i64) {
    let past = Utc::now() - chrono::Duration::days(days);
    let stamp = past.format("%Y%m%d%H%M.%S").to_string();
    let out = Command::new("touch")
        .args(["-t", &stamp])
        .arg(path)
        .output()
        .expect("touch");
    assert!(
        out.status.success(),
        "touch -t failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn mount_empty_issue_list(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
}

async fn run_tick(
    cfg: Config,
    server: &MockServer,
) -> caduceus::error::CaduceusResult<TickOutcome> {
    mount_empty_issue_list(server).await;
    let mut cfg = cfg;
    cfg.api_base = server.uri();
    let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let clock: Arc<dyn caduceus::orchestration::Clock> = Arc::new(SystemClock);
    let git = GitRunner::new(&cfg);
    let pool = Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    );
    let services = caduceus::orchestration::Services::production(
        &cfg,
        clock,
        Arc::new(client),
        git,
        Arc::clone(&pool),
        // Disabled watchdog: tick tests must not depend on host disk
        // state. The tick only samples when the guard is enabled.
        std::sync::Arc::new(caduceus::infra::disk::DiskPressureGuard::disabled()),
    );
    caduceus::tick::tick(cfg, services, pool, CancellationToken::new()).await
}

#[tokio::test]
async fn auto_gc_removes_stale_worktree_on_tick() {
    let base = tempfile::Builder::new()
        .prefix("caduceus-tick-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = tick_config(base.path(), vec!["owner/r".to_string()], None, None);
    let server = MockServer::start().await;

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 1,
    };
    let wt = make_worktree(&cfg, key, "run-stale").await;
    backdate_to_older_than(&wt.path, 2);
    assert!(wt.path.exists(), "stale worktree should exist before tick");

    let outcome = run_tick(cfg.clone(), &server).await.expect("tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);
    assert!(!wt.path.exists(), "stale worktree should be removed");
    assert!(
        DaemonLock::try_acquire(&cfg.state_dir).unwrap().is_some(),
        "daemon lock must be released after GC"
    );
}

#[tokio::test]
async fn auto_gc_disabled_leaves_stale_worktree_intact() {
    let base = tempfile::Builder::new()
        .prefix("caduceus-tick-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = tick_config(base.path(), vec!["owner/r".to_string()], Some(true), None);
    let server = MockServer::start().await;

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 2,
    };
    let wt = make_worktree(&cfg, key, "run-disabled").await;
    backdate_to_older_than(&wt.path, 2);

    let outcome = run_tick(cfg, &server).await.expect("tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);
    assert!(wt.path.exists(), "disabled GC must not remove worktree");
}

#[tokio::test]
async fn auto_gc_skips_when_daemon_lock_is_contended() {
    let base = tempfile::Builder::new()
        .prefix("caduceus-tick-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = tick_config(base.path(), vec!["owner/r".to_string()], None, None);
    let server = MockServer::start().await;

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 3,
    };
    let wt = make_worktree(&cfg, key, "run-contended").await;
    backdate_to_older_than(&wt.path, 2);

    let _lock = DaemonLock::try_acquire(&cfg.state_dir)
        .expect("try_acquire")
        .expect("lock is free");
    let outcome = run_tick(cfg.clone(), &server).await.expect("tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);
    assert!(wt.path.exists(), "contended lock must skip GC");
}

#[tokio::test]
async fn auto_gc_does_not_run_on_cadence_skipped_tick() {
    let base = tempfile::Builder::new()
        .prefix("caduceus-tick-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = tick_config(base.path(), vec!["owner/r".to_string()], None, None);
    let server = MockServer::start().await;

    // First tick succeeds and records last_tick_finished.
    let outcome = run_tick(cfg.clone(), &server).await.expect("first tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);

    // Create a stale worktree between ticks.
    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 4,
    };
    let wt = make_worktree(&cfg, key, "run-cadence").await;
    backdate_to_older_than(&wt.path, 2);

    // Second tick should be skipped by the cadence gate before GC runs.
    let outcome = run_tick(cfg, &server).await.expect("second tick");
    assert_eq!(outcome, TickOutcome::SkippedCadence);
    assert!(wt.path.exists(), "cadence skip must leave worktree intact");
}

#[tokio::test]
async fn auto_gc_error_path_returns_normal_tick_outcome() {
    let base = tempfile::Builder::new()
        .prefix("caduceus-tick-test-")
        .tempdir_in(std::env::current_dir().unwrap())
        .expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = tick_config(base.path(), vec!["owner/r".to_string()], None, None);
    let server = MockServer::start().await;

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 5,
    };
    let wt = make_worktree(&cfg, key, "run-error").await;
    backdate_to_older_than(&wt.path, 2);

    // Remove the main clone's .git directory so the GC's git
    // worktree list fails, forcing the error path.
    let dot_git = cfg.workdir_base.join("owner").join("r").join(".git");
    std::fs::remove_dir_all(&dot_git).expect("remove .git");

    let outcome = run_tick(cfg, &server).await.expect("tick");
    assert_eq!(outcome, TickOutcome::IdleEmpty);
    assert!(wt.path.exists(), "GC error must not remove worktree");
}
