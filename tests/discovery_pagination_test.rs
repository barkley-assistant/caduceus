//! Task 5.5 acceptance tests for bounded pagination in repository
//! discovery and labeled-issue polling.
//!
//! Each test exercises a single scenario from the spec:
//!
//! * `discover_watched_repos` follows rel="next" links up to max_pages.
//! * `discover_watched_repos` errors when max_pages is exceeded
//!   (infinite next-link loop case).
//! * `discover_watched_repos` with max_pages=0 errors immediately.
//! * `poll_code` follows rel="next" links up to max_pages.
//! * `poll_code` errors when max_pages is exceeded.
//! * `Config::from_raw` rejects `discovery_max_pages = 0`.
//! * `Config::test_defaults` sets `discovery_max_pages = 20`.

use std::path::{Path, PathBuf};

use caduceus::config::{
    Config, LoadContext, RawConfig, DEFAULT_DISCOVERY_MAX_PAGES, DEFAULT_TICKET_LABEL_CODE,
};
use caduceus::github::{Client, HttpCache};
use wiremock::matchers::{method, path};
use wiremock::{Match, Mock, Request, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-pagination-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn test_config(state_dir: &Path, api_base: &str, max_pages: u32) -> Config {
    let mut cfg = Config::test_defaults(state_dir);
    cfg.api_base = api_base.to_string();
    cfg.github_token = Some("test-token".to_string());
    cfg.discovery_max_pages = max_pages;
    cfg.http_timeout_seconds = 5;
    cfg
}

fn test_config_with_repos(
    state_dir: &Path,
    api_base: &str,
    max_pages: u32,
    watched_repos: Vec<String>,
) -> Config {
    let mut cfg = test_config(state_dir, api_base, max_pages);
    cfg.watched_repos = watched_repos;
    cfg
}

/// Build a minimal JSON array of repository objects.
fn repo_page(names: &[&str]) -> String {
    let items: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"full_name": "{n}", "archived": false, "disabled": false}}"#))
        .collect();
    format!("[{}]", items.join(","))
}

/// Build a minimal JSON array of issue objects.
fn issue_page(numbers: &[u64], label: &str) -> String {
    let items: Vec<String> = numbers
        .iter()
        .map(|n| {
            format!(
                r#"{{"number":{n},"title":"issue {n}","labels":[{{"name":"{label}"}}],"updated_at":"2026-01-01T00:00:00Z"}}"#
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Build a Link header pointing to a next page.
fn next_link(base: &str, page: usize) -> String {
    format!("<{base}/repos?page={page}>; rel=\"next\"")
}

/// Custom wiremock matcher that matches the `page` query parameter.
struct PageQuery(usize);

impl Match for PageQuery {
    fn matches(&self, req: &Request) -> bool {
        let url = req.url.as_str();
        // Match `?page=N` in the query string.
        url.contains(&format!("page={}", self.0))
    }
}

// ---------------------------------------------------------------------------
// discover_via_api follows rel="next" links up to max_pages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discover_via_api_follows_next_links_up_to_max_pages() {
    let state_dir = tempdir("follow-links");
    let gh = wiremock::MockServer::start().await;
    let cfg = test_config(&state_dir, &gh.uri(), 5);

    // Page 1: repos + next link to page 2
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(repo_page(&["owner/repo-a"]))
                .insert_header("link", next_link(&gh.uri(), 2).as_str()),
        )
        .expect(1)
        .mount(&gh)
        .await;

    // Page 2: repos + next link to page 3
    Mock::given(method("GET"))
        .and(PageQuery(2))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(repo_page(&["owner/repo-b"]))
                .insert_header("link", next_link(&gh.uri(), 3).as_str()),
        )
        .expect(1)
        .mount(&gh)
        .await;

    // Page 3: repos, no next link (last page)
    Mock::given(method("GET"))
        .and(PageQuery(3))
        .respond_with(ResponseTemplate::new(200).set_body_string(repo_page(&["owner/repo-c"])))
        .expect(1)
        .mount(&gh)
        .await;

    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let repos = caduceus::github::discover_watched_repos(&client, &cfg)
        .await
        .expect("discovery succeeds");

    assert_eq!(
        repos,
        vec![
            "owner/repo-a".to_string(),
            "owner/repo-b".to_string(),
            "owner/repo-c".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// discover_via_api errors when max_pages is exceeded (infinite loop)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discover_via_api_errors_when_max_pages_exceeded() {
    let state_dir = tempdir("infinite-loop");
    let gh = wiremock::MockServer::start().await;
    let max_pages = 3u32;
    let cfg = test_config(&state_dir, &gh.uri(), max_pages);

    // Every response returns repos + a next link. The daemon must stop
    // after max_pages requests and return an error.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(repo_page(&["owner/repo-loop"]))
                .insert_header("link", next_link(&gh.uri(), 99).as_str()),
        )
        .mount(&gh)
        .await;

    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let err = caduceus::github::discover_watched_repos(&client, &cfg)
        .await
        .expect_err("should error on page cap");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("exceeded"),
        "expected 'exceeded' in error, got: {msg}"
    );
    assert!(
        msg.contains(&max_pages.to_string()),
        "expected page limit {max_pages} in error, got: {msg}"
    );

    // Verify that exactly max_pages requests were made.
    let received = gh.received_requests().await.expect("requests");
    assert_eq!(
        received.len(),
        max_pages as usize,
        "expected {} requests, got {}",
        max_pages,
        received.len()
    );
}

// ---------------------------------------------------------------------------
// discover_via_api with max_pages=0 errors immediately
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discover_via_api_max_pages_zero_errors_immediately() {
    let state_dir = tempdir("zero-pages");
    let gh = wiremock::MockServer::start().await;
    let cfg = test_config(&state_dir, &gh.uri(), 0);

    // No mock should be hit because discovery errors before any HTTP call.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(0)
        .mount(&gh)
        .await;

    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let err = caduceus::github::discover_watched_repos(&client, &cfg)
        .await
        .expect_err("should error with max_pages=0");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("exceeded"),
        "expected 'exceeded' in error, got: {msg}"
    );
    assert!(
        msg.contains("0"),
        "expected page limit 0 in error, got: {msg}"
    );

    // Verify no HTTP requests were made.
    let received = gh.received_requests().await.expect("requests");
    assert!(
        received.is_empty(),
        "expected 0 requests, got {}",
        received.len()
    );
}

