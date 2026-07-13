//! Task 2.3 acceptance tests for labeled-issue polling.
//!
//! Covers every assertion in the task packet:
//!
//! - Unicode URL encoding for label query strings
//! - Code/investigation merge (single-label on each side)
//! - Both-label rejection (ambiguous diagnostic)
//! - Server returning an unrelated-label object (Unmatched)
//! - PR exclusion (PullRequest diagnostic)
//! - Empty / null body tolerance (missing title, missing labels,
//!   missing updated_at)
//! - Malformed number (Malformed diagnostic)
//! - Pagination in either query
//! - 304 reuse (the cache layer serves the second poll verbatim)
//! - No Events API fields (the typed schema decodes only the
//!   documented fields and silently ignores Events-API noise)

use std::path::PathBuf;

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache};
use caduceus::issue::IssueKey;
use caduceus::poll::{
    merge_outcomes, poll_code, poll_investigation, url_encode_label, IssuePollDiagnostic,
    IssuePollOutcome, IssueSummary,
};
use caduceus::queue::TicketType;
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const CODE_LABEL: &str = "🤖 auto-fix";
const INVESTIGATION_LABEL: &str = "🤖 auto-fix-investigate";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-issue-poll-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn mock_client(server: &MockServer) -> (Client, Config) {
    let state_dir = tempdir("mock");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    cfg.ticket_label_code = CODE_LABEL.to_string();
    cfg.ticket_label_investigation = INVESTIGATION_LABEL.to_string();
    cfg.watched_repos.clear();
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

/// Build a JSON body with realistic GitHub issue-list entries.
fn issue_list_json(entries: &[serde_json::Value]) -> serde_json::Value {
    serde_json::Value::Array(entries.to_vec())
}

fn minimal_issue(number: u64, title: &str, label_names: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": title,
        "labels": label_names
            .iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect::<Vec<_>>(),
        "updated_at": "2026-07-13T12:00:00Z",
        "user": {"login": "octocat"}
    })
}

fn pull_request_issue(number: u64, title: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": title,
        "labels": [],
        "updated_at": "2026-07-13T12:00:00Z",
        "pull_request": {"url": "https://api.github.com/repos/octocat/hello-world/pulls/1"}
    })
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

#[test]
fn url_encoded_label_handles_emoji_and_ascii() {
    assert_eq!(url_encode_label("bug"), "bug");
    assert_eq!(url_encode_label("🤖 auto-fix"), "%F0%9F%A4%96%20auto-fix");
    // The slash in "feature/foo" must be percent-encoded so the
    // URL has exactly one path segment per label.
    assert_eq!(url_encode_label("feature/foo"), "feature%2Ffoo");
}

#[tokio::test]
async fn code_label_query_carries_percent_encoded_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;

    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let _ = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();

    let received = server.received_requests().await.expect("received");
    let url = received[0].url.as_str();
    assert!(
        url.contains("%F0%9F%A4%96%20auto-fix"),
        "expected percent-encoded label in {url}"
    );
    assert!(
        !url.contains("🤖"),
        "raw emoji should not appear in the URL: {url}"
    );
}

// ---------------------------------------------------------------------------
// Code / investigation merge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn code_poll_returns_unique_issue_summary() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                7,
                "Fix login",
                &[CODE_LABEL, "bug"],
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(outcome.summaries.len(), 1);
    let summary = &outcome.summaries[0];
    assert_eq!(
        summary.key,
        IssueKey::parse("octocat/hello-world#7").unwrap()
    );
    assert_eq!(summary.title, "Fix login");
    assert_eq!(summary.ticket_type, TicketType::Code);
    assert!(outcome.diagnostics.is_empty());
}

#[tokio::test]
async fn investigation_poll_returns_unique_issue_summary() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                8,
                "Investigate",
                &[INVESTIGATION_LABEL],
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_investigation(&client, &cfg, &cfg.watched_repos)
        .await
        .unwrap();
    assert_eq!(outcome.summaries.len(), 1);
    assert_eq!(outcome.summaries[0].ticket_type, TicketType::Investigation);
}

