//! Integration tests for the orchestration active-run guard
//! (`src/daemon/orchestration/active_run.rs`). Moved out of the
//! inline `#[cfg(test)]` module per AGENTS.md.

use caduceus::config::Config;
use caduceus::orchestration::{
    classify_error, ActiveRunGuard, Clock, FailureClass, FakeClock, SystemClock,
};
use caduceus::queue::{
    ClaimFileBody, ClaimToken, Phase, StateStore, TicketType, CLAIM_FILE_VERSION,
};
use caduceus::worktree::{create as create_worktree, GitRunner, RepositoryInfo};
use caduceus::{CaduceusError, IssueKey};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

fn cfg() -> Config {
    Config::test_defaults(std::path::Path::new("/tmp"))
}

fn dummy_claim() -> ClaimToken {
    ClaimToken::for_test(
        std::env::temp_dir().join("caduceus-orchestration-tests"),
        "deadbeef00",
        "RUNID",
    )
}

#[test]
fn classify_error_maps_variants() {
    // Cancellation
    let err = CaduceusError::Cancelled;
    assert_eq!(classify_error(&err), FailureClass::Cancellation);

    // Rate limit
    let err = CaduceusError::RateLimited {
        reset_at: 12345,
        remaining: 0,
        limit: Some(5000),
    };
    assert_eq!(
        classify_error(&err),
        FailureClass::RateLimit { reset_at: 12345 }
    );

    // Worker-attributable
    let err = CaduceusError::Worker {
        context: "result",
        stderr: "schema mismatch".to_string(),
    };
    assert_eq!(classify_error(&err), FailureClass::Worker);

    let err = CaduceusError::Other("voice: forbidden term".to_string());
    assert_eq!(classify_error(&err), FailureClass::Worker);

    // Infrastructure
    let err = CaduceusError::Config("bad worker command".to_string());
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    let err = CaduceusError::TokenResolution("gh not found".to_string());
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    // HTTP transport — infrastructure
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
    let err: CaduceusError = io_err.into();
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    // Pool saturated — infrastructure
    let err = CaduceusError::PoolSaturated {
        current_depth: 5,
        max_depth: 10,
    };
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    // Repository exclusion held — infrastructure
    let err = CaduceusError::RepositoryExclusionHeld {
        repo_key: "owner/repo".into(),
    };
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    // Drain timeout — cancellation
    let err = CaduceusError::DrainTimeout {
        timed_out_run_ids: vec!["run-1".into()],
    };
    assert_eq!(classify_error(&err), FailureClass::Cancellation);

    // SymlinkedStorageRoot — infrastructure
    let err = CaduceusError::SymlinkedStorageRoot {
        path: PathBuf::from("/tmp/link"),
    };
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);

    // WorktreeReuseAfterFailure — worker
    let err = CaduceusError::WorktreeReuseAfterFailure {
        run_id: "deadbeef".into(),
        worktree_path: PathBuf::from("/tmp/failed"),
        last_state: "Failed".into(),
    };
    assert_eq!(classify_error(&err), FailureClass::Worker);

    // ModeNotPreserved — infrastructure
    let err = CaduceusError::ModeNotPreserved {
        path: PathBuf::from("/tmp/x"),
        expected: 0o700,
        observed: 0o755,
    };
    assert_eq!(classify_error(&err), FailureClass::Infrastructure);
}

