//! Acceptance tests for issue #119: code-ticket pre-PR finalization
//! must persist a durable queue `FinalizationCheckpoint` at every
//! stage (`ResultValidated` / `Committed` / `Pushed`), so a crash
//! after a commit or push resumes at the recorded stage instead of
//! re-dispatching the worker and creating a duplicate branch/commit.
//!
//! Each test drives the leaf finalization functions
//! (`commit_code_and_finalize`, `push_and_finalize`,
//! `find_or_create_pr_and_finalize`, `post_completion_only`) against
//! a real git remote and a mock GitHub API, simulates the crash state
//! (SQLite + queue checkpoints carrying the *original* run id), then
//! re-runs the resume arm's steps exactly as `run_resume_finalization`
//! would — asserting exactly one commit, one branch, one PR, one
//! comment.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::daemon::tick::resume::{resume_from_checkpoint, ResumeAction};
use caduceus::finalize::voice::generate_operation_id;
use caduceus::finalize::{
    archive_worker_result, commit_code_and_finalize, find_or_create_pr_and_finalize,
    post_completion_only, push_and_finalize, FinalizeContext, FinalizeRequest,
};
use caduceus::github::Client;
use caduceus::issue::IssueDetail;
use caduceus::queue::{
    ClaimToken, FinalizationCheckpoint, FinalizationStage, StateStore, TicketType,
};
use caduceus::state::checkpoints::persist_checkpoint;
use caduceus::state::store;
use caduceus::worker::{WorkerResult, WorkerStatus};
use caduceus::worktree::{create as create_worktree, GitRunner, RepositoryInfo, Worktree};
use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::MockGitHub;

const TEST_TOKEN: &str = "ghp_testtoken_value_xyz";
const EXPECTED_PR_NUMBER: u64 = 4242;

fn tempdir(label: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-code-pre-pr-{label}-{nonce}"));
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

fn sh(dir: &Path, op: &str, args: &[&str]) -> String {
    let out = Command::new(op)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("{op} {args:?}: {err}"));
    assert!(
        out.status.success(),
        "{op} {args:?} failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_bare(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    let _ = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(dir)
        .output()
        .expect("git init");
}

fn init_clone(bare: &Path, clone: &Path) {
    fs::create_dir_all(clone).expect("mkdir");
    let out = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(bare)
        .arg(clone)
        .output()
        .expect("git clone");
    assert!(out.status.success(), "clone failed");
    let _ = Command::new("git")
        .args(["config", "user.email", "seed@example.com"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Seed"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["checkout", "-q", "-b", "main"])
        .current_dir(clone)
        .output();
    fs::write(clone.join("README.md"), "base\n").expect("write");
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(clone)
        .output();
}

fn client_for(gh: &MockGitHub) -> Client {
    let state_dir = tempfile::tempdir().expect("state");
    let mut cfg = empty_config(state_dir.path());
    cfg.api_base = gh.uri();
    cfg.github_token = Some(TEST_TOKEN.to_string());
    Client::with_config(&cfg).expect("client")
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

/// Real worktree-backed context. `run_id` is the run the checkpoints
/// carry (the *original* run on the resume path); `claim` is patched
/// by the caller to the live claim token.
fn make_context(
    cfg: &Config,
    wt: &Worktree,
    issue: &IssueDetail,
    run_id: &str,
    remote_url: &str,
) -> FinalizeContext {
    let claim = ClaimToken::for_test(cfg.state_dir.join("claims"), "deadbeef00", run_id);
    FinalizeContext {
        client: inert_client(),
        config: cfg.clone(),
        repository: RepositoryInfo {
            path: wt.path.parent().unwrap().parent().unwrap().to_path_buf(),
            base_branch: "main".to_string(),
            remote_url: remote_url.to_string(),
        },
        issue: issue.clone(),
        claim,
        run_id: run_id.to_string(),
        worktree: wt.clone(),
        result: FinalizeRequest {
            issue: issue.key.clone(),
            branch_name: wt.branch_name.clone(),
            worktree_path: wt.path.clone(),
        },
    }
}

fn checkpoint(
    run_id: &str,
    branch_name: &str,
    result_path: &Path,
    stage: FinalizationStage,
    commit_oid: Option<String>,
    pr_number: Option<u64>,
    pr_url: Option<String>,
) -> FinalizationCheckpoint {
    FinalizationCheckpoint {
        run_id: run_id.to_string(),
        branch_name: branch_name.to_string(),
        result_path: result_path.to_path_buf(),
        stage,
        commit_oid,
        pr_number,
        pr_url,
    }
}

/// Persist a SQLite checkpoint exactly as the resume module's private
/// `checkpoint()` helper does (deterministic operation id, marker).
fn checkpoint_sqlite(
    conn: &rusqlite::Connection,
    run_id: &str,
    stage: FinalizationStage,
    marker: Option<&str>,
) {
    persist_checkpoint(
        conn,
        run_id,
        stage,
        None,
        Some(&generate_operation_id(run_id, stage.as_str())),
        marker,
    )
    .expect("persist checkpoint");
}

/// Real git setup: bare remote, seeded main clone, and a worktree for
/// the issue on branch `automation/issue-1-<run_id>`.
async fn setup_git(
    base: &Path,
    state_dir: &Path,
    run_id: &str,
) -> (Config, GitRunner, Worktree, IssueDetail, String) {
    let bare = base.join("owner.git");
    let clone = base.join("owner").join("repo");
    init_bare(&bare);
    init_clone(&bare, &clone);
    let cfg = empty_config(state_dir);
    let issue = make_issue();
    let info = RepositoryInfo {
        path: clone,
        base_branch: "main".to_string(),
        remote_url: bare.to_str().expect("utf8").to_string(),
    };
    let runner = GitRunner::new(&cfg);
    let wt = create_worktree(&cfg, &runner, &info, &issue.key, run_id)
        .await
        .expect("create worktree");
    (
        cfg,
        runner,
        wt,
        issue,
        bare.to_str().expect("utf8").to_string(),
    )
}

fn commits_ahead_of_main(wt: &Path) -> usize {
    let out = sh(wt, "git", &["rev-list", "--count", "origin/main..HEAD"]);
    out.parse().expect("commit count")
}

fn remote_branch_count(bare: &str) -> usize {
    let out = Command::new("git")
        .args(["ls-remote", "--heads", bare])
        .output()
        .unwrap_or_else(|err| panic!("ls-remote {bare}: {err}"));
    assert!(out.status.success(), "ls-remote failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("refs/heads/automation/issue-1"))
        .count()
}

async fn mount_pr_mocks(gh: &MockGitHub, branch: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls"))
        .and(query_param("state", "open"))
        .and(query_param("head", format!("owner:{branch}")))
        .and(query_param("base", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": EXPECTED_PR_NUMBER,
            "html_url": format!("https://github.com/owner/repo/pull/{EXPECTED_PR_NUMBER}"),
        })))
        .expect(1)
        .mount(gh.server())
        .await;
}

async fn mount_comment_mocks(gh: &MockGitHub) {
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(gh.server())
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 7 })))
        .expect(1)
        .mount(gh.server())
        .await;
}

