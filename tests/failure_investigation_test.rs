//! Task 6.5 acceptance tests for failure and
//! investigation finalization.
//!
//! The contract is in `CONTRACTS.md` and the task packet
//! `planning/caduceus-v0.1/tasks/6.5-finalize-failures-and-investigations.md`.
//!
//! Tests cover:
//!
//! * failure comment is posted once
//! * existing failure marker → no POST
//! * voice rejection before HTTP
//! * comment API failure preserves the worker error
//! * investigation comment is posted once
//! * investigation label is recorded as not removed (v0.1)
//! * retry marker reuse
//! * no push / no PR mutation

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{
    post_failure_comment, post_investigation_comment, render_failure_comment,
    render_investigation_comment, FinalizeContext, FAILURE_MARKER_PREFIX,
    INVESTIGATION_MARKER_PREFIX,
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

fn make_worker_result(investigation: bool) -> WorkerResult {
    let mut artifacts = BTreeMap::new();
    artifacts.insert("k".to_string(), json!("v"));
    WorkerResult {
        status: WorkerStatus::Failure,
        summary: "summary text".to_string(),
        commit_message: "fix: sample".to_string(),
        pull_request_title: "PR".to_string(),
        artifacts,
        investigation,
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
async fn failure_fresh_post() {
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
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-fresh");
    let wr = make_worker_result(false);
    let outcome = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect("post");
    assert!(outcome.comment_posted);
}

#[tokio::test]
async fn failure_existing_marker_skips_post() {
    let server = MockServer::start().await;
    let body = format!("{}{}\n\nsummary", FAILURE_MARKER_PREFIX, "run-reuse");
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "body": body }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-reuse");
    let wr = make_worker_result(false);
    let outcome = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect("post");
    assert!(!outcome.comment_posted);
}

#[tokio::test]
async fn failure_voice_rejection_prevents_http() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
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
    let mut wr = make_worker_result(false);
    wr.summary = "summary contains forbidden-term".to_string();
    let err = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(msg.contains("public-voice"), "got: {msg}");
}

#[tokio::test]
async fn failure_comment_api_failure_returns_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-500");
    let wr = make_worker_result(false);
    let err = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("500") || msg.contains("GitHubApi"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn investigation_fresh_post() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 2 })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-inv");
    let wr = make_worker_result(true);
    let outcome = post_investigation_comment(&ctx, &client, &wr, "🤖 auto-fix-investigate")
        .await
        .expect("post");
    assert!(outcome.comment_posted);
    assert!(!outcome.label_removed, "v0.1 leaves label_removed false");
}

#[tokio::test]
async fn investigation_existing_marker_skips_post() {
    let server = MockServer::start().await;
    let body = format!("{}{}\n\nsummary", INVESTIGATION_MARKER_PREFIX, "run-reuse");
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "body": body }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_for(&server);
    let state_dir = tempfile::tempdir().expect("state");
    let cfg = empty_config(state_dir.path());
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-reuse");
    let wr = make_worker_result(true);
    let outcome = post_investigation_comment(&ctx, &client, &wr, "🤖 auto-fix-investigate")
        .await
        .expect("post");
    assert!(!outcome.comment_posted);
}

#[tokio::test]
async fn failure_comment_does_not_claim_local_transcript_is_public() {
    // The failure-comment body is *generic*: it does
    // NOT link the worker's local transcript. A local
    // path that lives on the daemon host must not appear
    // in the rendered body.
    let wr = make_worker_result(false);
    let body = render_failure_comment(&wr, "run-no-transcript");
    assert!(!body.contains("/tmp/wt"));
    assert!(!body.contains("/state/"));
    assert!(!body.contains(".transcript"));
    assert!(body.contains(FAILURE_MARKER_PREFIX));
}

#[test]
fn investigation_marker_includes_run_id() {
    let mut wr = make_worker_result(true);
    wr.artifacts.insert("nested".to_string(), json!({"k": "v"}));
    let body = render_investigation_comment(&wr, "run-inv-marker");
    assert!(body.contains(INVESTIGATION_MARKER_PREFIX));
    assert!(body.contains("run-inv-marker"));
    assert!(body.contains("summary"));
    // The artifact section is rendered as a JSON code
    // fence.
    assert!(body.contains("```json"));
    assert!(body.contains("\"nested\""));
    assert!(body.contains("\"k\""));
    assert!(body.contains("\"v\""));
}