#[tokio::test]
async fn pull_request_objects_are_excluded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_list_json(&[
            minimal_issue(7, "Fix login", &[CODE_LABEL]),
            pull_request_issue(8, "Add feature"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(outcome.summaries.len(), 1);
    assert_eq!(outcome.diagnostics.len(), 1);
    match &outcome.diagnostics[0] {
        IssuePollDiagnostic::PullRequest { key, title } => {
            assert_eq!(key, &IssueKey::parse("octocat/hello-world#8").unwrap());
            assert_eq!(title, "Add feature");
        }
        other => panic!("expected PullRequest diagnostic, got {other:?}"),
    }
}

#[tokio::test]
async fn unrelated_label_object_is_diagnosed_as_unmatched() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue_list_json(&[
            // The labels array does not include CODE_LABEL even
            // though the query asked for it. Per the contract, the
            // label must be re-verified.
            minimal_issue(7, "Wrong label", &["some-other"]),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(outcome.summaries.is_empty());
    match &outcome.diagnostics[0] {
        IssuePollDiagnostic::Unmatched { key, labels, .. } => {
            assert_eq!(key, &IssueKey::parse("octocat/hello-world#7").unwrap());
            assert_eq!(labels, &vec!["some-other".to_string()]);
        }
        other => panic!("expected Unmatched, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Ambiguous (both labels) merge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_marks_issue_with_both_labels_as_ambiguous() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                7,
                "Both labels",
                &[CODE_LABEL, INVESTIGATION_LABEL],
            )])),
        )
        .expect(2)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let code = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    let investigation = poll_investigation(&client, &cfg, &cfg.watched_repos)
        .await
        .unwrap();
    assert_eq!(code.summaries.len(), 1);
    assert_eq!(investigation.summaries.len(), 1);

    let merged = merge_outcomes(code, investigation);
    assert!(
        merged.summaries.is_empty(),
        "ambiguous issue must not be enqueued; got {:?}",
        merged.summaries
    );
    assert_eq!(merged.diagnostics.len(), 1);
    match &merged.diagnostics[0] {
        IssuePollDiagnostic::Ambiguous { key, labels, .. } => {
            assert_eq!(key, &IssueKey::parse("octocat/hello-world#7").unwrap());
            assert!(labels.contains(&CODE_LABEL.to_string()));
            assert!(labels.contains(&INVESTIGATION_LABEL.to_string()));
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Empty / null tolerance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_or_null_title_body_and_labels_are_tolerated() {
    let server = MockServer::start().await;
    // All three tolerance cases:
    //  - "number" missing entirely (Malformed)
    //  - "title" null and "labels" null (still classified)
    //  - "labels" empty array (Unmatched, because no trigger label)
    //  - "updated_at" missing (we default to now)
    let body = r#"[
        {"number": 7, "title": null, "labels": null, "updated_at": null},
        {"number": 8, "title": "Labeled", "labels": [], "updated_at": null},
        {"title": "no number"}
    ]"#;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(
        outcome.summaries.is_empty(),
        "no trigger labels present; summaries should be empty"
    );
    // 7 → Unmatched (no labels), 8 → Unmatched (empty labels),
    // "no number" → Malformed.
    let mut kinds: Vec<&str> = outcome
        .diagnostics
        .iter()
        .map(|d| match d {
            IssuePollDiagnostic::Unmatched { .. } => "unmatched",
            IssuePollDiagnostic::Malformed { .. } => "malformed",
            _ => "other",
        })
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec!["malformed", "unmatched", "unmatched"]);
}

#[tokio::test]
async fn malformed_number_is_diagnosed() {
    let server = MockServer::start().await;
    let body = r#"[
        {"number": 0, "title": "Zero number", "labels": [], "updated_at": "2026-07-13T12:00:00Z"}
    ]"#;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(outcome.summaries.is_empty());
    assert!(matches!(
        &outcome.diagnostics[0],
        IssuePollDiagnostic::Malformed { .. }
    ));
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pagination_in_code_poll_follows_link_header() {
    let server = MockServer::start().await;
    let next_url = format!(
        "{}/repos/octocat/hello-world/issues?per_page=100&page=2&labels=%F0%9F%A4%96%20auto-fix&state=open&sort=updated&direction=desc",
        server.uri()
    );
    let link_header = format!("<{next_url}>; rel=\"next\"");
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .set_body_json(issue_list_json(&[minimal_issue(7, "First", &[CODE_LABEL])])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                8,
                "Second",
                &[CODE_LABEL],
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(outcome.summaries.len(), 2);
    let titles: Vec<&str> = outcome.summaries.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"First"));
    assert!(titles.contains(&"Second"));
}

