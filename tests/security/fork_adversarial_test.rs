//! Fork adversarial integration (issue #324, DAR §11.2).
//!
//! Proves a fork PR fixture flows through the discovery loop and is
//! skipped with `review_skipped_fork_unsupported` WITHOUT being
//! enqueued — the worker is never dispatched. The unit predicate is
//! tested in `tests/github/fork_gate_test.rs`; the `RowAction` is
//! tested in `tests/daemon/review_discovery_test.rs:fork_skips_...`;
//! THIS test proves the end-to-end discovery loop holds against the
//! #327 `fork.json` wire fixture.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use caduceus::config::Config;
use caduceus::daemon::tick::review_discovery::poll_review_step_for_tests;
use caduceus::error::CaduceusError;
use caduceus::github::fork_gate::FORK_SKIP_EVENT;
use caduceus::github::{Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::state::review::ReviewStore;
use caduceus::worktree::GitRunner;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const FORK_ROW: &str = include_str!("../fixtures/pr/fork.json");

/// Config with `auto_review` enabled and discovery pointed at the
/// wiremock base (review_discovery_test.rs::discovery_config shape).
fn discovery_config(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.watched_repos = vec!["owner/r".to_string()];
    cfg.auto_review = Some(caduceus::config::AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
        publication_mode: caduceus::config::PublicationMode::Update,
    });
    cfg
}

/// Capture the fork-skip event through the serial +
/// `tracing_appender::non_blocking` discipline (`tracing_core` caches
/// callsite interest process-wide — the #167 finding; the same
/// callsite is exercised by `fork_gate_test.rs` and
/// `review_discovery_test.rs`, so this must not run concurrently).
#[test]
#[serial_test::serial]
fn fork_pr_fixture_never_enqueued() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(async {
        let root = tempdir("fork-adversarial");
        let server = MockServer::start().await;
        let mut cfg = discovery_config(&root, &server.uri());
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;

        // The fork row comes off the wire exactly as #327 pinned it.
        let fork_row: serde_json::Value =
            serde_json::from_str(FORK_ROW).expect("fork fixture parses");
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([fork_row])))
            .mount(&server)
            .await;

        let store = ReviewStore::open(&root.join("state")).expect("review store opens");
        let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
        let client = Client::with_cache(&cfg, cache).expect("client builds");
        // The remote resolver must NEVER be called: a fork is skipped
        // at classification, before any mirror work (the whole point
        // of the Phase-1 gate — the daemon's single-origin mirror
        // cannot check out fork SHAs).
        let stats = poll_review_step_for_tests(
            &["owner/r".to_string()],
            &client,
            &cfg,
            &store,
            &GitRunner::new(&cfg),
            &|owner, repo| {
                Err(CaduceusError::Config(format!(
                    "resolver must not be called for a fork: {owner}/{repo}"
                )))
            },
            &|_repository: &caduceus::review::RepositoryId, _head_repo: &str| {
                Box::pin(async { None })
                    as Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
            },
        )
        .await
        .expect("fork skip is not a step error");

        (root, cfg, store, stats)
    });

    let (_root, _cfg, store, stats) = result;

    // AC 4 (issue #324): the fork PR never reaches the worker.
    assert_eq!(
        stats.skipped_fork, 1,
        "the fork fixture must be skipped by the fork gate"
    );
    assert_eq!(
        stats.admitted, 0,
        "a fork PR must never be admitted for review"
    );
    assert!(
        store
            .review_queue_snapshot()
            .expect("snapshot")
            .entries
            .is_empty(),
        "the queue must stay empty: nothing enqueued for the fork PR"
    );
}

/// The skip event's structured shape (repo, pr, head-repo identity).
/// Separate test so the event capture owns its own subscriber; still
/// serial (same `emit_fork_gate_skip` callsite as the test above).
#[test]
#[serial_test::serial]
fn fork_skip_event_carries_head_repo_identity() {
    let root = tempdir("fork-adversarial-event");
    let log_path = root.join("fork-skip.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    tracing::subscriber::with_default(subscriber, || {
        caduceus::github::fork_gate::emit_fork_gate_skip("owner/r", 7, Some("forkuser/r"));
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read capture file");
    assert!(
        body.contains(&format!("\"event\":\"{FORK_SKIP_EVENT}\"")),
        "fork-skip event missing: {body}"
    );
    assert!(body.contains("\"repo\":\"owner/r\""), "got: {body}");
    assert!(body.contains("\"pr\":7"), "got: {body}");
    assert!(
        body.contains("\"head_repo\":\"forkuser/r\""),
        "head-repo identity must be carried: {body}"
    );
}
