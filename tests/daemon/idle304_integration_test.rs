//! Integration test: when the label poll returns 304 (from cache),
//! `poll_repo` returns `Outcome304(true)`.
//!
//! The Idle304 path in the tick controller depends on this; the
//! controller tests in `tests/daemon/tick_test.rs` verify the
//! decision logic, and this test verifies the poll layer produces
//! the correct signal. (The two-poll merge was removed in N+1,
//! issue #331 — there is a single code-label poll now.)

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache};
use caduceus::poll::poll_code;
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Match, Mock, Request, ResponseTemplate};

use fixtures::MockGitHub;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const CODE_LABEL: &str = "autofix";

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

fn mock_client(gh: &MockGitHub) -> (Client, Config) {
    let state_dir = tempdir("mock");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    cfg.ticket_label_code = CODE_LABEL.to_string();
    cfg.watched_repos.clear();
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    (client, cfg)
}

struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

#[tokio::test]
async fn poll_from_cache_when_poll_is_304() {
    let gh = MockGitHub::start().await;

    // First poll (no If-None-Match) → 200 with a code-labeled issue.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"abc\"")
                .set_body_json(issue_list_json(&[minimal_issue(
                    7,
                    "Cached Code",
                    &[CODE_LABEL],
                )])),
        )
        .expect(1)
        .mount(gh.server())
        .await;

    // Second poll (with If-None-Match) → 304.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(wiremock::matchers::header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(gh.server())
        .await;

    let (client, mut cfg) = mock_client(&gh);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];

    // First poll primes the cache (200 response).
    let code = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(!code.from_cache);
    assert_eq!(code.summaries.len(), 1);
    assert_eq!(code.summaries[0].title, "Cached Code");

    // Second poll reuses cache (304 response) with the same summaries.
    let code2 = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(code2.from_cache, "second code poll should be from cache");
    assert_eq!(
        code2.summaries.len(),
        1,
        "cached body serves the summaries verbatim"
    );
    assert_eq!(code2.summaries[0].title, "Cached Code");
}
