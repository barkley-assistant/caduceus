//! Failure-comment finalization tests (the investigation-comment
//! half was removed in N+1, issue #331).
//!
//! Tests cover:
//!
//! * failure comment is posted once
//! * existing failure marker → no POST
//! * voice rejection before HTTP
//! * comment API failure preserves the worker error
//! * no push / no PR mutation

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{
    post_failure_comment, render_failure_comment, FinalizeContext, FAILURE_MARKER_PREFIX,
};
use caduceus::github::{Client, HttpCache};
use caduceus::issue::IssueDetail;
use caduceus::queue::ClaimToken;
use caduceus::worker::{WorkerResult, WorkerStatus};
use caduceus::worktree::Worktree;
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const ETAG: &str = "\"abc\"";

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
        status: WorkerStatus::Failure,
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
        main_path: Path::new("/tmp/repo").to_path_buf(),
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

/// Cache-backed client for the 304-replay test. The cache must live
/// in an owned `PathBuf` (`fixtures::tempdir`, no auto-cleanup) so it
/// survives past the helper's return — the `client_for` helper drops
/// its `TempDir` (and the cache DB) before any HTTP call.
fn cached_client_for(server: &MockServer) -> Client {
    let state_dir = tempdir("fail-304");
    let mut cfg = empty_config(&state_dir);
    cfg.api_base = server.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    let cache = HttpCache::open(&state_dir).expect("cache opens");
    Client::with_cache(&cfg, cache).expect("client builds")
}

/// Matches requests that do NOT carry the named header. The first
/// (unconditional) GET has no `If-None-Match`; the second does. This
/// disambiguates the two mocks on the same path — same pattern as
/// `tests/github/merge_detect_test.rs`.
struct NoHeader(&'static str);

impl Match for NoHeader {
    fn matches(&self, req: &Request) -> bool {
        !req.headers.contains_key(self.0)
    }
}

/// Mount the two-arm conditional-GET sequence on the issue-comments
/// endpoint: 200 + ETag for the unconditional GET, 304 for the
/// re-GET.
async fn mount_comments_two_arm(server: &MockServer, comments_body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .and(NoHeader("if-none-match"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", ETAG)
                .set_body_json(comments_body),
        )
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .and(header("if-none-match", ETAG))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(server)
        .await;
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
    let wr = make_worker_result();
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
    let wr = make_worker_result();
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
    let mut wr = make_worker_result();
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
    let wr = make_worker_result();
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
async fn failure_comment_does_not_claim_local_transcript_is_public() {
    // The failure-comment body is *generic*: it does
    // NOT link the worker's local transcript. A local
    // path that lives on the daemon host must not appear
    // in the rendered body.
    let wr = make_worker_result();
    let body = render_failure_comment(&wr, "run-no-transcript");
    assert!(!body.contains("/tmp/wt"));
    assert!(!body.contains("/state/"));
    assert!(!body.contains(".transcript"));
    assert!(body.contains(FAILURE_MARKER_PREFIX));
}

#[tokio::test]
async fn failure_304_replay_skips_post() {
    // The ETag-cached second list GET replies 304 and the client
    // replays the cached body, which carries the failure marker —
    // the marker check must parse it like a 200, so no POST and no
    // failure. On current main the second call returns
    // Err(GitHubApi { status: 304 }).
    let server = MockServer::start().await;
    let body = format!("{}{}\n\nsummary", FAILURE_MARKER_PREFIX, "run-fail-304");
    mount_comments_two_arm(&server, serde_json::json!([{ "id": 1, "body": body }])).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let client = cached_client_for(&server);
    let state_dir = tempdir("fail-304-cfg");
    let cfg = empty_config(&state_dir);
    let issue = make_issue();
    let ctx = make_context(&cfg, &issue, "run-fail-304");
    let wr = make_worker_result();
    let first = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect("fresh call succeeds");
    assert!(!first.comment_posted);
    let second = post_failure_comment(&ctx, &client, &wr)
        .await
        .expect("304 replay must not fail (issue #396)");
    assert!(!second.comment_posted);
}
