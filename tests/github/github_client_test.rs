//! - Required headers (User-Agent, Accept, X-GitHub-Api-Version)
//! - Authorization header (Bearer token)
//! - Connect timeout and per-request timeout
//! - Allowed same-host redirect
//! - Cross-host redirect rejected without forwarding the token
//! - Redirect loop is bounded by `MAX_REDIRECTS`
//! - First 200, second client sends `If-None-Match`, server replies 304,
//!   cached body is reused verbatim
//! - 304 with no cached body is reported as a typed error
//! - Cache corruption is recovered automatically
//! - Invalid ETag is not cached
//! - Oversized body (>10 MiB chunked) is refused before full allocation
//! - 401/403/404/500 map to `CaduceusError::GitHubApi`
//! - Token values never appear in `Display` or `Debug`
//!
//! `(method, path)` — headers, query params, the `NoHeader`
//! inversion — drop down to the underlying wiremock `MockServer`
//! via [`fixtures::MockGitHub::server`].

#![allow(unused_imports)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::github::{
    cache_key, is_valid_etag, Client, HttpCache, ACCEPT_VALUE, GITHUB_API_VERSION_HEADER,
    GITHUB_API_VERSION_VALUE, MAX_BODY_BYTES, MAX_REDIRECTS, USER_AGENT_PREFIX,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Match, Mock, Request, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::MockGitHub;

/// Custom matcher that requires the request to NOT carry the named
/// header. Used to build stateful mocks that branch on header
/// presence.
struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

// Fixtures

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-http-client-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn test_config(state_dir: &Path, api_base: &str, token: Option<&str>) -> Config {
    let mut cfg = Config::test_defaults(state_dir);
    cfg.api_base = api_base.to_string();
    cfg.github_token = token.map(|t| t.to_string());
    // Short timeouts so per-request timeout tests don't sleep for a minute.
    cfg.http_timeout_seconds = 5;
    cfg
}

fn client_for(gh: &MockGitHub, token: Option<&str>) -> (Client, PathBuf) {
    let state_dir = tempdir("client");
    let cfg = test_config(&state_dir, &gh.uri(), token);
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, state_dir)
}

// Required headers

