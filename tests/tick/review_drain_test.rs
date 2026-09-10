//! Step-6.5a review drain tests (issue #339, DAR §5): the tick's
//! review-claim loop bounded by `max_reviews_per_tick`, gated by
//! `auto_review.enabled`, and inert in dry-run mode.
//!
//! Entries are seeded DIRECTLY into the review store (the claim path
//! is exercised; the admission path is review_discovery_test's job).
//! Discovery sees an empty `/pulls` list, so no mirror work happens;
//! each claimed entry hits `GET /pulls/{n}` → 404 and quiet-skips
//! (`ReviewGone` → `finish_skip`), which is enough to observe the
//! drain loop's claim/budget behaviour end-to-end through the real
//! tick.

use std::sync::Arc;

use caduceus::config::{AutoReviewConfig, Config, LoadContext, RawConfig};
use caduceus::github::{Client, HttpCache};
use caduceus::meta::TickOutcome;
use caduceus::orchestration::SystemClock;
use caduceus::review::{RepositoryId, ReviewTarget};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::state::review::{ReviewPhase, ReviewStore};
use caduceus::worktree::GitRunner;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

fn repo_id() -> RepositoryId {
    RepositoryId {
        owner: "owner".to_string(),
        repo: "r".to_string(),
    }
}

fn seed_review_entries(state_dir: &std::path::Path, count: u64) {
    let store = ReviewStore::open(state_dir).expect("review store opens");
    for n in 1..=count {
        let sha = format!("{:040x}", n);
        store
            .enqueue_review(&ReviewTarget {
                repository: repo_id(),
                pull_request: n,
                head_sha: sha.clone(),
                base_sha: sha.clone(),
                base_ref: "main".to_string(),
                merge_base: sha.clone(),
            })
            .expect("enqueue review target");
    }
}

fn tick_cfg(
    base: &std::path::Path,
    api_base: &str,
    auto_review: Option<AutoReviewConfig>,
) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(base.join("state")),
        workdir_base: Some(base.to_path_buf()),
        watched_repos: Some(vec!["owner/r".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(base.to_path_buf()),
        ..Default::default()
    };
    let mut cfg = Config::from_raw(raw, &ctx).expect("config");
    cfg.api_base = api_base.to_string();
    cfg.auto_review = auto_review;
    cfg
}

async fn run_tick(
    cfg: Config,
    server: &MockServer,
    pool: Arc<Pool>,
) -> caduceus::error::CaduceusResult<TickOutcome> {
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;
    let mut cfg = cfg;
    cfg.api_base = server.uri();
    let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let clock: Arc<dyn caduceus::orchestration::Clock> = Arc::new(SystemClock);
    let git = GitRunner::new(&cfg);
    let services = caduceus::orchestration::Services::production(
        &cfg,
        clock,
        Arc::new(client),
        git,
        Arc::clone(&pool),
        Arc::new(caduceus::infra::disk::DiskPressureGuard::disabled()),
    );
    caduceus::tick::tick(cfg, services, pool, CancellationToken::new()).await
}

fn make_pool(cfg: &Config) -> Arc<Pool> {
    Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    )
}

fn review_phases(state_dir: &std::path::Path) -> Vec<ReviewPhase> {
    ReviewStore::open(state_dir)
        .expect("review store opens")
        .review_queue_snapshot()
        .expect("review queue snapshot")
        .entries
        .values()
        .map(|e| e.phase)
        .collect()
}

fn counts(phases: Vec<ReviewPhase>) -> (usize, usize, usize) {
    let queued = phases.iter().filter(|p| **p == ReviewPhase::Queued).count();
    let skipped = phases
        .iter()
        .filter(|p| **p == ReviewPhase::Skipped)
        .count();
    let other = phases.len() - queued - skipped;
    (queued, skipped, other)
}

