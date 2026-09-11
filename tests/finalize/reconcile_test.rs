//! Reconcile-helper tests (issue #396).
//!
//! The ETag-cached GitHub client answers an unchanged GET with
//! HTTP 304 plus the byte-identical cached body. The reconcile
//! reads (`reconcile_pr`, `reconcile_comment`) used to treat any
//! non-200 as a failure, so a crash-resume reconciliation on an
//! unchanged URL failed permanently. These tests pin the fix: a
//! 304 replay must parse the cached body exactly like a 200.

use caduceus::config::Config;
use caduceus::finalize::{reconcile_comment, reconcile_pr, ReconcileResult};
use caduceus::github::{Client, HttpCache};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, Request, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const OWNER: &str = "owner";
const REPO: &str = "repo";
const PR_PATH: &str = "/repos/owner/repo/pulls";
const COMMENTS_PATH: &str = "/repos/owner/repo/issues/1/comments";
const ETAG: &str = "\"abc\"";

/// Matches requests that do NOT carry the named header. The first
/// (unconditional) GET has no `If-None-Match`; the second does. This
/// disambiguates the two mocks on the same path — same pattern as
/// `tests/github/merge_detect_test.rs`.
struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

/// Cache-backed client for the 304-replay tests. The cache must live
/// in an owned `PathBuf` (`fixtures::tempdir`, no auto-cleanup) so it
/// survives past the helper's return — a `tempfile::tempdir()` local
/// binding would drop the cache DB before any HTTP call.
fn mock_client(gh: &MockGitHub) -> Client {
    let state_dir = tempdir("reconcile");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

/// Mount the two-arm conditional-GET sequence on the open-PR list
/// endpoint: 200 + ETag for the unconditional GET, 304 for the
/// re-GET. Query matching mirrors `pr_test.rs`: path + `state=open`
/// only, never the percent-encoded head/base params.
async fn mount_pulls_two_arm(gh: &MockGitHub, pr_body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .and(query_param("state", "open"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", ETAG)
                .set_body_json(pr_body),
        )
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path(PR_PATH))
        .and(query_param("state", "open"))
        .and(header("if-none-match", ETAG))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(gh.server())
        .await;
}

/// Same two-arm sequence on the issue-comments endpoint.
async fn mount_comments_two_arm(gh: &MockGitHub, comments_body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(COMMENTS_PATH))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", ETAG)
                .set_body_json(comments_body),
        )
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path(COMMENTS_PATH))
        .and(header("if-none-match", ETAG))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(gh.server())
        .await;
}

#[tokio::test]
async fn reconcile_pr_304_replay_already_applied() {
    let gh = MockGitHub::start().await;
    mount_pulls_two_arm(
        &gh,
        serde_json::json!([
            { "number": 7, "html_url": "https://github.com/owner/repo/pull/7" }
        ]),
    )
    .await;

    let client = mock_client(&gh);
    let first = reconcile_pr(
        &client,
        OWNER,
        REPO,
        "automation/issue-1-run-x",
        "main",
        Some("7"),
    )
    .await
    .expect("fresh list reconciles as applied");
    assert_eq!(first, ReconcileResult::AlreadyApplied);

    // The conditional re-GET 304s and the client replays the cached
    // body. This reconcile must NOT fail (on current main the second
    // call returns Err(GitHubApi { status: 304 })).
    let second = reconcile_pr(
        &client,
        OWNER,
        REPO,
        "automation/issue-1-run-x",
        "main",
        Some("7"),
    )
    .await
    .expect("304 replay must not fail (issue #396)");
    assert_eq!(second, ReconcileResult::AlreadyApplied);
}

#[tokio::test]
async fn reconcile_comment_304_replay_already_applied() {
    let gh = MockGitHub::start().await;
    mount_comments_two_arm(
        &gh,
        serde_json::json!([
            { "id": 1, "body": "<!-- automation-run:run-x\n\nsummary" }
        ]),
    )
    .await;

    let client = mock_client(&gh);
    let first = reconcile_comment(&client, OWNER, REPO, 1, "run-x", "<!-- automation-run:")
        .await
        .expect("fresh list reconciles as applied");
    assert_eq!(first, ReconcileResult::AlreadyApplied);

    let second = reconcile_comment(&client, OWNER, REPO, 1, "run-x", "<!-- automation-run:")
        .await
        .expect("304 replay must not fail (issue #396)");
    assert_eq!(second, ReconcileResult::AlreadyApplied);
}
