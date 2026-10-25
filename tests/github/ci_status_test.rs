//! Pure CI rollup truth table and fetch-layer pagination
//! (issue #435).

use caduceus::config::Config;
use caduceus::github::{
    fetch_ci_rollup, rollup, CheckRun, CheckRunsPage, CiLookupError, CiRollup, Client,
    CombinedStatus, HttpCache,
};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

#[test]
fn empty_empty_is_absent_even_when_combined_state_is_pending() {
    assert_eq!(
        rollup(&empty_status(), &checks(vec![], Some(0))),
        CiRollup::Absent
    );
}

#[test]
fn legacy_failure_is_failure() {
    // Pure rollup with empty checks (fetch short-circuits before
    // check-runs, but the pure function still classifies legacy
    // failure on its own).
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
    // Treated as Pending by the pure function; the fetch layer
    // surfaces Retryable instead so discovery never records a
    // durable failure for it.
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

#[tokio::test]
async fn short_check_runs_page_is_retryable() {
    // One page, no Link header, total_count larger than the runs
    // array: must not roll up to Success (unseen page could fail).
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
        .and(path_regex(format!(
            r"^/repos/owner/r/commits/{sha}/check-runs"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 2,
            "check_runs": [{
                "status": "completed",
                "conclusion": "success"
            }]
        })))
        .mount(&server)
        .await;

    let state_dir = tempdir("ci-short-page");
    let mut cfg = Config::test_defaults(&state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some("ghp_testtoken_value_xyz".to_string());
    let cache = HttpCache::open(&state_dir).expect("cache");
    let client = Client::with_cache(&cfg, cache).expect("client");

    let err = fetch_ci_rollup(&client, "owner", "r", sha)
        .await
        .expect_err("incomplete page must not admit");
    assert!(matches!(err, CiLookupError::Retryable(_)), "got {err:?}");
}
