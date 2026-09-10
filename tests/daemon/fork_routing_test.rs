//! Fork gate routing tests (issue #337, Phase 2).
//!
//! Proves the routing contract at `classify_discovery_row`'s fork-gate
//! arm and at the discovery emit site:
//!
//! - a denied fork keeps the Phase-1 `RowAction::SkipFork` and the
//!   exact `review_skipped_fork_unsupported` event payload
//!   (byte-for-byte regression);
//! - an ALLOWED fork (its slug in `auto_review.fork_policy.
//!   allow_fork_prs`) routes to `RowAction::AdmitFork { head_repo }`
//!   — the quarantine-fetch admission path;
//! - the allow-list is an exact per-repo opt-in: only the listed slug
//!   admits, a same-repo row is never `AdmitFork`, and a row with no
//!   head repo (deleted head branch) can never admit.

use std::path::Path;

use caduceus::config::{AutoReviewConfig, Config, ForkPolicy};
use caduceus::daemon::tick::review_discovery::{
    classify_discovery_row_for_tests, poll_review_step_for_tests, HeldShas, RowAction,
};
use caduceus::error::CaduceusError;
use caduceus::github::fork_gate::{emit_fork_gate_skip, FORK_SKIP_EVENT};
use caduceus::github::{Client, HttpCache};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::state::review::ReviewStore;
use caduceus::worktree::GitRunner;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

/// A `/pulls` row. `None` renders `"repo": null` (the deleted-head
/// wire shape). Defaults: number 7, open, non-draft, SHAs `aaaa` /
/// `bbbb` (review_discovery_test.rs::pr_row shape).
fn pr_row(head_repo: Option<&str>, base_repo: Option<&str>) -> serde_json::Value {
    let mut row = serde_json::json!({
        "number": 7,
        "title": "Discovery row",
        "state": "open",
        "draft": false,
        "base": {"ref": "main", "sha": "aaaa"},
        "head": {"ref": "feature-x", "sha": "bbbb"}
    });
    row["base"]["repo"] = base_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row["head"]["repo"] = head_repo
        .map(|name| serde_json::json!({"full_name": name}))
        .unwrap_or(serde_json::Value::Null);
    row
}

