//! Integration test: when both label polls return 304 (from cache),
//! `poll_repo` returns `Outcome304(true)`.
//!
//! The Idle304 path in the tick controller depends on this; the
//! controller tests in `tests/daemon/tick_test.rs` verify the
//! decision logic, and this test verifies the poll layer produces
//! the correct signal.

use caduceus::config::Config;
use caduceus::github::{Client, HttpCache};
use caduceus::poll::{merge_outcomes, poll_code, poll_investigation};
use wiremock::matchers::{method, path, query_param_is_missing};
use wiremock::{Match, Mock, Request, ResponseTemplate};

use fixtures::MockGitHub;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const CODE_LABEL: &str = "autofix";
const INVESTIGATION_LABEL: &str = "autofix-investigate";

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
    cfg.ticket_label_investigation = INVESTIGATION_LABEL.to_string();
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
async fn poll_from_cache_when_both_polls_are_304() {
    let gh = MockGitHub::start().await;

    // First poll (no If-None-Match) → 200 with body containing
    // both a code-labeled and investigation-labeled issue. Each
    // poll (code/investigation) parses the same body but filters
    // by its own label.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"abc\"")
                .set_body_json(issue_list_json(&[
                    minimal_issue(7, "Cached Code", &[CODE_LABEL]),
                    minimal_issue(8, "Cached Inv", &[INVESTIGATION_LABEL]),
                ])),
        )
        .expect(2)
        .mount(gh.server())
        .await;

    // Second poll (with If-None-Match) → 304.
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/issues"))
        .and(query_param_is_missing("page"))
        .and(wiremock::matchers::header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(304))
        .expect(2)
        .mount(gh.server())
        .await;

    let (client, mut cfg) = mock_client(&gh);
    cfg.watched_repos = vec!["octocat/hello-world".to_string()];

    // First poll primes the cache (200 response).
    let code = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(!code.from_cache);
    assert_eq!(code.summaries.len(), 1);
    assert_eq!(code.summaries[0].title, "Cached Code");

    let inv = poll_investigation(&client, &cfg, &cfg.watched_repos)
        .await
        .unwrap();
    assert!(!inv.from_cache);
    assert_eq!(inv.summaries.len(), 1);
    assert_eq!(inv.summaries[0].title, "Cached Inv");

    // Second poll reuses cache (304 response).
    let code2 = poll_code(&client, &cfg, &cfg.watched_repos).await.unwrap();
    assert!(code2.from_cache, "second code poll should be from cache");

    let inv2 = poll_investigation(&client, &cfg, &cfg.watched_repos)
        .await
        .unwrap();
    assert!(
        inv2.from_cache,
        "second investigation poll should be from cache"
    );

    // Merge outcomes: both from_cache=true → merged.from_cache=true.
    let merged = merge_outcomes(code2, inv2);
    assert!(merged.from_cache);
    assert_eq!(
        merged.summaries.len(),
        2,
        "expected 2 summaries, got {:?}",
        merged.summaries
    );
}