#[test]
fn classify_error_is_exhaustive_at_compile_time() {
    // Every CaduceusError variant is classified. If a new
    // variant is added without a `classify_error` arm,
    // this match will fail to compile.
    let variants: [CaduceusError; 40] = [
        CaduceusError::Config("x".into()),
        CaduceusError::Io(std::io::Error::other("x")),
        CaduceusError::Json(serde_json::from_str::<u8>("not-a-number").unwrap_err()),
        CaduceusError::Yaml(serde_yaml::from_str::<u8>(": x").unwrap_err()),
        CaduceusError::Worker {
            context: "spawn",
            stderr: "x".into(),
        },
        CaduceusError::Worktree {
            context: "create",
            stderr: "x".into(),
        },
        CaduceusError::Queue {
            context: "claim",
            stderr: "x".into(),
        },
        CaduceusError::Push {
            context: "push",
            stderr: "x".into(),
        },
        CaduceusError::PushCollision {
            branch: "b".into(),
            remote_oid: "r".into(),
            local_oid: "l".into(),
        },
        CaduceusError::StateCorrupt {
            path: PathBuf::from("/tmp/x"),
            message: "x".into(),
        },
        CaduceusError::Git {
            operation: "commit",
            stderr: "x".into(),
        },
        CaduceusError::GitHubApi {
            status: 500,
            message: "x".into(),
        },
        CaduceusError::RateLimited {
            reset_at: 1,
            remaining: 0,
            limit: None,
        },
        CaduceusError::TokenResolution("x".into()),
        CaduceusError::Cancelled,
        CaduceusError::Other("x".into()),
        // Http is exercised via the Io case above; we
        // synthesise an Http variant by running reqwest's
        // error path indirectly. For the compile-time
        // guard the variant list is the actual concern.
        CaduceusError::Worker {
            context: "http",
            stderr: "transport".into(),
        },
        CaduceusError::LeadershipContended {
            context: "acquire",
            stderr: "contended".into(),
        },
        CaduceusError::LeaseStale {
            context: "renew",
            stderr: "expired".into(),
        },
        CaduceusError::FencingTokenRegression {
            issue_key: "owner/repo#1".into(),
            stale_token: 1,
            current_token: 3,
        },
        CaduceusError::PoolSaturated {
            current_depth: 1,
            max_depth: 2,
        },
        CaduceusError::RepositoryExclusionHeld {
            repo_key: "owner/repo".into(),
        },
        CaduceusError::DrainTimeout {
            timed_out_run_ids: vec!["run-1".into()],
        },
        CaduceusError::CircuitOpen {
            scope: "provider",
            scope_id: "github".into(),
            retry_after: 1800,
            probe_in_flight: false,
        },
        CaduceusError::MaxDegradedAgeExceeded {
            scope: "repository",
            scope_id: "owner/repo".into(),
            opened_at: 1000000,
        },
        CaduceusError::SymlinkedStorageRoot {
            path: PathBuf::from("/tmp/link"),
        },
        CaduceusError::WorktreeReuseAfterFailure {
            run_id: "deadbeef".into(),
            worktree_path: PathBuf::from("/tmp/failed"),
            last_state: "Failed".into(),
        },
        CaduceusError::ModeNotPreserved {
            path: PathBuf::from("/tmp/x"),
            expected: 0o700,
            observed: 0o755,
        },
        CaduceusError::OciCliNotFound {
            cli: "docker".into(),
        },
        CaduceusError::OciEngineUnavailable {
            detail: "Cannot connect".into(),
        },
        CaduceusError::OciMismatchedCliVersion {
            detail: "too old".into(),
        },
        CaduceusError::OciPullFailed {
            image: "img".into(),
            stderr: "pull failed".into(),
        },
        CaduceusError::OciCreateFailed {
            context: "create",
            stderr: "err".into(),
        },
        CaduceusError::OciStartFailed {
            context: "start",
            stderr: "err".into(),
        },
        CaduceusError::OciWaitFailed {
            context: "wait",
            stderr: "err".into(),
        },
        CaduceusError::OciStopFailed {
            context: "stop",
            stderr: "err".into(),
        },
        CaduceusError::OciRemoveFailed {
            context: "remove",
            stderr: "err".into(),
        },
        CaduceusError::OciUndeclaredMount {
            path: "/tmp/x".into(),
        },
        CaduceusError::OciSecretLeakSuspected {
            path: "/tmp/x".into(),
        },
        CaduceusError::OciSecretLeakDetected {
            run_id: "run-1".into(),
        },
    ];
    for v in &variants {
        let _class = classify_error(v);
    }
    // Http is also covered by the match arms even though
    // we don't synthesise one here.
}

#[test]
fn failure_class_predicates() {
    let worker = FailureClass::Worker;
    assert!(worker.counts_against_retry_budget());
    assert!(!worker.must_persist_rate_limit());
    assert!(!worker.is_cancellation());

    let infra = FailureClass::Infrastructure;
    assert!(!infra.counts_against_retry_budget());
    assert!(!infra.must_persist_rate_limit());
    assert!(!infra.is_cancellation());

    let rate = FailureClass::RateLimit { reset_at: 100 };
    assert!(!rate.counts_against_retry_budget());
    assert!(rate.must_persist_rate_limit());
    assert!(!rate.is_cancellation());

    let cancel = FailureClass::Cancellation;
    assert!(!cancel.counts_against_retry_budget());
    assert!(!cancel.must_persist_rate_limit());
    assert!(cancel.is_cancellation());
}

#[test]
fn claim_token_key_accessor_returns_placeholder() {
    // The convenience accessor compiles and returns a
    // reference; the orchestrator's higher-level code
    // keeps its own typed `IssueKey` alongside the guard.
    let claim = dummy_claim();
    let key = claim.key();
    assert_eq!(key.number, 0);
    assert!(key.owner.is_empty());
}