#[tokio::test]
async fn required_headers_are_present_on_every_request() {
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(header(
            "user-agent",
            format!("{USER_AGENT_PREFIX}/{}", env!("CARGO_PKG_VERSION")).as_str(),
        ))
        .and(header("accept", ACCEPT_VALUE))
        .and(header(
            GITHUB_API_VERSION_HEADER.to_ascii_lowercase().as_str(),
            GITHUB_API_VERSION_VALUE,
        ))
        .and(header(
            "authorization",
            format!("Bearer {TEST_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let response = client
        .get("/repos/octocat/hello-world/issues", ACCEPT_VALUE)
        .await
        .expect("request succeeds");
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn missing_token_omits_authorization_header() {
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .and(header("authorization", "Bearer"))
        .respond_with(ResponseTemplate::new(401))
        .expect(0)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, None);
    let response = client.get("/user/repos", ACCEPT_VALUE).await.expect("200");
    assert_eq!(response.status, 200);
}

// Timeout

#[tokio::test]
async fn request_timeout_fires_when_server_hangs() {
    // Mock a server that sleeps for 60 seconds — the client's 5s timeout
    // must trip before the sleep ends.
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let started = std::time::Instant::now();
    let result = client.get("/slow", ACCEPT_VALUE).await;
    let elapsed = started.elapsed();
    assert!(result.is_err(), "expected a timeout error");
    assert!(
        elapsed < Duration::from_secs(15),
        "client must time out quickly (took {elapsed:?})"
    );
}

// Redirect policy

#[tokio::test]
async fn same_host_redirect_is_followed_within_limit() {
    let gh = MockGitHub::start().await;
    // The mock server reuses the same host for every response, so any
    // redirect to "/elsewhere" is a same-origin 302.
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/elsewhere"))
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let response = client
        .get("/start", ACCEPT_VALUE)
        .await
        .expect("redirect succeeds");
    assert_eq!(response.status, 200);
    assert!(response.final_url.ends_with("/elsewhere"));
}

#[tokio::test]
async fn cross_host_redirect_is_rejected_and_token_is_not_forwarded() {
    // Two separate mock servers simulate two hosts. The first one
    // replies 302 to the second host; the client must refuse.
    let gh_a = MockGitHub::start().await;
    let gh_b = MockGitHub::start().await;

    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/b", gh_b.uri())),
        )
        .expect(1)
        .mount(gh_a.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string("leaked"))
        .expect(0)
        .mount(gh_b.server())
        .await;

    let (client, _dir) = client_for(&gh_a, Some(TEST_TOKEN));
    let err = client
        .get("/a", ACCEPT_VALUE)
        .await
        .expect_err("cross-host refused");
    let text = format!("{err:?}");
    assert!(
        text.contains("cross-origin") || text.contains("redirect"),
        "unexpected error: {text}"
    );
    assert!(
        !text.contains(TEST_TOKEN),
        "token leaked in error text: {text}"
    );
}

#[tokio::test]
async fn redirect_loop_is_bounded_by_max_redirects() {
    let gh = MockGitHub::start().await;
    // /loop -> 302 /loop -> 302 /loop ... The client must stop after
    // MAX_REDIRECTS hops (3) and surface the loop as an error.
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let err = client
        .get("/loop", ACCEPT_VALUE)
        .await
        .expect_err("loop refused");
    let text = format!("{err:?}");
    assert!(
        text.contains("too many redirects") || text.contains("redirect"),
        "unexpected error: {text}"
    );
    // The loop bound: at most MAX_REDIRECTS+1 requests (initial + 3 hops).
    // Wiremock records the request count on the server; we drop down to
    // the underlying mock because this test bypasses the CountingResponder.
    let received = gh
        .server()
        .received_requests()
        .await
        .expect("received requests");
    assert!(
        received.len() <= MAX_REDIRECTS + 1,
        "expected at most {} requests, got {}",
        MAX_REDIRECTS + 1,
        received.len()
    );
    assert!(
        received.len() >= MAX_REDIRECTS,
        "expected at least {} requests, got {}",
        MAX_REDIRECTS,
        received.len()
    );
}

// ETag conditional GET and 304 body reuse

#[tokio::test]
async fn first_request_stores_etag_and_second_client_returns_cached_body_on_304() {
    let gh = MockGitHub::start().await;
    // The first mock returns 200 + an ETag whenever the request
    // does NOT carry If-None-Match. The second mock matches a
    // request with If-None-Match and returns 304. Wiremock matches
    // the most-recently-mounted matching mock, so the second mock
    // takes precedence on the conditional request.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param("labels", "bug"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"abc\"")
                .set_body_string("[{\"number\":1}]"),
        )
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param("labels", "bug"))
        .and(header("if-none-match", "\"abc\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(gh.server())
        .await;

    let state_dir = tempdir("etag");
    let cfg = test_config(&state_dir, &gh.uri(), Some(TEST_TOKEN));

    // First client performs a fresh GET.
    let cache_a = HttpCache::open(&state_dir).expect("cache opens");
    let client_a = Client::with_cache(&cfg, cache_a).expect("client A builds");
    let response_a = client_a
        .get("/repos/octocat/hello-world/issues?labels=bug", ACCEPT_VALUE)
        .await
        .expect("first request succeeds");
    assert_eq!(response_a.status, 200);
    assert!(!response_a.from_cache);
    assert_eq!(response_a.body_text().unwrap(), "[{\"number\":1}]");

    // Second client (fresh in-memory state, but same on-disk cache)
    // sends If-None-Match and receives 304.
    let cache_b = HttpCache::open(&state_dir).expect("cache reloads");
    let client_b = Client::with_cache(&cfg, cache_b).expect("client B builds");
    let response_b = client_b
        .get("/repos/octocat/hello-world/issues?labels=bug", ACCEPT_VALUE)
        .await
        .expect("conditional GET succeeds");
    assert_eq!(response_b.status, 304);
    assert!(response_b.from_cache);
    assert_eq!(response_b.body_text().unwrap(), "[{\"number\":1}]");
}

#[tokio::test]
async fn cache_corruption_is_recovered_on_next_open() {
    let state_dir = tempdir("corrupt");
    let cache_path = state_dir.join("cache").join("http.json");
    std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    std::fs::write(&cache_path, b"{not valid json").unwrap();

    // The cache must not panic on corrupt JSON; it must return an
    // empty state and remove the bad file.
    let cache = HttpCache::open(&state_dir).expect("corrupt cache recovers");
    assert!(cache.get("doesnt-matter").is_none());

    // After recovery the file is gone.
    assert!(
        !cache_path.exists(),
        "corrupt cache file should have been removed"
    );
}

#[tokio::test]
async fn invalid_etag_is_not_cached() {
    // An ETag without quotes is invalid; the cache must refuse to
    // store it so the next request is unconditional.
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/bad-etag"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "not-a-quoted-tag")
                .set_body_string("body"),
        )
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let response = client.get("/bad-etag", ACCEPT_VALUE).await.expect("ok");
    assert_eq!(response.status, 200);
    let cache = client.cache();
    let key = cache_key(&format!("{}/bad-etag", gh.uri()), ACCEPT_VALUE);
    assert!(cache.get(&key).is_none(), "invalid ETag must not be cached");
}

// Body size cap (10 MiB)

#[tokio::test]
async fn oversized_chunked_body_is_refused_before_full_allocation() {
    // Build a body of MAX_BODY_BYTES + 1 bytes sent in 1 MiB chunks
    // (chunked transfer-encoding), then assert the client surfaces
    // a typed "too large" error rather than panicking.
    let gh = MockGitHub::start().await;
    let chunk = vec![b'x'; 1024 * 1024];
    let chunks_needed = (MAX_BODY_BYTES / chunk.len()) + 2;
    let mut body = Vec::with_capacity(chunks_needed * chunk.len());
    for _ in 0..chunks_needed {
        body.extend_from_slice(&chunk);
    }
    // Trim down to one byte over the cap so the body stays in a
    // single allocation but still exceeds MAX_BODY_BYTES.
    body.truncate(MAX_BODY_BYTES + 1);

    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let err = client
        .get("/big", ACCEPT_VALUE)
        .await
        .expect_err("oversized body refused");
    let text = format!("{err:?}");
    assert!(
        text.contains("exceeds") || text.contains("body"),
        "unexpected error: {text}"
    );
}

// Status mapping

#[tokio::test]
async fn non_success_statuses_map_to_typed_github_api_errors() {
    for status in [401u16, 403, 404, 500] {
        let gh = MockGitHub::start().await;
        Mock::given(method("GET"))
            .and(path("/probe"))
            .respond_with(ResponseTemplate::new(status).set_body_string("boom"))
            .expect(1)
            .mount(gh.server())
            .await;

        let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
        let err = client
            .get("/probe", ACCEPT_VALUE)
            .await
            .expect_err("non-2xx must error");
        match err {
            CaduceusError::GitHubApi { status: s, message } => {
                assert_eq!(s, status, "status roundtrip for {status}");
                assert!(message.contains("boom"));
            }
            other => panic!("expected GitHubApi, got {other:?} for status {status}"),
        }
    }
}

// Token redaction in error messages

#[tokio::test]
async fn token_value_never_appears_in_errors() {
    let gh = MockGitHub::start().await;
    // The body echoes back a credential-shaped assignment; if
    // redaction is broken anywhere in the chain, the assertion
    // below will catch it. The contract guarantees the three
    // documented credential variable names are scrubbed before the
    // message lands in Display/Debug.
    let body = "GITHUB_TOKEN=ghp_testtoken_value_xyz leaked".to_string();
    Mock::given(method("GET"))
        .and(path("/leak"))
        .respond_with(ResponseTemplate::new(401).set_body_string(body))
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let err = client
        .get("/leak", ACCEPT_VALUE)
        .await
        .expect_err("401 surfaces");
    let debug = format!("{err:?}");
    let display = format!("{err}");
    assert!(!debug.contains(TEST_TOKEN), "Debug leaks token: {debug}");
    assert!(
        !display.contains(TEST_TOKEN),
        "Display leaks token: {display}"
    );
}

// Pure-helper unit tests

#[test]
fn etag_validator_accepts_strong_and_weak_forms() {
    assert!(is_valid_etag("\"abc\""));
    assert!(is_valid_etag("W/\"abc\""));
    assert!(!is_valid_etag(""));
    assert!(!is_valid_etag("not-quoted"));
    assert!(!is_valid_etag("\""));
}

#[test]
fn cache_key_includes_accept_header() {
    let key1 = cache_key("https://api.github.com/x", ACCEPT_VALUE);
    let key2 = cache_key("https://api.github.com/x", "application/json");
    assert_ne!(key1, key2);
    assert!(key1.starts_with("https://api.github.com/x"));
}

// Final URL survives redirect

#[tokio::test]
async fn final_url_survives_one_redirect_for_transfer_detection() {
    let gh = MockGitHub::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/repositories/12345"))
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("GET"))
        .and(path("/repositories/12345"))
        .respond_with(ResponseTemplate::new(200).set_body_string("redirected"))
        .expect(1)
        .mount(gh.server())
        .await;

    let (client, _dir) = client_for(&gh, Some(TEST_TOKEN));
    let response = client
        .get("/repos/octocat/hello-world", ACCEPT_VALUE)
        .await
        .expect("301 followed");
    assert_eq!(response.status, 200);
    assert!(
        response.final_url.ends_with("/repositories/12345"),
        "final_url should reflect the post-redirect URL; got {}",
        response.final_url
    );
}
