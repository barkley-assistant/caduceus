//! Pure CI rollup truth table and fetch-layer pagination
//! (issue #435 / PR #436 review).

use caduceus::config::Config;
use caduceus::github::{
    fetch_ci_rollup, is_safe_commit_sha, rollup, CheckRun, CheckRunsPage, CiRollup, Client,
    CombinedStatus, HttpCache,
};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

fn status(state: &str, total: u64) -> CombinedStatus {
    CombinedStatus {
        state: Some(state.to_string()),
        total_count: Some(total),
    }
}

fn empty_status() -> CombinedStatus {
    CombinedStatus {
        state: Some("pending".to_string()),
        total_count: Some(0),
    }
}

fn checks(runs: Vec<CheckRun>, total: Option<u64>) -> CheckRunsPage {
    let total_count = total.or(Some(runs.len() as u64));
    CheckRunsPage {
        total_count,
        check_runs: runs,
    }
}

fn run(status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun {
        status: Some(status.to_string()),
        conclusion: conclusion.map(str::to_string),
    }
}

fn success_run() -> serde_json::Value {
    serde_json::json!({"status": "completed", "conclusion": "success"})
}

fn failure_run() -> serde_json::Value {
    serde_json::json!({"status": "completed", "conclusion": "failure"})
}

struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