/// Set up the store, claim a code ticket for the recovery tick (RUN2),
/// and build a context whose run_id is the ORIGINAL run (RUN1) — the
/// durable checkpoint cursor — with the live RUN2 claim token.
///
/// The seeded checkpoint's `commit_oid` is `None`. Tests that need to
/// patch in the real `commit_oid` from a pre-crash `commit_code_and_finalize`
/// call do so with an explicit `save_resumed_finalization` overwrite
/// after this helper returns.
#[allow(clippy::too_many_arguments)]
fn seed_recovery_state(
    root: &Path,
    cfg: &Config,
    wt: &Worktree,
    issue: &IssueDetail,
    remote_url: &str,
    gh: &MockGitHub,
    crash_stage: FinalizationStage,
) -> (
    StateStore,
    caduceus::issue::IssueKey,
    FinalizeContext,
    std::path::PathBuf,
) {
    let store = StateStore::open(root).expect("open store");
    let key = issue.key.clone();
    store
        .enqueue(&key, TicketType::Code, false)
        .expect("enqueue");
    let now = Utc::now();
    let claimed = store
        .acquire_next("RUN2", 1, now)
        .expect("acquire")
        .expect("claimed entry");

    let mut ctx = make_context(cfg, wt, issue, "RUN1", remote_url);
    ctx.claim = claimed.claim.clone();
    ctx.client = Arc::new(client_for(gh));

    // Archive a worker result under the original run id.
    let worktree_result = wt.path.join("worker-result.json");
    fs::write(&worktree_result, r#"{"status":"success"}"#).expect("write result");
    let archive_path =
        archive_worker_result(&worktree_result, &cfg.state_dir, "RUN1").expect("archive result");

    // Seed the crash state: the queue entry durably carries the
    // checkpoint at the crashed stage, anchored to the ORIGINAL run.
    // The seeded commit_oid is None; tests that need it real (to mirror
    // what `run_code_finalize` would have written) overwrite the
    // checkpoint after committing.
    store
        .save_resumed_finalization(
            &ctx.claim,
            checkpoint(
                "RUN1",
                &wt.branch_name,
                &archive_path,
                crash_stage,
                None,
                None,
                None,
            ),
        )
        .expect("seed crash-state queue checkpoint");

    (store, key, ctx, archive_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_commit_resumes_at_pushed_one_commit_one_branch() {
    let gh = MockGitHub::start().await;
    mount_pr_mocks(&gh, "automation/issue-1-run1").await;
    mount_comment_mocks(&gh).await;

    let root = tempdir("state");
    let (cfg, runner, wt, issue, bare) = setup_git(&root.join("git"), &root, "run1").await;

    let (store, key, ctx, archive_path) = seed_recovery_state(
        &root,
        &cfg,
        &wt,
        &issue,
        &bare,
        &gh,
        FinalizationStage::Committed,
    );

    // Pre-crash work: the commit landed on the daemon branch.
    fs::write(wt.path.join("README.md"), "modified\n").expect("write");
    let result = make_worker_result();
    let commit_out =
        commit_code_and_finalize(&ctx, &result, &runner, &archive_path).expect("pre-crash commit");
    assert_eq!(commits_ahead_of_main(&wt.path), 1, "exactly one commit");

    // Patch the seeded queue checkpoint with the real commit_oid so
    // the test reflects what `run_code_finalize` would have written at
    // this stage on the fresh path. (Future-proofs against any change
    // that starts reading finalization.commit_oid for routing.)
    store
        .save_resumed_finalization(
            &ctx.claim,
            checkpoint(
                "RUN1",
                &wt.branch_name,
                &archive_path,
                FinalizationStage::Committed,
                commit_out.commit_oid.clone(),
                None,
                None,
            ),
        )
        .expect("patch seeded commit_oid");

    // The fresh path persisted SQLite checkpoints through Committed
    // (SQLite first, then queue — the queue save is the crash state).
    let conn = store::open_in(&cfg.state_dir).expect("open conn");
    checkpoint_sqlite(&conn, "RUN1", FinalizationStage::ResultValidated, None);
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Committed,
        commit_out.commit_oid.as_deref(),
    );

    // Recovery routing: the durable queue checkpoint's run_id (RUN1)
    // selects the SQLite cursor and resumes at Pushed — NOT a fresh
    // dispatch. Consulting the fresh claim run_id (RUN2) instead would
    // find no checkpoints and re-dispatch (the pre-fix bug).
    match resume_from_checkpoint(&conn, "RUN1").expect("resume") {
        ResumeAction::Skip(FinalizationStage::Pushed) => {}
        other => panic!("expected Skip(Pushed), got {other:?}"),
    }
    assert!(
        matches!(
            resume_from_checkpoint(&conn, "RUN2").expect("resume"),
            ResumeAction::StartFresh
        ),
        "fresh run must find no checkpoints"
    );

    // Resume the Pushed arm (mirrors run_resume_finalization): re-run
    // push (the real first push — nothing was pushed before the crash),
    // then PR create + completion comment, persisting the queue
    // checkpoint at each stage.
    let push_out = push_and_finalize(&ctx, &runner).await.expect("resume push");
    assert_eq!(remote_branch_count(&bare), 1, "exactly one remote branch");
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Pushed,
        push_out.pushed_oid.as_deref(),
    );

    let pr_output = find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &result)
        .await
        .expect("resume PR create");
    store
        .save_resumed_finalization(
            &ctx.claim,
            checkpoint(
                "RUN1",
                &wt.branch_name,
                &archive_path,
                FinalizationStage::PrCreated,
                commit_out.commit_oid.clone(),
                pr_output.pr_number,
                pr_output.pr_url,
            ),
        )
        .expect("save resumed PrCreated");
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::PrCreated,
        pr_output.pr_number.map(|n| n.to_string()).as_deref(),
    );

    let comment_out = post_completion_only(&ctx, ctx.client.as_ref(), &result)
        .await
        .expect("resume comment");
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Commented,
        comment_out.comment_id.map(|n| n.to_string()).as_deref(),
    );

    // Acceptance: exactly one commit and one branch survive recovery.
    assert_eq!(commits_ahead_of_main(&wt.path), 1, "exactly one commit");
    assert_eq!(remote_branch_count(&bare), 1, "exactly one remote branch");

    // The queue entry durably records the terminal resume stage under
    // the original run.
    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry present");
    let finalization = entry
        .finalization
        .as_ref()
        .expect("finalization checkpoint persisted");
    assert_eq!(finalization.stage, FinalizationStage::PrCreated);
    assert_eq!(finalization.run_id, "RUN1");
    assert_eq!(finalization.pr_number, Some(EXPECTED_PR_NUMBER));
}

