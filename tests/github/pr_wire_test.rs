//! PR wire-model and endpoint wrapper tests (issue #301).
//!
//! - L1-L5 list: happy path, `next_page` pagination, page cap,
//!   rate-limit exhaustion, malformed page.
//! - F1-F4 fetch: typed parse, nullable `head.repo`, 404 → `Ok(None)`,
//!   closed+merged fixture.
//! - C1-C6 comments: create/update happy path, voice-gate-before-HTTP,
//!   oversize cap, 404 propagation.
//!
//! Follows the `issue_detail_test.rs` client/config construction and
//! the `rate_limit_test.rs` header-carrying mock pattern.

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::github::{
    create_pr_comment, fetch_pull_request, list_pull_requests, update_pr_comment, Client, HttpCache,
};
use chrono::{DateTime, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::{tempdir, MockGitHub};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn mock_client(gh: &MockGitHub) -> (Client, Config) {
    let state_dir = tempdir("pr-wire");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

/// A realistic `/pulls` row. `state == "closed"` implies merged so
/// the closed+merged discrimination fixture shares one builder.
fn pr_json(number: u64, title: &str, state: &str) -> serde_json::Value {
    let mut row = serde_json::json!({
        "number": number,
        "title": title,
        "body": "Fixes the thing",
        "draft": false,
        "user": {"login": "octocat"},
        "state": state,
        "base": {
            "ref": "main",
            "sha": "aaaa",
            "repo": {"full_name": "octocat/hello-world"}
        },
        "head": {
            "ref": "feature-x",
            "sha": "bbbb",
            "repo": {"full_name": "octocat/hello-world"}
        }
    });
    row["merged"] = serde_json::json!(state == "closed");
    if state == "closed" {
        row["merged_at"] = serde_json::json!("2026-07-13T12:00:00Z");
    } else {
        row["merged_at"] = serde_json::Value::Null;
    }
    row
}

// L1: list happy path, one page

#[tokio::test]
async fn list_returns_typed_rows() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        "/repos/octocat/hello-world/pulls",
        serde_json::json!([
            pr_json(12, "Add widget", "open"),
            pr_json(13, "Merge done work", "closed")
        ]),
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let prs = list_pull_requests(&client, "octocat", "hello-world")
        .await
        .expect("list succeeds");
    assert_eq!(prs.len(), 2);

    let first = &prs[0];
    assert_eq!(first.number, Some(12));
    assert_eq!(first.title.as_deref(), Some("Add widget"));
    assert_eq!(first.state.as_deref(), Some("open"));
    assert_eq!(first.merged, Some(false));
    assert!(first.merged_at.is_none());
    assert!(!first.draft);
    assert_eq!(first.author.as_deref(), Some("octocat"));
    assert_eq!(
        first.base.as_ref().and_then(|b| b.ref_name.as_deref()),
        Some("main")
    );
    assert_eq!(
        first
            .head
            .as_ref()
            .and_then(|h| h.repo.as_ref())
            .and_then(|r| r.full_name.as_deref()),
        Some("octocat/hello-world")
    );

    let closed = &prs[1];
    assert_eq!(closed.state.as_deref(), Some("closed"));
    assert_eq!(closed.merged, Some(true));
    assert!(closed.merged_at.is_some());
}

// L2: list pagination

#[tokio::test]
async fn list_follows_next_page() {
    let gh = MockGitHub::start().await;
    gh.mount_paged(
        "/repos/octocat/hello-world/pulls",
        vec![
            vec![pr_json(1, "One", "open")],
            vec![pr_json(2, "Two", "closed")],
        ],
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let prs = list_pull_requests(&client, "octocat", "hello-world")
        .await
        .expect("list succeeds");
    assert_eq!(prs.len(), 2);
    assert_eq!(prs[0].number, Some(1));
    assert_eq!(prs[1].number, Some(2));

    let counts = gh.counts();
    assert_eq!(counts.get, 2, "one GET per page");
    let requests = gh.received_requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].url.as_str().contains("page=2"),
        "page-2 URL carried page=2: {}",
        requests[1].url
    );
}

// L3: list page cap