#[tokio::test]
async fn budget_limits_reviews_claimed_per_tick() {
    let base = tempdir("review-drain-budget");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.max_reviews_per_tick = 2;
    cfg.git_timeout_seconds = 30;
    seed_review_entries(&cfg.state_dir, 3);

    // Discovery sees no pull requests (no admission work); each
    // claimed entry's PR fetch 404s → quiet skip.
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/owner/r/pulls/[0-9]+"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let outcome = run_tick(cfg.clone(), &server, make_pool(&cfg))
        .await
        .expect("tick runs");
    // The tick's final outcome derives from the POLL phase (any_200
    // from the empty 200 issue list), mirroring the issue drain where
    // claim outcomes are folded via http_status/last_error, not the
    // outcome enum. The review drain's contract is observable in the
    // QUEUE: exactly `max_reviews_per_tick` entries claimed.
    let _ = outcome;

    let (queued, skipped, other) = counts(review_phases(&cfg.state_dir));
    assert_eq!(queued, 1, "the third entry must stay queued (budget = 2)");
    assert_eq!(skipped, 2, "two entries must be claimed and skipped");
    assert_eq!(other, 0);
}

#[tokio::test]
async fn disabled_auto_review_never_claims() {
    let base = tempdir("review-drain-disabled");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), None);
    cfg.max_reviews_per_tick = 0;
    seed_review_entries(&cfg.state_dir, 2);

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let outcome = run_tick(cfg.clone(), &server, make_pool(&cfg))
        .await
        .expect("tick runs");
    assert_eq!(outcome, TickOutcome::IdleEmpty);

    let (queued, skipped, other) = counts(review_phases(&cfg.state_dir));
    assert_eq!(queued, 2, "no claims when auto_review is disabled");
    assert_eq!(skipped, 0);
    assert_eq!(other, 0);
}

#[tokio::test]
async fn dry_run_leaves_review_queue_untouched() {
    let base = tempdir("review-drain-dry-run");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.dry_run = true;
    cfg.max_reviews_per_tick = 0;
    seed_review_entries(&cfg.state_dir, 2);

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let outcome = run_tick(cfg.clone(), &server, make_pool(&cfg))
        .await
        .expect("tick runs");
    assert_eq!(outcome, TickOutcome::IdleEmpty);

    let (queued, skipped, other) = counts(review_phases(&cfg.state_dir));
    assert_eq!(queued, 2, "dry-run must not claim review entries");
    assert_eq!(skipped, 0);
    assert_eq!(other, 0);
}

fn ar_config(enabled: bool) -> AutoReviewConfig {
    AutoReviewConfig {
        enabled,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
    }
}

#[tokio::test]
async fn pool_saturation_requeues_review_with_backoff() {
    // Hold the only worker-pool permit BEFORE the tick: the review
    // drain's `pool.admit` must fail with PoolSaturated and requeue
    // the claimed entry through `finish_infrastructure` (backoff,
    // attempts untouched — SCHED-001 mirror of the issue arm).
    let base = tempdir("review-drain-saturated");
    let server = MockServer::start().await;
    let mut cfg = tick_cfg(&base, &server.uri(), Some(ar_config(true)));
    cfg.worker_parallelism = 1;
    cfg.max_reviews_per_tick = 0;
    cfg.git_timeout_seconds = 30;
    seed_review_entries(&cfg.state_dir, 1);

    let pool = make_pool(&cfg);
    let _held_permit = pool
        .admit("repo:owner/r", "owner/r")
        .await
        .expect("test holds the only permit");

    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let _ = run_tick(cfg.clone(), &server, pool)
        .await
        .expect("tick runs");

    let (queued, skipped, other) = counts(review_phases(&cfg.state_dir));
    assert_eq!(queued, 1, "saturated claim must requeue, not skip or run");
    assert_eq!(skipped, 0);
    assert_eq!(other, 0);

    // The requeue is infrastructure-classed: attempts unchanged,
    // backoff scheduled.
    let entry = ReviewStore::open(&cfg.state_dir)
        .expect("review store opens")
        .review_queue_snapshot()
        .expect("snapshot")
        .entries
        .values()
        .find(|e| e.target.pull_request == 1)
        .expect("review entry")
        .clone();
    assert_eq!(
        entry.attempts, 0,
        "pool saturation is not worker-attributable"
    );
    assert!(entry.next_attempt_at.is_some(), "backoff must be scheduled");
    assert!(
        entry
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("pool saturated"),
        "requeue reason must record the pool-saturation error"
    );
}