/// An `auto_review` config with a `fork_policy` allow-list.
fn ar_config(allow_fork_prs: &[&str]) -> AutoReviewConfig {
    AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: Some(ForkPolicy {
            allow_fork_prs: allow_fork_prs.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

/// Config with `auto_review` enabled and discovery pointed at the
/// wiremock base (fork_adversarial_test.rs::discovery_config shape).
fn discovery_config(root: &Path, api_base: &str) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.watched_repos = vec!["owner/r".to_string()];
    cfg.auto_review = Some(AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
    });
    cfg
}

fn empty_held() -> HeldShas {
    HeldShas {
        active: Vec::new(),
        last_reviewed: None,
    }
}

// ---------------------------------------------------------------------------
// Classification (pure seam)
// ---------------------------------------------------------------------------

#[test]
fn denied_fork_row_keeps_phase1_skip_classification() {
    let row = pr_row(Some("forkuser/r"), Some("owner/r"));
    let row: caduceus::github::pr::PullRequestDetail =
        serde_json::from_value(row).expect("row parses");
    let decision = classify_discovery_row_for_tests(&row, &ar_config(&[]), &empty_held());
    assert_eq!(
        decision.action,
        RowAction::SkipFork {
            head_repo: Some("forkuser/r".to_string()),
        },
        "an empty allow-list must preserve Phase-1 SkipFork exactly"
    );
    assert!(
        !matches!(decision.action, RowAction::AdmitFork { .. }),
        "no policy = no quarantine admission"
    );
}

#[test]
fn allowed_fork_row_routes_to_admit_fork() {
    let row = pr_row(Some("forkuser/r"), Some("owner/r"));
    let row: caduceus::github::pr::PullRequestDetail =
        serde_json::from_value(row).expect("row parses");
    let decision = classify_discovery_row_for_tests(&row, &ar_config(&["owner/r"]), &empty_held());
    assert_eq!(
        decision.action,
        RowAction::AdmitFork {
            head_repo: "forkuser/r".to_string(),
        },
        "the watched repo's slug opts its forks INTO quarantine admission"
    );
}

#[test]
fn allow_list_is_exact_slug_opt_in() {
    let row = pr_row(Some("forkuser/r"), Some("owner/r"));
    let row: caduceus::github::pr::PullRequestDetail =
        serde_json::from_value(row).expect("row parses");
    // A different repo's slug does NOT opt owner/r's forks in.
    let decision =
        classify_discovery_row_for_tests(&row, &ar_config(&["other/repo"]), &empty_held());
    assert_eq!(
        decision.action,
        RowAction::SkipFork {
            head_repo: Some("forkuser/r".to_string()),
        },
        "allow-list membership is per-repo and exact"
    );
}

#[test]
fn same_repo_row_never_admit_fork() {
    let row = pr_row(Some("owner/r"), Some("owner/r"));
    let row: caduceus::github::pr::PullRequestDetail =
        serde_json::from_value(row).expect("row parses");
    let decision = classify_discovery_row_for_tests(&row, &ar_config(&["owner/r"]), &empty_held());
    assert_eq!(
        decision.action,
        RowAction::Admit,
        "a same-repo row is never routed through the quarantine path"
    );
}

#[test]
fn missing_head_repo_never_admit_fork() {
    // Deleted head branch: `head.repo: null` (HeadRepoMissing). Even
    // with the slug allowed, there is no fork identity to resolve —
    // the row keeps the Phase-1 skip with `head_repo: None`.
    let row = pr_row(None, Some("owner/r"));
    let row: caduceus::github::pr::PullRequestDetail =
        serde_json::from_value(row).expect("row parses");
    let decision = classify_discovery_row_for_tests(&row, &ar_config(&["owner/r"]), &empty_held());
    assert_eq!(
        decision.action,
        RowAction::SkipFork { head_repo: None },
        "a row without a head repo can never admit through quarantine"
    );
}

// ---------------------------------------------------------------------------
// Emit-site regression (serial: the traced callsite is cached
// process-wide — the #167 finding)
// ---------------------------------------------------------------------------

/// Capture JSON tracing lines emitted while `body` runs.
fn capture_events<F: FnOnce()>(body: F) -> Vec<serde_json::Value> {
    let root = tempdir("fork-routing-capture");
    let capture = root.join("events.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture file");
    let (writer, appender_guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        body();
    }
    drop(appender_guard);
    let body = std::fs::read_to_string(&capture).expect("read capture");
    body.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The fork-skip event fields as emitted today (Phase-1 contract).
fn phase1_skip_fields() -> serde_json::Value {
    let events = capture_events(|| {
        emit_fork_gate_skip("owner/r", 7, Some("forkuser/r"));
    });
    let line = events
        .iter()
        .find(|v| v["fields"]["event"].as_str() == Some(FORK_SKIP_EVENT))
        .unwrap_or_else(|| panic!("no {FORK_SKIP_EVENT} event in capture"));
    line["fields"].clone()
}

#[test]
#[serial_test::serial]
fn denied_fork_emit_payload_matches_phase1() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(async {
        let root = tempdir("fork-routing-emit");
        let server = MockServer::start().await;
        let mut cfg = discovery_config(&root, &server.uri());
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;

        // A fork row (forkuser/r → owner/r).
        let fork_row = pr_row(Some("forkuser/r"), Some("owner/r"));
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([fork_row])))
            .mount(&server)
            .await;

        let store = ReviewStore::open(&root.join("state")).expect("review store opens");
        let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
        let client = Client::with_cache(&cfg, cache).expect("client builds");
        // The base remote resolver must NEVER be called for a denied
        // fork — classification skips before any mirror work.
        let stats = poll_review_step_for_tests(
            &["owner/r".to_string()],
            &client,
            &cfg,
            &store,
            &GitRunner::new(&cfg),
            &|owner, repo| {
                Err(CaduceusError::Config(format!(
                    "resolver must not be called for a denied fork: {owner}/{repo}"
                )))
            },
            &|_repository: &caduceus::review::RepositoryId, _head_repo: &str| None,
        )
        .await
        .expect("fork skip is not a step error");
        (root, stats)
    });
    let (_root, stats) = result;

    assert_eq!(stats.skipped_fork, 1, "the denied fork skips exactly once");
    assert_eq!(stats.admitted, 0, "a denied fork is never admitted");

    // The payload must be byte-for-byte the Phase-1 emit: the same
    // `emit_fork_gate_skip(owner/r, 7, Some(forkuser/r))` call with
    // the same fields. Compare the `fields` objects from a direct
    // Phase-1 emit and from the real poll run.
    let expected = phase1_skip_fields();
    let events = capture_events(|| {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async {
            // Re-run the identical poll under the capture subscriber.
            let root = tempdir("fork-routing-emit-2");
            let server = MockServer::start().await;
            let mut cfg = discovery_config(&root, &server.uri());
            cfg.repo_storage_root = root.join("repos");
            cfg.git_timeout_seconds = 30;
            let fork_row = pr_row(Some("forkuser/r"), Some("owner/r"));
            Mock::given(method("GET"))
                .and(path("/repos/owner/r/pulls"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!([fork_row])),
                )
                .mount(&server)
                .await;
            let store = ReviewStore::open(&root.join("state")).expect("review store opens");
            let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
            let client = Client::with_cache(&cfg, cache).expect("client builds");
            let _ = poll_review_step_for_tests(
                &["owner/r".to_string()],
                &client,
                &cfg,
                &store,
                &GitRunner::new(&cfg),
                &|_o, _r| Err(CaduceusError::Config("unused".to_string())),
                &|_repository: &caduceus::review::RepositoryId, _head_repo: &str| None,
            )
            .await
            .expect("step returns Ok");
        });
    });
    let actual = events
        .iter()
        .find(|v| v["fields"]["event"].as_str() == Some(FORK_SKIP_EVENT))
        .unwrap_or_else(|| panic!("no {FORK_SKIP_EVENT} event in poll capture"))
        .get("fields")
        .cloned()
        .expect("fields present");
    assert_eq!(
        actual, expected,
        "the poll-run {FORK_SKIP_EVENT} payload must match the Phase-1 emit byte-for-byte"
    );
}
