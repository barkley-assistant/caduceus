//! Task 2.2 acceptance tests for repository discovery.
//!
//! Covers every assertion in the task packet:
//!
//! - Configured bypass (no API call, deterministic dedup + sort)
//! - Two-page Link traversal
//! - Sorting / deduplication (case-insensitive)
//! - Empty result
//! - Archived / disabled exclusion
//! - Malformed object (missing `full_name`)
//! - Page-cap error (>20 pages)
//! - Rate-limit on page two

use std::path::{Path, PathBuf};

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::github::{Client, HttpCache};
use caduceus::poll::{discover_watched_repos, next_url_from_link_header, MAX_PAGES_PER_ENDPOINT};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-repo-poll-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn configured_client(state_dir: &Path, watched: &[&str]) -> (Client, Config) {
    let cfg = {
        let mut c = Config::test_defaults(state_dir);
        c.watched_repos = watched.iter().map(|s| s.to_string()).collect();
        c
    };
    let cache = HttpCache::open(state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

fn mock_client(server: &MockServer) -> Client {
    let state_dir = tempdir("mock");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

// ---------------------------------------------------------------------------
// Configured bypass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn configured_bypass_skips_api_and_dedupes_case_insensitively() {
    // The mock server is intentionally unreachable in spirit — if
    // the bypass works, no request is ever sent.
    let state_dir = tempdir("bypass");
    let (client, cfg) = configured_client(
        &state_dir,
        &[
            "Barkley-Assistant/RepoA",
            "octocat/hello-world",
            "barkley-assistant/repoa",
        ],
    );
    let repos = discover_watched_repos(&client, &cfg)
        .await
        .expect("configured repos validate");
    assert_eq!(
        repos,
        vec![
            "Barkley-Assistant/RepoA".to_string(),
            "octocat/hello-world".to_string(),
        ]
    );
}

#[tokio::test]
async fn configured_bypass_is_sorted_case_insensitively() {
    let state_dir = tempdir("bypass-sort");
    let (client, cfg) = configured_client(&state_dir, &["octocat/zzz", "OctoCat/AAA", "mona/BBB"]);
    let repos = discover_watched_repos(&client, &cfg).await.unwrap();
    // Sorted case-insensitively: mona/bbb < octocat/aaa < octocat/zzz
    assert_eq!(
        repos,
        vec![
            "mona/BBB".to_string(),
            "OctoCat/AAA".to_string(),
            "octocat/zzz".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// API discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_result_returns_empty_vec() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("empty"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let repos = discover_watched_repos(&client, &cfg)
        .await
        .expect("discovery");
    assert!(repos.is_empty());
}

#[tokio::test]
async fn archived_and_disabled_repos_are_excluded() {
    let server = MockServer::start().await;
    let body = r#"[
        {"full_name": "octocat/hello", "archived": false, "disabled": false},
        {"full_name": "octocat/archived", "archived": true, "disabled": false},
        {"full_name": "octocat/disabled", "archived": false, "disabled": true},
        {"full_name": "octocat/world", "archived": false, "disabled": false}
    ]"#;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("archived"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let repos = discover_watched_repos(&client, &cfg)
        .await
        .expect("discovery");
    assert_eq!(
        repos,
        vec!["octocat/hello".to_string(), "octocat/world".to_string(),]
    );
}

#[tokio::test]
async fn malformed_object_missing_full_name_errors() {
    let server = MockServer::start().await;
    let body = r#"[
        {"full_name": "octocat/ok", "archived": false, "disabled": false},
        {"archived": false, "disabled": false}
    ]"#;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("malformed"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let err = discover_watched_repos(&client, &cfg)
        .await
        .expect_err("missing full_name is fatal");
    let text = format!("{err:?}");
    assert!(
        text.contains("full_name") && text.contains("missing"),
        "unexpected error: {text}"
    );
}

#[tokio::test]
async fn malformed_json_array_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not an array"))
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("badjson"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let err = discover_watched_repos(&client, &cfg)
        .await
        .expect_err("non-array body errors");
    let text = format!("{err:?}");
    assert!(text.contains("JSON parse"), "unexpected error: {text}");
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_page_link_traversal_merges_results() {
    let server = MockServer::start().await;
    // Page 1 returns two repos plus a `rel="next"` link to page 2.
    let next_url = format!(
        "{}/user/repos?per_page=100&sort=full_name&page=2",
        server.uri()
    );
    let page1 = r#"[
            {"full_name": "octocat/a", "archived": false, "disabled": false},
            {"full_name": "octocat/b", "archived": false, "disabled": false}
        ]"#;
    let link_header = format!("<{next_url}>; rel=\"next\"");
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(wiremock::matchers::query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .set_body_string(page1),
        )
        .expect(1)
        .mount(&server)
        .await;

    let page2 = r#"[
        {"full_name": "octocat/c", "archived": false, "disabled": false}
    ]"#;
    // Page 2 is matched by the query string `page=2` so wiremock
    // does not hand it back to the first mock.
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(page2))
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("twopage"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let repos = discover_watched_repos(&client, &cfg)
        .await
        .expect("discovery");
    assert_eq!(
        repos,
        vec![
            "octocat/a".to_string(),
            "octocat/b".to_string(),
            "octocat/c".to_string(),
        ]
    );
    let received = server.received_requests().await.expect("received");
    assert_eq!(received.len(), 2, "expected exactly two page fetches");
}

#[tokio::test]
async fn rate_limit_on_page_two_surfaces_typed_error() {
    let server = MockServer::start().await;
    let next_url = format!(
        "{}/user/repos?per_page=100&sort=full_name&page=2",
        server.uri()
    );
    let link_header = format!("<{next_url}>; rel=\"next\"");
    // Page 1 returns a successful 200 with the next link.
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(wiremock::matchers::query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .insert_header("x-ratelimit-remaining", "5000")
                .set_body_string("[]"),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Page 2 returns a 200 body but rate-limit headers indicate exhaustion.
    let reset = chrono::Utc::now().timestamp() + 600;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-limit", "5000")
                .insert_header("x-ratelimit-reset", reset.to_string())
                .set_body_string("[]"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("ratelimit"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let err = discover_watched_repos(&client, &cfg)
        .await
        .expect_err("rate-limited on page two");
    match err {
        CaduceusError::RateLimited {
            reset_at,
            remaining,
            limit,
        } => {
            assert_eq!(remaining, 0);
            assert_eq!(limit, Some(5000));
            // Reset is in the future but bounded by the configured 600s
            // above; allow a small clock skew window.
            assert!(
                reset_at <= 600,
                "reset_at should be at most 600 seconds; got {reset_at}"
            );
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Page cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exceeding_max_pages_errors() {
    // Mount a server that always emits a `rel="next"` link, so the
    // discovery loop walks forever and trips MAX_PAGES_PER_ENDPOINT.
    let server = MockServer::start().await;
    let next_url = format!(
        "{}/user/repos?per_page=100&sort=full_name&page=99999",
        server.uri()
    );
    let link_header = format!("<{next_url}>; rel=\"next\"");
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", link_header)
                .set_body_string("[]"),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let mut cfg = Config::test_defaults(&tempdir("cap"));
    cfg.api_base = server.uri();
    cfg.watched_repos.clear();
    let err = discover_watched_repos(&client, &cfg)
        .await
        .expect_err("page cap exceeded");
    let text = format!("{err:?}");
    assert!(
        text.contains(&format!("{MAX_PAGES_PER_ENDPOINT}")),
        "unexpected error: {text}"
    );
}

// ---------------------------------------------------------------------------
// Link header parser
// ---------------------------------------------------------------------------

#[test]
fn link_header_parser_extracts_rel_next() {
    let header = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=5>; rel="last""#;
    assert_eq!(
        next_url_from_link_header(header).as_deref(),
        Some("https://api.github.com/user/repos?page=2")
    );
}

#[test]
fn link_header_parser_returns_none_when_only_last_rel_present() {
    let header = r#"<https://api.github.com/user/repos?page=5>; rel="last""#;
    assert!(next_url_from_link_header(header).is_none());
}

#[test]
fn link_header_parser_returns_none_for_empty_input() {
    assert!(next_url_from_link_header("").is_none());
}