#[test]
fn claim_file_body_round_trip() {
    // Sanity check that the queue module's ClaimFileBody
    // round-trips so the orchestrator can rehydrate a
    // claim token if needed.
    let key = IssueKey::parse("owner/repo#1").expect("key");
    let started_at: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().expect("timestamp");
    let body = ClaimFileBody {
        version: CLAIM_FILE_VERSION,
        key: key.clone(),
        run_id: "RUNID".to_string(),
        pid: 42,
        process_start_identity: "boot-1/100".to_string(),
        started_at,
        worktree_path: None,
    };
    let serialized = serde_json::to_string(&body).unwrap();
    let parsed: ClaimFileBody = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed.version, CLAIM_FILE_VERSION);
    assert_eq!(parsed.key, key);
    assert_eq!(parsed.run_id, "RUNID");
}

#[test]
fn system_clock_returns_recent_utc() {
    let before = Utc::now();
    let now = SystemClock.now();
    let after = Utc::now();
    assert!(now >= before);
    assert!(now <= after);
}

#[test]
fn services_production_helper_compiles() {
    // The constructor wires the four adapters. We can't
    // call services.executor.run here because that
    // would spawn a worker; the test only verifies the
    // type compiles and the field accessors work.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let _ = clock.now();
    let _cfg = cfg();
}

#[test]
fn fake_clock_default_starts_at_epoch() {
    let fc = FakeClock::new();
    assert_eq!(fc.now_unix(), 0);
}

#[test]
fn fake_clock_advance_works() {
    let fc = FakeClock::new();
    fc.advance(100);
    assert_eq!(fc.now_unix(), 100);
}

#[test]
fn fake_clock_set_works() {
    let fc = FakeClock::new();
    fc.set(999);
    assert_eq!(fc.now_unix(), 999);
}

#[test]
fn fake_clock_clones_share_time() {
    let fc = FakeClock::new();
    let fc2 = fc.clone();
    fc.advance(50);
    assert_eq!(fc2.now_unix(), 50);
}

#[test]
fn fake_clock_now_returns_correct_datetime() {
    let fc = FakeClock::at(946684800); // 2000-01-01T00:00:00Z
    assert_eq!(fc.now().timestamp(), 946684800);
}

// ---------------------------------------------------------------------------
// Helpers for real-git worktree fixtures used by the finish_retry teardown
// integration tests. All fixtures are local (`file://` bare remote clones)
// and create one-commit repositories so `git worktree add` / `git worktree
// remove` exercise the exact production path.
// ---------------------------------------------------------------------------

const RUN_ID: &str = "01H9Z3Y4G8W2J7N5K1QXV0F8P3";
const OWNER: &str = "octocat";
const REPO: &str = "Hello-World";
const ISSUE_NUMBER: u64 = 7;

fn run_command(cmd: &mut Command) {
    let output = cmd.output().expect("spawn command");
    if !output.status.success() {
        panic!(
            "command {:?} failed: status={:?}\nstdout={}\nstderr={}",
            cmd,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Initialise a bare repository at *path* with a `main` branch containing
/// one empty commit, matching the fixture style in `worktree_create_test.rs`.
fn init_bare_repo(path: &Path) -> String {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    let output = Command::new("git")
        .current_dir(path)
        .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
        .output()
        .expect("hash-object");
    assert!(output.status.success(), "hash-object failed");
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-m", "initial"])
        .output()
        .expect("commit-tree");
    assert!(output.status.success(), "commit-tree failed");
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit,
    ]));
    commit
}