fn client_for(server: &MockServer, state_dir: &std::path::Path) -> Client {
    let mut cfg = Config::test_defaults(state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some("ghp_testtoken_value_xyz".to_string());
    let cache = HttpCache::open(state_dir).expect("cache");
    Client::with_cache(&cfg, cache).expect("client")
}

#[test]
fn empty_empty_is_absent_even_when_combined_state_is_pending() {
    assert_eq!(
        rollup(&empty_status(), &checks(vec![], Some(0))),
        CiRollup::Absent
    );
}

#[test]
fn legacy_failure_is_failure() {
    assert_eq!(
        rollup(&status("failure", 1), &checks(vec![], Some(0))),
        CiRollup::Failure
    );
    assert_eq!(
        rollup(&status("error", 2), &checks(vec![], Some(0))),
        CiRollup::Failure
    );
}

#[test]
fn actions_only_in_progress_is_pending() {
    assert_eq!(
        rollup(
            &empty_status(),
            &checks(vec![run("in_progress", None)], Some(1))
        ),
        CiRollup::Pending
    );
}

#[test]
fn success_status_plus_failing_check_is_failure() {
    assert_eq!(
        rollup(
            &status("success", 1),
            &checks(vec![run("completed", Some("failure"))], Some(1))
        ),
        CiRollup::Failure
    );
}

#[test]
fn failure_wins_over_sibling_pending_check() {
    assert_eq!(
        rollup(
            &empty_status(),
            &checks(
                vec![run("completed", Some("failure")), run("in_progress", None),],
                Some(2)
            )
        ),
        CiRollup::Failure
    );
}

#[test]
fn neutral_and_skipped_only_is_success() {
    assert_eq!(
        rollup(
            &empty_status(),
            &checks(
                vec![
                    run("completed", Some("neutral")),
                    run("completed", Some("skipped")),
                ],
                Some(2)
            )
        ),
        CiRollup::Success
    );
}

#[test]
fn action_required_is_pending() {
    assert_eq!(
        rollup(
            &empty_status(),
            &checks(vec![run("completed", Some("action_required"))], Some(1))
        ),
        CiRollup::Pending
    );
}

#[test]
fn unrecognized_conclusion_does_not_admit() {
    assert_eq!(
        rollup(
            &empty_status(),
            &checks(vec![run("completed", Some("weird"))], Some(1))
        ),
        CiRollup::Pending
    );
}

#[test]
fn all_success_is_success() {
    assert_eq!(
        rollup(
            &status("success", 1),
            &checks(vec![run("completed", Some("success"))], Some(1))
        ),
        CiRollup::Success
    );
}

#[test]
fn cancelled_timed_out_stale_are_failure() {
    for conclusion in ["cancelled", "timed_out", "stale", "startup_failure"] {
        assert_eq!(
            rollup(
                &empty_status(),
                &checks(vec![run("completed", Some(conclusion))], Some(1))
            ),
            CiRollup::Failure,
            "{conclusion}"
        );
    }
}

#[test]
fn is_safe_commit_sha_table() {
    assert!(is_safe_commit_sha("a"));
    assert!(is_safe_commit_sha(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    assert!(is_safe_commit_sha(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    assert!(is_safe_commit_sha("ABCDEF"));
    assert!(!is_safe_commit_sha(""));
    assert!(!is_safe_commit_sha(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    assert!(!is_safe_commit_sha("not-hex"));
    assert!(!is_safe_commit_sha("../etc"));
    assert!(!is_safe_commit_sha("abc/def"));
}

#[tokio::test]
async fn total_count_larger_than_latest_array_is_not_retryable() {
    // filter=latest can leave total_count > array length after a
    // re-run; Link termination means a single page with no next is
    // complete (PR #436 review).
    let server = MockServer::start().await;
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "pending",
            "total_count": 0,
            "statuses": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 2,
            "check_runs": [success_run()]
        })))
        .mount(&server)
        .await;

    let state_dir = tempdir("ci-total-gt-array");
    let client = client_for(&server, &state_dir);
    let rollup = fetch_ci_rollup(&client, "owner", "r", sha)
        .await
        .expect("complete single page must admit");
    assert_eq!(rollup, CiRollup::Success);
}

#[tokio::test]
async fn link_header_two_page_rollup() {
    let server = MockServer::start().await;
    let sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let page2 = format!(
        "{}/repos/owner/r/commits/{sha}/check-runs?filter=latest&per_page=100&page=2",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/status")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "state": "pending",
            "total_count": 0,
            "statuses": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", format!("<{page2}>; rel=\"next\""))
                .set_body_json(serde_json::json!({
                    "total_count": 2,
                    "check_runs": [success_run()]
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 2,
            "check_runs": [failure_run()]
        })))
        .mount(&server)
        .await;

    let state_dir = tempdir("ci-two-page");
    let client = client_for(&server, &state_dir);
    let rollup = fetch_ci_rollup(&client, "owner", "r", sha)
        .await
        .expect("two pages roll up");
    assert_eq!(rollup, CiRollup::Failure);
}

#[tokio::test]
async fn check_runs_304_preserves_link_for_page_two() {
    let server = MockServer::start().await;
    let sha = "cccccccccccccccccccccccccccccccccccccccc";
    let page2 = format!(
        "{}/repos/owner/r/commits/{sha}/check-runs?filter=latest&per_page=100&page=2",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/status")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"status1\"")
                .set_body_json(serde_json::json!({
                    "state": "pending",
                    "total_count": 0,
                    "statuses": []
                })),
        )
        .mount(&server)
        .await;

    // Page 1: first GET (no If-None-Match) → 200 + ETag + Link.
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .and(query_param_is_missing("page"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"page1\"")
                .insert_header("Link", format!("<{page2}>; rel=\"next\""))
                .set_body_json(serde_json::json!({
                    "total_count": 2,
                    "check_runs": [success_run()]
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    // Page 1 revalidation → 304 without Link (GitHub behaviour).
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .and(query_param_is_missing("page"))
        .and(wiremock::matchers::header("if-none-match", "\"page1\""))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&server)
        .await;
    // Page 2 served both polls (distinct URL / cache key).
    Mock::given(method("GET"))
        .and(path(format!("/repos/owner/r/commits/{sha}/check-runs")))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"page2\"")
                .set_body_json(serde_json::json!({
                    "total_count": 2,
                    "check_runs": [failure_run()]
                })),
        )
        .expect(2)
        .mount(&server)
        .await;

    let state_dir = tempdir("ci-304-link");
    let client = client_for(&server, &state_dir);
    let first = fetch_ci_rollup(&client, "owner", "r", sha)
        .await
        .expect("first poll");
    assert_eq!(first, CiRollup::Failure);

    // Same client / cache: page-1 304 must still follow cached Link.
    let second = fetch_ci_rollup(&client, "owner", "r", sha)
        .await
        .expect("304 must not drop page 2");
    assert_eq!(second, CiRollup::Failure);
}