// ---------------------------------------------------------------------------
// 304 reuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_poll_reuses_cached_body_on_304() {
    let server = MockServer::start().await;
    // First request: no If-None-Match → 200 + ETag.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"abc\"")
                .set_body_json(issue_list_json(&[minimal_issue(
                    7,
                    "Cached",
                    &[CODE_LABEL],
                )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Second request: with If-None-Match → 304 (empty body).
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(wiremock::matchers::header("if-none-match", "\"abc\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;

    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];

    let first = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(first.summaries.len(), 1);
    assert_eq!(first.summaries[0].title, "Cached");

    // Second poll on the same client reuses the cache; server
    // returns 304, client surfaces the cached body verbatim.
    let second = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(second.summaries.len(), 1);
    assert_eq!(second.summaries[0].title, "Cached");
    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 2, "expected 200 then 304");
}

// ---------------------------------------------------------------------------
// Custom matcher: request MUST NOT carry the named header
// ---------------------------------------------------------------------------

struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

// ---------------------------------------------------------------------------
// No Events API fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_api_fields_are_ignored() {
    let server = MockServer::start().await;
    // Include a few extra fields GitHub sometimes attaches to
    // events-derived payloads. They must be silently ignored.
    let body = r#"[
        {
            "number": 7,
            "title": "Fix login",
            "labels": [{"name": "🤖 auto-fix"}],
            "updated_at": "2026-07-13T12:00:00Z",
            "event": "labeled",
            "performed_via_github_app": null,
            "performed_by": "octocat",
            "renamed_title": null,
            "transferred": null
        }
    ]"#;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;
    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(outcome.summaries.len(), 1);
    assert!(outcome.diagnostics.is_empty());
}

// ---------------------------------------------------------------------------
// Multiple repos
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_repos_are_polled_in_sequence() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                7,
                "Hello fix",
                &[CODE_LABEL],
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/world/issues"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(issue_list_json(&[minimal_issue(
                9,
                "World fix",
                &[CODE_LABEL],
            )])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let (client, mut cfg) = mock_client(&server);
    cfg.watched_repos = vec![
        "octocat/hello-world".to_string(),
        "octocat/world".to_string(),
    ];
    let outcome = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert_eq!(outcome.summaries.len(), 2);
    let keys: Vec<String> = outcome
        .summaries
        .iter()
        .map(|s| s.key.display_key())
        .collect();
    assert!(keys.contains(&"octocat/hello-world#7".to_string()));
    assert!(keys.contains(&"octocat/world#9".to_string()));
}

// ---------------------------------------------------------------------------
// Pure-helper unit tests
// ---------------------------------------------------------------------------

#[test]
fn merge_with_empty_inputs_returns_empty() {
    let merged = merge_outcomes(IssuePollOutcome::default(), IssuePollOutcome::default());
    assert!(merged.summaries.is_empty());
    assert!(merged.diagnostics.is_empty());
}

#[test]
fn merge_deduplicates_same_ticket_type() {
    let key = IssueKey::parse("octocat/hello-world#7").unwrap();
    let summary = IssueSummary {
        key: key.clone(),
        title: "Same".to_string(),
        labels: vec![CODE_LABEL.to_string()],
        ticket_type: TicketType::Code,
        updated_at: chrono::Utc::now(),
    };
    let code = IssuePollOutcome {
        summaries: vec![summary.clone()],
        diagnostics: vec![],
    };
    let investigation = IssuePollOutcome {
        summaries: vec![summary.clone()],
        diagnostics: vec![],
    };
    let merged = merge_outcomes(code, investigation);
    // Same key, same ticket_type → dedup, no ambiguous diagnostic.
    assert_eq!(merged.summaries.len(), 1);
    assert!(merged.diagnostics.is_empty());
}