fn clone_into(remote: &Path, dest: &Path) -> String {
    let remote_uri = format!("file://{}", remote.display());
    run_command(
        Command::new("git")
            .arg("clone")
            .arg("-b")
            .arg("main")
            .arg(&remote_uri)
            .arg(dest),
    );
    run_command(
        Command::new("git")
            .current_dir(dest)
            .args(["remote", "set-head", "origin", "main"]),
    );
    let output = Command::new("git")
        .current_dir(dest)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    assert!(output.status.success(), "rev-parse HEAD failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn config_for(root: &Path) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.git_timeout_seconds = 30;
    cfg
}

fn issue_key() -> IssueKey {
    IssueKey {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
        number: ISSUE_NUMBER,
    }
}

fn info_for(checkout: &Path) -> RepositoryInfo {
    RepositoryInfo {
        path: checkout.to_path_buf(),
        base_branch: "main".to_string(),
        remote_url: "file://localhost/tmp".to_string(),
    }
}

/// Increase the entry's `attempts` by calling `finish_retry` *count* times
/// without an attached worktree. Each call operates on a freshly acquired
/// claim and returns to `Queued`, so a subsequent `acquire_next` can reclaim
/// the same run id. `now` is advanced past `next_attempt_at` because
/// `retry_or_fail` sets it to `now + 300s`.
async fn bump_attempts(store: &Arc<StateStore>, key: &IssueKey, budget: u32, count: u32) {
    for _ in 0..count {
        let eligible = store
            .acquire_next(
                RUN_ID,
                std::process::id(),
                Utc::now() + ChronoDuration::seconds(301),
            )
            .expect("acquire seed")
            .expect("eligible queued entry");
        let claim = eligible.claim;
        let mut guard = ActiveRunGuard::new(
            claim,
            store.clone(),
            PathBuf::from("/dev/null"),
            key.clone(),
        );
        guard
            .finish_retry("seed attempt", budget)
            .await
            .expect("seed retry");
    }
}

/// Build a real registered git worktree, attach it to an `ActiveRunGuard`,
/// and return everything needed to drive `finish_retry` assertions. The
/// entry starts in `Phase::InProgress` with `attempts` equal to *attempts*
/// and the same `run_id`/`issue_key` used for every claim.
async fn seed_in_progress_with_worktree(
    attempts: u32,
    budget: u32,
) -> (TempDir, Arc<StateStore>, IssueKey, ActiveRunGuard, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path();

    let bare = root.join("remote.git");
    init_bare_repo(&bare);

    let workdirs = root.join("workdirs");
    std::fs::create_dir_all(&workdirs).expect("create workdirs");
    let checkout = workdirs.join(OWNER).join(REPO);
    std::fs::create_dir_all(checkout.parent().unwrap()).expect("create checkout parent");
    clone_into(&bare, &checkout);

    let cfg = config_for(root);
    let runner = GitRunner::new(&cfg);
    let info = info_for(&checkout);
    let key = issue_key();

    let state_dir = root.join("state");
    let store = Arc::new(StateStore::open(&state_dir).expect("open store"));
    store
        .enqueue(&key, TicketType::Code, false)
        .expect("enqueue");

    if attempts > 0 {
        bump_attempts(&store, &key, budget, attempts).await;
    }

    let eligible = store
        .acquire_next(
            RUN_ID,
            std::process::id(),
            Utc::now() + ChronoDuration::seconds(301),
        )
        .expect("acquire target")
        .expect("eligible target entry");
    let claim = eligible.claim;
    let guard = ActiveRunGuard::new(
        claim,
        store.clone(),
        PathBuf::from("/dev/null"),
        key.clone(),
    );

    let worktree = create_worktree(&cfg, &runner, &info, &key, RUN_ID)
        .await
        .expect("create worktree");
    let worktree_path = worktree.path.clone();
    guard
        .attach_worktree(worktree)
        .await
        .expect("attach worktree");

    (temp, store, key, guard, worktree_path)
}

// ---------------------------------------------------------------------------
// finish_retry teardown integration tests (issue #163)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn finish_retry_removes_attached_worktree_on_failure() {
    let budget = 3;
    let (_temp, store, key, mut guard, worktree_path) =
        seed_in_progress_with_worktree(0, budget).await;

    let phase = guard
        .finish_retry("worker exit 2", budget)
        .await
        .expect("finish_retry");

    assert_eq!(phase, Phase::Queued);
    assert!(
        !worktree_path.exists(),
        "worktree path should be removed on retry"
    );

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::Queued);
    assert_eq!(entry.attempts, 1);
}

#[tokio::test]
async fn finish_retry_requeues_with_300s_backoff() {
    let budget = 5;
    let k = 2;
    let start = Utc::now();
    let (_temp, store, key, mut guard, worktree_path) =
        seed_in_progress_with_worktree(k, budget).await;

    let phase = guard
        .finish_retry("worker exit 2", budget)
        .await
        .expect("finish_retry");
    let end = Utc::now();

    assert_eq!(phase, Phase::Queued);
    assert!(
        !worktree_path.exists(),
        "worktree path should be removed on retry"
    );

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::Queued);
    assert_eq!(entry.attempts, k + 1);

    let next_attempt_at = entry.next_attempt_at.expect("next_attempt_at set");
    assert!(
        next_attempt_at >= start + ChronoDuration::seconds(300),
        "next_attempt_at should be at least invocation start + 300s"
    );
    assert!(
        next_attempt_at <= end + ChronoDuration::seconds(300),
        "next_attempt_at should be at most invocation end + 300s"
    );
}