#[tokio::test]
async fn list_page_cap_errors_instead_of_truncating() {
    let gh = MockGitHub::start().await;
    let pages: Vec<Vec<serde_json::Value>> = (1..=21)
        .map(|i| vec![pr_json(i, &format!("PR {i}"), "open")])
        .collect();
    gh.mount_paged("/repos/octocat/hello-world/pulls", pages)
        .await;

    let (client, _cfg) = mock_client(&gh);
    let err = list_pull_requests(&client, "octocat", "hello-world")
        .await
        .expect_err("page cap must error");
    let text = format!("{err:?}");
    assert!(text.contains("exceeded"), "expected page-cap error: {text}");
    assert_eq!(gh.counts().get, 20, "cap is 20 pages, no silent truncation");
}

// L4: list rate limit

#[tokio::test]
async fn list_rate_limit_zero_remaining_is_typed_error() {
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/pulls"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-limit", "5000")
                .insert_header("x-ratelimit-reset", "0")
                .set_body_string("[]"),
        )
        .mount(gh.server())
        .await;

    let (client, _cfg) = mock_client(&gh);
    let err = list_pull_requests(&client, "octocat", "hello-world")
        .await
        .expect_err("zero remaining errors");
    let text = format!("{err:?}");
    assert!(text.contains("RateLimited"), "expected RateLimited: {text}");
}

// L5: list malformed page

#[tokio::test]
async fn list_malformed_page_is_parse_error() {
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"number": "not-a-number"}]"#))
        .mount(gh.server())
        .await;

    let (client, _cfg) = mock_client(&gh);
    let err = list_pull_requests(&client, "octocat", "hello-world")
        .await
        .expect_err("malformed page errors");
    let text = format!("{err:?}");
    assert!(text.contains("JSON parse"), "expected JSON parse: {text}");
}

// F1: fetch happy path

#[tokio::test]
async fn fetch_returns_typed_detail() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        "/repos/octocat/hello-world/pulls/7",
        pr_json(7, "Add widget", "open"),
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let detail = fetch_pull_request(&client, "octocat", "hello-world", 7)
        .await
        .expect("fetch succeeds")
        .expect("PR exists");
    assert_eq!(detail.number, Some(7));
    assert_eq!(detail.title.as_deref(), Some("Add widget"));
    assert_eq!(detail.state.as_deref(), Some("open"));
    assert_eq!(detail.merged, Some(false));
    assert!(detail.merged_at.is_none());
    assert_eq!(detail.author.as_deref(), Some("octocat"));
    assert_eq!(
        detail
            .base
            .as_ref()
            .and_then(|b| b.repo.as_ref())
            .and_then(|r| r.full_name.as_deref()),
        Some("octocat/hello-world")
    );
    let head = detail.head.as_ref().expect("head present");
    assert_eq!(head.ref_name.as_deref(), Some("feature-x"));
    assert_eq!(head.sha.as_deref(), Some("bbbb"));
    assert_eq!(
        head.repo.as_ref().and_then(|r| r.full_name.as_deref()),
        Some("octocat/hello-world")
    );
}

// F2: fetch with head.repo null (deleted head branch)

#[tokio::test]
async fn fetch_null_head_repo_is_optional_skip() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        "/repos/octocat/hello-world/pulls/7",
        serde_json::json!({
            "number": 7,
            "title": "Deleted head",
            "body": null,
            "draft": false,
            "user": null,
            "state": "open",
            "merged": false,
            "merged_at": null,
            "base": {
                "ref": "main",
                "sha": "aaaa",
                "repo": {"full_name": "octocat/hello-world"}
            },
            "head": {"ref": "deleted-branch", "sha": "bbbb", "repo": null}
        }),
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let detail = fetch_pull_request(&client, "octocat", "hello-world", 7)
        .await
        .expect("fetch succeeds")
        .expect("PR exists");
    let head = detail.head.as_ref().expect("head present");
    assert!(
        head.repo.is_none(),
        "deleted head repo is None, not a crash"
    );
    assert_eq!(head.ref_name.as_deref(), Some("deleted-branch"));
    assert_eq!(detail.author, None, "null user maps to None author");
}

// F3: fetch 404 maps to None

#[tokio::test]
async fn fetch_404_maps_to_none() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "GET",
        "/repos/octocat/hello-world/pulls/7",
        404,
        serde_json::json!({"message": "Not Found"}),
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let detail = fetch_pull_request(&client, "octocat", "hello-world", 7)
        .await
        .expect("404 maps to Ok(None), not an error");
    assert!(detail.is_none());
}

// F4: fetch closed + merged fixture

