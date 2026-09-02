//! Integration test: the PR number returned by GitHub during code-ticket
//! finalization must reach `QueueEntry.finalization.pr_number`.
//!
//! The fixture drives `find_or_create_pr_and_finalize` against a mock
//! GitHub API, then saves the returned checkpoint through
//! `StateStore::save_finalization` exactly as the daemon's resume path
//! does, and asserts the queue entry carries the PR number.

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{find_or_create_pr_and_finalize, FinalizeContext, FinalizeRequest};
use caduceus::github::Client;
use caduceus::issue::IssueDetail;
use caduceus::queue::{
    ClaimToken, FinalizationCheckpoint, FinalizationStage, Phase, StateStore, TicketType,
};
use caduceus::worker::{WorkerResult, WorkerStatus};
use caduceus::worktree::{RepositoryInfo, Worktree};
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use fixtures::MockGitHub;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const EXPECTED_PR_NUMBER: u64 = 4242;

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

fn inert_client() -> Arc<Client> {
    Arc::new(Client::new("https://api.github.com"))
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
        summary: "summary".to_string(),
        commit_message: "fix: sample".to_string(),
        pull_request_title: "PR title".to_string(),
        artifacts,
        investigation: false,
    }
}

fn make_context(cfg: &Config, issue: &IssueDetail, run_id: &str) -> FinalizeContext {
    let branch_name = "automation/issue-1".to_string();
    let wt = Worktree {
        issue: issue.key.clone(),
        run_id: run_id.to_string(),
        branch_name: branch_name.clone(),
        path: Path::new("/tmp/wt").to_path_buf(),
        main_path: Path::new("/tmp/repo").to_path_buf(),
        base_oid: "deadbeef".to_string(),
        fresh: false,
        created_at: Utc::now(),
    };
    let claim = ClaimToken::for_test(cfg.state_dir.join("claims"), "deadbeef00", run_id);
    FinalizeContext {
        client: inert_client(),
        config: cfg.clone(),
        repository: RepositoryInfo {
            path: Path::new("/tmp/repo").to_path_buf(),
            base_branch: "main".to_string(),
            remote_url: "file://localhost".to_string(),
        },
        issue: issue.clone(),
        claim,
        run_id: run_id.to_string(),
        worktree: wt,
        result: FinalizeRequest {
            issue: issue.key.clone(),
            branch_name,
            worktree_path: Path::new("/tmp/wt").to_path_buf(),
        },
    }
}

fn client_for(gh: &MockGitHub) -> Client {
    let state_dir = tempfile::tempdir().expect("state");
    let mut cfg = empty_config(state_dir.path());
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    Client::with_config(&cfg).expect("client")
}

#[tokio::test]
async fn pr_number_reaches_queue_entry_after_pr_creation() {
    let gh = MockGitHub::start().await;

    // No existing open PR: empty list response.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", "owner:automation/issue-1"))
        .and(query_param("base", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(gh.server())
        .await;

    // GitHub creates the PR and returns its number.
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": EXPECTED_PR_NUMBER,
            "html_url": format!("https://github.com/owner/repo/pull/{EXPECTED_PR_NUMBER}"),
        })))
        .expect(1)
        .mount(gh.server())
        .await;

    // Set up state store and claim a code ticket.
    let root = tempdir("state");
    let store = StateStore::open(&root).expect("open store");
    let key = caduceus::issue::IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    };
    store
        .enqueue(&key, TicketType::Code, false)
        .expect("enqueue");
    let now = Utc::now();
    let claimed = store
        .acquire_next("RUN1", 1, now)
        .expect("acquire")
        .expect("claimed entry");

    // Build the finalization context using the real claim token.
    let cfg = empty_config(&root);
    let issue = make_issue();
    let mut ctx = make_context(&cfg, &issue, "RUN1");
    ctx.claim = claimed.claim.clone();
    ctx.client = Arc::new(client_for(&gh));

    // Run the PR finalization pipeline.
    let output = find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &make_worker_result())
        .await
        .expect("finalize");

    // The output carries the PR number returned by GitHub.
    assert_eq!(output.pr_number, Some(EXPECTED_PR_NUMBER));

    // Persist the checkpoint as the daemon would at every call site.
    let checkpoint = FinalizationCheckpoint {
        run_id: "RUN1".to_string(),
        branch_name: ctx.worktree.branch_name.clone(),
        result_path: root.join("runs").join("RUN1.result.json"),
        stage: FinalizationStage::PrCreated,
        commit_oid: None,
        pr_number: output.pr_number,
        pr_url: output.pr_url,
    };
    store
        .save_finalization(&claimed.claim, checkpoint)
        .expect("save finalization");

    // The queue entry now records the PR number.
    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry present");
    assert_eq!(entry.phase, Phase::InProgress, "phase unchanged by save");
    let finalization = entry
        .finalization
        .as_ref()
        .expect("finalization checkpoint persisted");
    assert_eq!(finalization.pr_number, Some(EXPECTED_PR_NUMBER));
}