// ---------------------------------------------------------------------------
// poll_code follows rel="next" links up to max_pages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn poll_code_follows_next_links_up_to_max_pages() {
    let state_dir = tempdir("poll-follow-links");
    let gh = wiremock::MockServer::start().await;
    let main_label = DEFAULT_TICKET_LABEL_CODE;
    let cfg = test_config_with_repos(&state_dir, &gh.uri(), 5, vec!["owner/widgets".to_string()]);

    // Page 1: issues + next link to page 2
    Mock::given(method("GET"))
        .and(path("/repos/owner/widgets/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(issue_page(&[1], main_label))
                .insert_header("link", next_link(&gh.uri(), 2).as_str()),
        )
        .expect(1)
        .mount(&gh)
        .await;

    // Page 2: issues + next link to page 3
    Mock::given(method("GET"))
        .and(PageQuery(2))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(issue_page(&[2], main_label))
                .insert_header("link", next_link(&gh.uri(), 3).as_str()),
        )
        .expect(1)
        .mount(&gh)
        .await;

    // Page 3: issues, no next link
    Mock::given(method("GET"))
        .and(PageQuery(3))
        .respond_with(ResponseTemplate::new(200).set_body_string(issue_page(&[3], main_label)))
        .expect(1)
        .mount(&gh)
        .await;

    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let repos = vec!["owner/widgets".to_string()];
    let outcome = caduceus::github::poll_code(&client, &cfg, &repos)
        .await
        .expect("poll succeeds");

    assert_eq!(outcome.summaries.len(), 3);
    let numbers: Vec<u64> = outcome.summaries.iter().map(|s| s.key.number).collect();
    assert_eq!(numbers, vec![1, 2, 3]);
}

// ---------------------------------------------------------------------------
// poll_code errors when max_pages is exceeded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn poll_code_errors_when_max_pages_exceeded() {
    let state_dir = tempdir("poll-infinite-loop");
    let gh = wiremock::MockServer::start().await;
    let max_pages = 2u32;
    let main_label = DEFAULT_TICKET_LABEL_CODE;
    let cfg = test_config_with_repos(
        &state_dir,
        &gh.uri(),
        max_pages,
        vec!["owner/widgets".to_string()],
    );

    // Every response returns issues + a next link.
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(issue_page(&[1], main_label))
                .insert_header("link", next_link(&gh.uri(), 99).as_str()),
        )
        .mount(&gh)
        .await;

    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let repos = vec!["owner/widgets".to_string()];
    let err = caduceus::github::poll_code(&client, &cfg, &repos)
        .await
        .expect_err("should error on page cap");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("exceeded"),
        "expected 'exceeded' in error, got: {msg}"
    );
    assert!(
        msg.contains(&max_pages.to_string()),
        "expected page limit {max_pages} in error, got: {msg}"
    );

    // Verify exactly max_pages requests were made.
    let received = gh.received_requests().await.expect("requests");
    assert_eq!(
        received.len(),
        max_pages as usize,
        "expected {} requests, got {}",
        max_pages,
        received.len()
    );
}

// ---------------------------------------------------------------------------
// Config validation (no network)
// ---------------------------------------------------------------------------

#[test]
fn config_from_raw_rejects_discovery_max_pages_zero() {
    let yaml = r#"
        discovery_max_pages: 0
        worker_command: ["python3", "bridge.py"]
        "#;
    let raw: RawConfig = serde_yaml::from_str(yaml).expect("yaml parses");
    let root = tempdir("cfg-zero-max");
    let ctx = LoadContext {
        hermes_home: Some(root.join("home")),
        plugin_root: Some(root.join("plugin")),
        env: caduceus::config::RawEnv::default(),
    };
    let err = Config::from_raw(raw, &ctx).expect_err("must reject 0");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("discovery_max_pages must be > 0"),
        "got: {msg}"
    );
}

#[test]
fn test_defaults_sets_discovery_max_pages_to_20() {
    let root = tempdir("defaults-max");
    let cfg = Config::test_defaults(&root);
    assert_eq!(cfg.discovery_max_pages, 20);
    assert_eq!(cfg.discovery_max_pages, DEFAULT_DISCOVERY_MAX_PAGES);
}