#[tokio::test]
async fn fetch_closed_merged_round_trips_state_fields() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "GET",
        "/repos/octocat/hello-world/pulls/7",
        pr_json(7, "Merged PR", "closed"),
    )
    .await;

    let (client, _cfg) = mock_client(&gh);
    let detail = fetch_pull_request(&client, "octocat", "hello-world", 7)
        .await
        .expect("fetch succeeds")
        .expect("PR exists");
    assert_eq!(detail.state.as_deref(), Some("closed"));
    assert_eq!(detail.merged, Some(true));
    let expected = DateTime::parse_from_rfc3339("2026-07-13T12:00:00Z")
        .expect("fixture timestamp parses")
        .with_timezone(&Utc);
    assert_eq!(detail.merged_at, Some(expected));
}

// C1: create comment happy path

#[tokio::test]
async fn create_comment_returns_id() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/7/comments",
        201,
        serde_json::json!({"id": 4242}),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let id = create_pr_comment(&client, &cfg, "octocat", "hello-world", 7, "looks good")
        .await
        .expect("create succeeds");
    assert_eq!(id, 4242);
    assert_eq!(gh.counts().post, 1);
    let requests = gh.received_requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = requests[0].body_json().expect("request body is JSON");
    assert_eq!(
        body["body"], "looks good",
        "request body carries exactly the sent text"
    );
}

// C2: create voice rejection before any HTTP

#[tokio::test]
async fn create_comment_voice_rejection_runs_before_http() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/7/comments",
        201,
        serde_json::json!({"id": 4242}),
    )
    .await;

    let (client, mut cfg) = mock_client(&gh);
    cfg.comment_forbidden_strings = vec!["secret".to_string()];
    let err = create_pr_comment(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        7,
        "this contains secret material",
    )
    .await
    .expect_err("forbidden term rejects");
    let text = format!("{err:?}");
    assert!(
        text.contains("public-voice"),
        "expected voice rejection: {text}"
    );
    assert_eq!(gh.counts().post, 0, "voice gate ran before any HTTP");
}

// C3: create oversize cap before any HTTP

#[tokio::test]
async fn create_comment_oversize_rejects_before_http() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "POST",
        "/repos/octocat/hello-world/issues/7/comments",
        201,
        serde_json::json!({"id": 4242}),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let oversized = "x".repeat(70_000);
    let err = create_pr_comment(&client, &cfg, "octocat", "hello-world", 7, &oversized)
        .await
        .expect_err("oversize rejects");
    let text = format!("{err:?}");
    assert!(
        text.contains("public-voice"),
        "expected voice rejection: {text}"
    );
    assert!(
        text.contains("65536"),
        "expected the 65 536-byte cap: {text}"
    );
    assert_eq!(gh.counts().post, 0, "voice gate ran before any HTTP");
}

// C4: update comment happy path

#[tokio::test]
async fn update_comment_succeeds() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/4242",
        serde_json::json!({"id": 4242, "body": "revised"}),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    update_pr_comment(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        4242,
        "revised review",
    )
    .await
    .expect("update succeeds");
    assert_eq!(gh.counts().patch, 1);
    let requests = gh.received_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        "/repos/octocat/hello-world/issues/comments/4242"
    );
}

// C5: update voice rejection before any HTTP

#[tokio::test]
async fn update_comment_voice_rejection_runs_before_http() {
    let gh = MockGitHub::start().await;
    gh.mount(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/4242",
        serde_json::json!({"id": 4242}),
    )
    .await;

    let (client, mut cfg) = mock_client(&gh);
    cfg.comment_forbidden_strings = vec!["secret".to_string()];
    let err = update_pr_comment(
        &client,
        &cfg,
        "octocat",
        "hello-world",
        4242,
        "secret updated review",
    )
    .await
    .expect_err("forbidden term rejects");
    let text = format!("{err:?}");
    assert!(
        text.contains("public-voice"),
        "expected voice rejection: {text}"
    );
    assert_eq!(gh.counts().patch, 0, "voice gate ran before any HTTP");
}

// C6: update non-2xx propagates

#[tokio::test]
async fn update_comment_404_propagates_as_github_api() {
    let gh = MockGitHub::start().await;
    gh.mount_status(
        "PATCH",
        "/repos/octocat/hello-world/issues/comments/4242",
        404,
        serde_json::json!({"message": "Not Found"}),
    )
    .await;

    let (client, cfg) = mock_client(&gh);
    let err = update_pr_comment(&client, &cfg, "octocat", "hello-world", 4242, "updated")
        .await
        .expect_err("404 propagates");
    match err {
        CaduceusError::GitHubApi { status: 404, .. } => {}
        other => panic!("expected GitHubApi 404, got {other:?}"),
    }
}
