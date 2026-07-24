//! - Complete parse (issue + comments + events).
//! - Null body / user tolerance.
//! - Empty data (no comments, no events).
//! - Chronological normalisation (most recent 100 retained).
//! - Multi-page comments (paginated across two pages).
//! - Malformed comments (JSON parse error).
//! - 404 on the issue path.
//! - Rate-limit on one branch of the join (only one of the
//!   three concurrent requests trips the rate-limit).
//! - `Serialize` round-trip for context construction.

use std::path::PathBuf;

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache, ACCEPT_VALUE};
use caduceus::issue::{fetch_issue_detail, IssueComment, IssueDetail, IssueEvent, IssueKey};
use chrono::{TimeZone, Utc};
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-issue-detail-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn mock_client(server: &MockServer) -> (Client, Config) {
    let state_dir = tempdir("mock");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

fn issue_body(title: &str, body: &str, labels: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "number": 7,
        "title": title,
        "body": body,
        "labels": labels.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>(),
        "state": "open"
    })
}

fn comment_body(author: &str, body: &str, created_at: &str) -> serde_json::Value {
    serde_json::json!({
        "body": body,
        "user": {"login": author},
        "created_at": created_at
    })
}

fn event_body(kind: &str, actor: &str, created_at: &str, label: Option<&str>) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "event": kind,
        "actor": {"login": actor},
        "created_at": created_at
    });
    if let Some(name) = label {
        obj["label"] = serde_json::json!({"name": name});
    }
    obj
}

// Complete parse

#[tokio::test]
async fn complete_parse_returns_typed_detail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body(
            "Fix login",
            "Login is broken when...",
            &["bug", "🤖 auto-fix"],
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            comment_body("octocat", "I can reproduce", "2026-07-13T12:00:00Z"),
            comment_body("hacker", "Unrelated remark", "2026-07-13T13:00:00Z")
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([event_body(
                "labeled",
                "octocat",
                "2026-07-13T11:00:00Z",
                Some("🤖 auto-fix")
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect("fetch succeeds");
    assert_eq!(detail.title, "Fix login");
    assert_eq!(detail.body, "Login is broken when...");
    assert_eq!(
        detail.labels,
        vec!["bug".to_string(), "🤖 auto-fix".to_string()]
    );
    assert_eq!(detail.comments.len(), 2);
    assert!(detail
        .events
        .iter()
        .any(|e| e.kind == "labeled" && e.label_name.as_deref() == Some("🤖 auto-fix")));
}

// Null body / user tolerance

#[tokio::test]
async fn null_body_and_user_are_tolerated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"title":null,"body":null,"labels":null,"state":"open"}"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"[{"body":null,"user":null,"created_at":null}]"#),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect("null fields are tolerated");
    assert_eq!(detail.title, "");
    assert_eq!(detail.body, "");
    assert!(detail.labels.is_empty());
    assert_eq!(detail.comments.len(), 1);
    assert_eq!(detail.comments[0].author, "");
    assert_eq!(detail.comments[0].body, "");
    assert!(detail.events.is_empty());
}

// Empty data

#[tokio::test]
async fn empty_comments_and_events_produce_empty_vecs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body("Empty", "", &[])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect("empty is fine");
    assert!(detail.comments.is_empty());
    assert!(detail.events.is_empty());
}

// Chronological normalisation (most recent 100)

#[tokio::test]
async fn chronological_normalisation_retains_most_recent_one_hundred() {
    // Build 150 comments; only the 100 most recent (i.e. with the
    // highest `created_at`) survive the cap.
    let server = MockServer::start().await;
    let mut all: Vec<serde_json::Value> = (0..150)
        .map(|i| {
            let ts = Utc.timestamp_opt(1_700_000_000 + i, 0).unwrap();
            let s = ts.to_rfc3339();
            comment_body("octocat", &format!("Comment {i}"), &s)
        })
        .collect();
    // Send the first 100; the test will then send a second page
    // (or rely on Link-header pagination). To keep the wire
    // simple, send all 150 in the first page; the client should
    // still cap at 100.
    let page_body = serde_json::Value::Array(all.clone());
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body("Title", "Body", &[])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_body))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;
    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect("cap test");
    assert_eq!(detail.comments.len(), 100);
    // The retained comments must be the most recent 100, in
    // chronological order.
    let first = &detail.comments[0].body;
    let last = &detail.comments[99].body;
    assert_eq!(first, "Comment 50");
    assert_eq!(last, "Comment 149");
    // Sanity: list is ascending.
    for w in detail.comments.windows(2) {
        assert!(w[0].created_at <= w[1].created_at);
    }
    let _ = all.pop();
}