#[tokio::test(flavor = "multi_thread")]
async fn crash_after_push_resumes_at_pr_created_one_commit_one_branch() {
    let gh = MockGitHub::start().await;
    mount_pr_mocks(&gh, "automation/issue-1-run1").await;
    mount_comment_mocks(&gh).await;

    let root = tempdir("state");
    let (cfg, runner, wt, issue, bare) = setup_git(&root.join("git"), &root, "run1").await;

    let (store, key, ctx, archive_path) = seed_recovery_state(
        &root,
        &cfg,
        &wt,
        &issue,
        &bare,
        &gh,
        FinalizationStage::Pushed,
    );

    // Pre-crash work: commit AND push both landed.
    fs::write(wt.path.join("README.md"), "modified\n").expect("write");
    let result = make_worker_result();
    let commit_out =
        commit_code_and_finalize(&ctx, &result, &runner, &archive_path).expect("pre-crash commit");
    let push_out = push_and_finalize(&ctx, &runner)
        .await
        .expect("pre-crash push");
    assert_eq!(commits_ahead_of_main(&wt.path), 1, "exactly one commit");
    assert_eq!(remote_branch_count(&bare), 1, "exactly one remote branch");

    // Patch the seeded queue checkpoint with the real commit_oid so
    // the test reflects what `run_code_finalize` would have written at
    // the Pushed stage on the fresh path. (Future-proofs against any
    // change that starts reading finalization.commit_oid for routing.
    // Note: the resume `Pushed` arm uses commit_oid: None since it
    // doesn't reconstruct commit_out — that's a deliberate asymmetry
    // flagged in the plan, not a bug.)
    store
        .save_resumed_finalization(
            &ctx.claim,
            checkpoint(
                "RUN1",
                &wt.branch_name,
                &archive_path,
                FinalizationStage::Pushed,
                commit_out.commit_oid.clone(),
                None,
                None,
            ),
        )
        .expect("patch seeded commit_oid");

    // The fresh path persisted SQLite checkpoints through Pushed.
    let conn = store::open_in(&cfg.state_dir).expect("open conn");
    checkpoint_sqlite(&conn, "RUN1", FinalizationStage::ResultValidated, None);
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Committed,
        commit_out.commit_oid.as_deref(),
    );
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Pushed,
        push_out.pushed_oid.as_deref(),
    );

    // Recovery routing: resume at PrCreated (push already durable).
    match resume_from_checkpoint(&conn, "RUN1").expect("resume") {
        ResumeAction::Skip(FinalizationStage::PrCreated) => {}
        other => panic!("expected Skip(PrCreated), got {other:?}"),
    }

    // Resume the PrCreated arm (mirrors run_resume_finalization): PR
    // create-or-reuse + completion comment. No push is re-issued, so
    // the remote branch is not duplicated.
    let pr_output = find_or_create_pr_and_finalize(&ctx, ctx.client.as_ref(), &result)
        .await
        .expect("resume PR create");
    store
        .save_resumed_finalization(
            &ctx.claim,
            checkpoint(
                "RUN1",
                &wt.branch_name,
                &archive_path,
                FinalizationStage::PrCreated,
                None,
                pr_output.pr_number,
                pr_output.pr_url,
            ),
        )
        .expect("save resumed PrCreated");
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::PrCreated,
        pr_output.pr_number.map(|n| n.to_string()).as_deref(),
    );

    let comment_out = post_completion_only(&ctx, ctx.client.as_ref(), &result)
        .await
        .expect("resume comment");
    checkpoint_sqlite(
        &conn,
        "RUN1",
        FinalizationStage::Commented,
        comment_out.comment_id.map(|n| n.to_string()).as_deref(),
    );

    // Acceptance: exactly one commit and one branch survive recovery —
    // the remote branch count stays 1 after resume.
    assert_eq!(commits_ahead_of_main(&wt.path), 1, "exactly one commit");
    assert_eq!(remote_branch_count(&bare), 1, "exactly one remote branch");

    let snap = store.snapshot().expect("snapshot");
    let entry = snap.entry(&key).expect("entry present");
    let finalization = entry
        .finalization
        .as_ref()
        .expect("finalization checkpoint persisted");
    assert_eq!(finalization.stage, FinalizationStage::PrCreated);
    assert_eq!(finalization.run_id, "RUN1");
    assert_eq!(finalization.pr_number, Some(EXPECTED_PR_NUMBER));
}