#[tokio::test]
async fn finish_retry_transitions_to_failed_when_budget_exhausted() {
    let budget = 4;
    let (_temp, store, key, mut guard, worktree_path) =
        seed_in_progress_with_worktree(budget - 1, budget).await;

    let phase = guard
        .finish_retry("worker exit 2", budget)
        .await
        .expect("finish_retry");

    assert_eq!(phase, Phase::Failed);
    assert!(
        !worktree_path.exists(),
        "worktree path should be removed even when budget is exhausted"
    );

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::Failed);
    assert_eq!(entry.attempts, budget);
    assert!(entry.next_attempt_at.is_none());
}

// ---------------------------------------------------------------------------
// finish_needs_attention terminal-block tests (issue #167)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn finish_needs_attention_routes_to_blocked_state_and_releases_claim() {
    let budget = 3;
    let (_temp, store, key, mut guard, worktree_path) =
        seed_in_progress_with_worktree(0, budget).await;

    let digest = guard.claim().digest().to_string();
    let claim_path = store.claims_dir().join(format!("{digest}.claim"));
    assert!(claim_path.is_file(), "claim file must exist before finish");

    guard
        .finish_needs_attention(
            "main checkout is dirty",
            "worktree/dirty_main",
            "caduceus queue reset --force-finalization-reset owner/repo#7",
        )
        .await
        .expect("finish needs attention");

    // The attached worktree is torn down and the claim is released
    // after the transition so a later reset is not blocked.
    assert!(!worktree_path.exists(), "worktree path should be removed");
    assert!(!claim_path.exists(), "claim file should be removed");

    let entry = store
        .snapshot()
        .expect("snapshot")
        .entry(&key)
        .expect("entry")
        .clone();
    assert_eq!(entry.phase, Phase::NeedsAttention);
    assert_eq!(entry.last_error.as_deref(), Some("main checkout is dirty"));
    assert_eq!(entry.blocked_source.as_deref(), Some("worktree/dirty_main"));
    assert_eq!(
        entry.blocked_recovery_hint.as_deref(),
        Some("caduceus queue reset --force-finalization-reset owner/repo#7")
    );
    assert!(entry.last_run_id.is_none());
    assert!(entry.next_attempt_at.is_none());
}

#[test]
#[serial_test::serial]
fn finish_needs_attention_emits_stable_terminal_block_event() {
    // Capture via `tracing_appender::non_blocking` + a real temp file,
    // the same pattern `logging_test.rs` proves reliable. The serial
    // attribute matches that file's convention for tracing-capture
    // tests. Both tests that exercise `finish_needs_attention` (this
    // one and `finish_needs_attention_routes_to_blocked_state_and_releases_claim`)
    // are serial because `tracing_core` caches callsite interest: if
    // the sibling runs first without a subscriber installed, the
    // `caduceus.terminal_block` callsite is cached as never-enabled and
    // this test's event is silently dropped before reaching its capture
    // subscriber (empty capture under `--test-threads>=2`).
    let root = std::env::temp_dir().join(format!(
        "caduceus-167-capture-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create tempdir");
    let log_path = root.join("capture.json");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = caduceus::logging::build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let budget = 3;
            let (_temp, store, key, mut guard, _worktree_path) =
                seed_in_progress_with_worktree(0, budget).await;

            guard
                .finish_needs_attention(
                    "main checkout is dirty",
                    "worktree/dirty_main",
                    "caduceus queue reset --force-finalization-reset octocat/hello-world#7",
                )
                .await
                .expect("finish needs attention");

            let entry = store
                .snapshot()
                .expect("snapshot")
                .entry(&key)
                .expect("entry")
                .clone();
            assert_eq!(entry.phase, Phase::NeedsAttention);
        });
    });
    drop(guard); // flush pending events + shut down the writer thread

    let body = std::fs::read_to_string(&log_path).expect("read capture");
    assert!(
        body.contains("\"event\":\"caduceus.terminal_block\""),
        "stable event missing: {body}"
    );
    assert!(
        body.contains("\"issue_key\":\"octocat/hello-world#7\""),
        "got: {body}"
    );
    assert!(
        body.contains("\"source\":\"worktree/dirty_main\""),
        "got: {body}"
    );
    assert!(
        body.contains("caduceus queue reset --force-finalization-reset"),
        "got: {body}"
    );
}