// Multi-page comments

#[tokio::test]
async fn multi_page_comments_merged_in_order() {
    let server = MockServer::start().await;
    let next_url = format!(
        "{}/repos/octocat/hello-world/issues/7/comments?page=2&per_page=100",
        server.uri()
    );
    let link_header = format!("<{next_url}>; rel=\"next\"");
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body("Title", "Body", &[])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .set_body_json(serde_json::json!([
                    comment_body("octocat", "First", "2026-07-13T10:00:00Z"),
                    comment_body("octocat", "Second", "2026-07-13T11:00:00Z")
                ])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            comment_body("octocat", "Third", "2026-07-13T12:00:00Z"),
            comment_body("octocat", "Fourth", "2026-07-13T13:00:00Z")
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect("multi-page fetch");
    let bodies: Vec<&str> = detail.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(bodies, vec!["First", "Second", "Third", "Fourth"]);
}

// Malformed comments

#[tokio::test]
async fn malformed_comments_is_a_transient_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body("Title", "Body", &[])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not an array"))
        .expect(1)
        .mount(&server)
        .await;
    // The events branch is short-circuited by try_join3 when
    // the comments parse errors, so this mock may be hit zero
    // or one time depending on poll order. Mount without
    // `expect(N)` so the test is not brittle.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let err = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect_err("malformed comments fail");
    let text = format!("{err:?}");
    assert!(text.contains("JSON parse"), "expected parse error: {text}");
}

// 404 on the issue path

#[tokio::test]
async fn four_oh_four_on_issue_path_surfaces_github_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(404).set_body_string("{}"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let err = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect_err("404 must surface");
    let text = format!("{err:?}");
    assert!(text.contains("404"), "expected 404: {text}");
}

// Rate limit on one branch of the join

#[tokio::test]
async fn rate_limit_on_one_branch_surfaces_rate_limited_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_body("Title", "Body", &[])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/comments"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues/7/events"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "0"),
        )
        .mount(&server)
        .await;

    let (client, cfg) = mock_client(&server);
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let err = fetch_issue_detail(&client, &key, &cfg.feedback_author_allowlist)
        .await
        .expect_err("rate-limited event fetch fails");
    let text = format!("{err:?}");
    assert!(text.contains("RateLimited"), "expected RateLimited: {text}");
}

// Serialize round-trip

#[test]
fn issue_detail_serialize_round_trip() {
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let detail = IssueDetail {
        key: key.clone(),
        title: "Fix".to_string(),
        body: "body".to_string(),
        labels: vec!["bug".to_string()],
        comments: vec![IssueComment {
            author: "octocat".to_string(),
            body: "first".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }],
        trusted_comments: vec![],
        events: vec![IssueEvent {
            kind: "labeled".to_string(),
            actor: "octocat".to_string(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            label_name: Some("bug".to_string()),
        }],
        fetched_at: Utc.timestamp_opt(1_700_000_001, 0).unwrap(),
    };
    let json = detail.to_context_json().expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json is valid");
    assert_eq!(parsed["title"], "Fix");
    assert_eq!(parsed["key"]["owner"], "octocat");
    assert_eq!(parsed["comments"][0]["body"], "first");
    assert_eq!(parsed["events"][0]["kind"], "labeled");
    assert_eq!(parsed["events"][0]["label_name"], "bug");
    // Round-trip back to typed.
    let back: IssueDetail = serde_json::from_str(&json).expect("round-trip");
    assert_eq!(back, detail);
    // Silence unused-import warning if the constant isn't referenced
    // elsewhere in the test binary.
    let _ = ACCEPT_VALUE;
}
