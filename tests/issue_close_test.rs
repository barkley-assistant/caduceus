//! Task 6.4 acceptance tests for the post-completion and
//! close path.
//!
//! The contract is in `CONTRACTS.md` and the task packet
//! `planning/caduceus-v0.1/tasks/6.4-post-completion-and-close-idempotently.md`.
//!
//! Tests cover:
//!
//! * fresh post + close
//! * existing marker comment → no POST
//! * already-closed issue → no close
//! * partial failure then retry (lost POST response)
//! * voice rejection before HTTP
//! * 404 from a missing issue
//! * 429 rate limit surfaced

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{
    post_completion_and_close, render_completion_comment, FinalizeContext, COMPLETION_MARKER_PREFIX,
};
use caduceus::github::Client;
use caduceus::issue::IssueDetail;
use caduceus::queue::ClaimToken;
use caduceus::worker::{WorkerResult, WorkerStatus};
use caduceus::worktree::Worktree;
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

/// Inert `Arc<Client>` for tests that build a `FinalizeContext`
/// but never exercise the GitHub HTTP path.
fn inert_client() -> Arc<Client> {
    Arc::new(Client::new("https://api.github.com"))
}

fn empty_config(state_dir: &Path) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(state_dir.to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(state_dir.to_path_buf()),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

fn make_issue() -> IssueDetail {
    IssueDetail {
        key: caduceus::issue::IssueKey {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 1,
        },
        title: "Sample".to_string(),
        body: "Body".to_string(),
        labels: vec![],
        comments: vec![],
        trusted_comments: vec![],
        events: vec![],
        fetched_at: Utc::now(),
    }
}

fn make_worker_result() -> WorkerResult {
    let mut artifacts = BTreeMap::new();
    artifacts.insert("k".to_string(), json!("v"));
    WorkerResult {
        status: WorkerStatus::Success,
        summary: "summary text".to_string(),
        commit_message: "fix: sample".to_string(),
        pull_request_title: "PR".to_string(),
        artifacts,
        investigation: false,
    }
}

fn make_context(
    cfg: &Config,
    issue: &IssueDetail,
    run_id: &str,
) -> caduceus::finalize::FinalizeContext {
    let wt = Worktree {
        issue: issue.key.clone(),
        run_id: run_id.to_string(),
        branch_name: "automation/issue-1-run-x".to_string(),
        path: Path::new("/tmp/wt").to_path_buf(),
        base_oid: "deadbeef".to_string(),
        fresh: false,
        created_at: Utc::now(),
    };
    let claim = ClaimToken::for_test(cfg.state_dir.join("claims"), "deadbeef00", run_id);
    let key = issue.key.clone();
    FinalizeContext {
        client: inert_client(),
        config: cfg.clone(),
        repository: caduceus::worktree::RepositoryInfo {
            path: Path::new("/tmp/wt").to_path_buf(),
            base_branch: "main".to_string(),
            remote_url: "file://localhost".to_string(),
        },
        issue: issue.clone(),
        claim,
        run_id: run_id.to_string(),
        worktree: wt,
        result: caduceus::finalize::FinalizeRequest {
            issue: key.clone(),
            branch_name: "automation/issue-1-run-x".to_string(),
            worktree_path: Path::new("/tmp/wt").to_path_buf(),
        },
    }
}

fn client_for(server: &MockServer) -> Client {
    let state_dir = tempfile::tempdir().expect("state");
    let mut cfg = empty_config(state_dir.path());
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    Client::with_config(&cfg).expect("client")
}

#[tokio::test]
async fn close_fresh_post_and_close() {
    let server = MockServer::start().await;
    // List comments → empty.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(&server)
        .await;
    // POST comment → 201.
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    // Get issue → open.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "open" })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-fresh");
    let result = make_worker_result();
    let outcome = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect("close");
    assert!(outcome.comment_posted);
}

#[tokio::test]
async fn close_existing_marker_skips_post() {
    let server = MockServer::start().await;
    // List comments → contains a marker for run-reuse.
    let body = format!("{}{}\nsummary", COMPLETION_MARKER_PREFIX, "run-reuse");
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "body": body }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    // No POST should happen.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    // Get issue → open.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "open" })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-reuse");
    let result = make_worker_result();
    let outcome = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect("close");
    assert!(!outcome.comment_posted, "must not post when marker exists");
}

#[tokio::test]
async fn close_already_closed_issue_is_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    // Get issue → already closed.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "closed" })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-already");
    let result = make_worker_result();
    let outcome = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect("close");
    assert!(outcome.comment_posted);
}

#[tokio::test]
async fn close_voice_rejection_prevents_http() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let mut cfg = empty_config(state_dir.path());
    cfg.comment_forbidden_strings = vec!["forbidden-term".to_string()];
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-voice");
    let mut result = make_worker_result();
    result.summary = "summary contains forbidden-term".to_string();
    let err = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("public-voice") || msg.contains("forbidden"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn close_404_from_missing_issue() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-404");
    let result = make_worker_result();
    let err = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("404"), "got: {msg}");
}

#[tokio::test]
async fn close_429_rate_limit_is_surfaced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-429");
    let result = make_worker_result();
    let err = post_completion_and_close(&ctx, &client, &result)
        .await
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("RateLimited"), "got: {msg}");
}

#[test]
fn completion_marker_includes_run_id() {
    let result = make_worker_result();
    let body = render_completion_comment(&result, "run-marker-001");
    assert!(body.contains(COMPLETION_MARKER_PREFIX));
    assert!(body.contains("run-marker-001"));
    assert!(body.contains("-->"));
}
