//! Integration tests for the durable investigation finalization
//! checkpoint (issue #120).
//!
//! The fresh path persists `InvestigationReady` (queue + SQLite) before
//! the findings comment POST and `InvestigationCommented` after it
//! succeeds, mirroring the code-ticket durable-checkpoint pattern from
//! #89/#103. These tests prove the queue entry records both stages and
//! that a crash after the comment does not duplicate it on recovery
//! (the idempotent `run_id` marker suppresses the re-post).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::finalize::{
    post_investigation_comment_and_finalize, FinalizeContext, FinalizeRequest,
    INVESTIGATION_MARKER_PREFIX,
};
use caduceus::github::Client;
use caduceus::issue::IssueDetail;
use caduceus::queue::{
    ClaimToken, FinalizationCheckpoint, FinalizationStage, Phase, StateStore, TicketType,
};
use caduceus::worker::{WorkerResult, WorkerStatus};
use caduceus::worktree::{RepositoryInfo, Worktree};
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::MockGitHub;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-investigation-finalize-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
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

fn make_worker_result(investigation: bool) -> WorkerResult {
    let mut artifacts = BTreeMap::new();
    artifacts.insert("k".to_string(), json!("v"));
    WorkerResult {
        status: WorkerStatus::Success,
        summary: "findings summary".to_string(),
        commit_message: "fix: sample".to_string(),
        pull_request_title: "PR title".to_string(),
        artifacts,
        investigation,
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

fn checkpoint(
    run_id: &str,
    branch_name: &str,
    result_path: &Path,
    stage: FinalizationStage,
) -> FinalizationCheckpoint {
    FinalizationCheckpoint {
        run_id: run_id.to_string(),
        branch_name: branch_name.to_string(),
        result_path: result_path.to_path_buf(),
        stage,
        commit_oid: None,
        pr_number: None,
        pr_url: None,
    }
}

/// The durable checkpoint sequence `run_investigation_finalize` follows:
/// `InvestigationReady` (SQLite + queue) → findings comment POST →
/// `InvestigationCommented` (SQLite + queue).
#[tokio::test]
async fn investigation_checkpoint_persisted_around_comment() {
    let gh = MockGitHub::start().await;

    // No existing comment: the idempotency scan finds nothing.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(gh.server())
        .await;

    // The findings comment is posted exactly once.
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 7 })))
        .expect(1)
        .mount(gh.server())
        .await;

    // Set up the state store and claim an investigation ticket.
    let root = tempdir("state");
    let store = StateStore::open(&root).expect("open store");
    let key = caduceus::issue::IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    };
    store
        .enqueue(&key, TicketType::Investigation, false)
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

    // The exact checkpoint sequence run_investigation_finalize performs:
    // InvestigationReady before the effect, InvestigationCommented after.
    let result_path = root.join("runs").join("RUN1.result.json");
    store
        .save_finalization(
            &claimed.claim,
            checkpoint(
                "RUN1",
                &ctx.worktree.branch_name,
                &result_path,
                FinalizationStage::InvestigationReady,
            ),
        )
        .expect("save InvestigationReady");

    let output = post_investigation_comment_and_finalize(
        &ctx,
        ctx.client.as_ref(),
        &make_worker_result(true),
        &cfg.ticket_label_investigation,
    )
    .await
    .expect("post investigation comment");
    assert_eq!(
        output.action,
        caduceus::finalize::FinalizeAction::InvestigationCommented
    );

    store
        .save_finalization(
            &claimed.claim,
            checkpoint(
                "RUN1",
                &ctx.worktree.branch_name,
                &result_path,
                FinalizationStage::InvestigationCommented,
            ),
        )
        .expect("save InvestigationCommented");

    // The queue entry durably records the terminal investigation stage.
    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry present");
    assert_eq!(entry.phase, Phase::InProgress, "phase unchanged by save");
    let finalization = entry
        .finalization
        .as_ref()
        .expect("finalization checkpoint persisted");
    assert_eq!(
        finalization.stage,
        FinalizationStage::InvestigationCommented
    );
    assert_eq!(finalization.run_id, "RUN1");
}

/// Acceptance test for issue #120: a crash after the findings comment
/// leaves the queue entry at `InvestigationCommented`; recovery must
/// finish the entry without re-posting the comment. The resume path
/// re-invokes `post_investigation_comment_and_finalize` under the
/// original `run_id`, whose marker suppresses the duplicate POST.
#[tokio::test]
async fn crash_after_comment_no_duplicate_on_recovery() {
    let gh = MockGitHub::start().await;

    // The comment already exists on the issue (posted before the crash),
    // carrying the original run's marker.
    let existing_body = format!(
        "{}{}\n\nfindings summary",
        INVESTIGATION_MARKER_PREFIX, "RUN1"
    );
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 7, "body": existing_body }
        ])))
        .expect(1)
        .mount(gh.server())
        .await;

    // Recovery must NOT re-post the comment: the marker scan finds the
    // original comment, so the POST is never issued.
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 8 })))
        .expect(0)
        .mount(gh.server())
        .await;

    let root = tempdir("state");
    let store = StateStore::open(&root).expect("open store");
    let key = caduceus::issue::IssueKey {
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        number: 1,
    };
    store
        .enqueue(&key, TicketType::Investigation, false)
        .expect("enqueue");
    let now = Utc::now();
    let claimed = store
        .acquire_next("RUN2", 1, now)
        .expect("acquire")
        .expect("claimed entry");

    // Simulate the crash state: the queue entry already carries the
    // durable InvestigationCommented checkpoint from the prior run.
    store
        .save_resumed_finalization(
            &claimed.claim,
            checkpoint(
                "RUN1",
                "automation/issue-1",
                &root.join("runs").join("RUN1.result.json"),
                FinalizationStage::InvestigationCommented,
            ),
        )
        .expect("save resumed checkpoint");

    let cfg = empty_config(&root);
    let issue = make_issue();
    let mut ctx = make_context(&cfg, &issue, "RUN1");
    ctx.claim = claimed.claim.clone();
    ctx.client = Arc::new(client_for(&gh));

    // Recovery re-invokes the comment step with the ORIGINAL run id
    // (the resume path derives ctx.run_id from the queue checkpoint).
    let output = post_investigation_comment_and_finalize(
        &ctx,
        ctx.client.as_ref(),
        &make_worker_result(true),
        &cfg.ticket_label_investigation,
    )
    .await
    .expect("recovery re-post");
    assert!(
        output
            .idempotency_observations
            .iter()
            .any(|o| o == "investigation_comment_posted=false"),
        "recovery must observe the existing marker"
    );
}
