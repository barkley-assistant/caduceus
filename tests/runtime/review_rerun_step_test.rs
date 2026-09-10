//! Trusted-comment re-review listener integration tests (issue #335,
//! DAR §17): wiremock GitHub + a real local bare remote, mirroring
//! `review_discovery_test.rs`'s StepHarness.
//!
//! Coverage:
//! - Trusted author `/caduceus review` → explicit re-review enqueued
//!   for the current head SHA (AC1).
//! - Untrusted author → ignored with the skip event, never enqueued,
//!   and never even fetches the current PR (AC2 security posture).
//! - Same-SHA re-review after completion appends a SECOND history row
//!   without migration (AC3).
//! - Repeated trigger lines in one tick → one enqueue (§3.5 per-tick
//!   dedup).
//! - Auto-discovery polling never triggers same-SHA re-review; the
//!   explicit path does (AC2).

use std::path::Path;
use std::process::Command;

use caduceus::config::{AutoReviewConfig, Config};
use caduceus::daemon::tick::review_discovery::poll_review_step_for_tests;
use caduceus::daemon::tick::review_rerun::{poll_rerun_step_for_tests, ReviewRerunStats};
use caduceus::github::{Client, HttpCache};
use caduceus::review::{
    ExecutionStatus, RepositoryId, Review, ReviewResult, ReviewTarget, Verdict,
    REVIEW_SCHEMA_VERSION,
};
use caduceus::state::review::{ReviewHistoryRow, ReviewPhase, ReviewStore};
use caduceus::worktree::GitRunner;
use chrono::{Duration, TimeZone, Utc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const OWNER: &str = "owner";
const REPO: &str = "r";
const PR: u64 = 7;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn repo_id() -> RepositoryId {
    RepositoryId {
        owner: OWNER.to_string(),
        repo: REPO.to_string(),
    }
}

fn ar_config() -> AutoReviewConfig {
    AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
    }
}

/// Config with the `auto_review` block enabled, the allowlist set, and
/// discovery pointed at the wiremock base (TrustedHost is fine:
/// `test_defaults` bypasses `from_raw`'s OCI-required check).
fn rerun_config(root: &Path, api_base: &str, allowlist: &[&str]) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.api_base = api_base.to_string();
    cfg.watched_repos = vec![format!("{OWNER}/{REPO}")];
    cfg.auto_review = Some(ar_config());
    cfg.feedback_author_allowlist = allowlist.iter().map(|s| s.to_string()).collect();
    cfg
}

/// Initialise a bare remote with `main` (commit A), a `feature`
/// branch (commit B, child of A), and commit C (child of B, the
/// branch tip). Returns `(A, B, C)` — A is the merge base of A and C.
fn init_bare_remote_with_feature(path: &Path) -> (String, String, String) {
    let run = |cmd: &mut Command| {
        let output = cmd.output().expect("spawn command");
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(Command::new("git").arg("init").arg("--bare").arg(path));
    run(Command::new("git")
        .current_dir(path)
        .args(["symbolic-ref", "HEAD", "refs/heads/main"]));
    let tree = {
        let output = Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit = |message: &str, parent: Option<&str>| -> String {
        let mut args = vec!["commit-tree".to_string(), tree.clone()];
        if let Some(p) = parent {
            args.push("-p".to_string());
            args.push(p.to_string());
        }
        args.push("-m".to_string());
        args.push(message.to_string());
        let output = Command::new("git")
            .current_dir(path)
            .args(&args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let commit_a = commit("base", None);
    run(Command::new("git")
        .current_dir(path)
        .args(["update-ref", "refs/heads/main", &commit_a]));
    let commit_b = commit("feature", Some(&commit_a));
    let commit_c = commit("feature tip", Some(&commit_b));
    run(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &commit_c,
    ]));
    (commit_a, commit_b, commit_c)
}

fn valid_result_json(status: ExecutionStatus) -> String {
    serde_json::to_string(&ReviewResult {
        schema_version: REVIEW_SCHEMA_VERSION,
        status,
        review: match status {
            ExecutionStatus::Success => Some(Review {
                verdict: Verdict::Pass,
                summary: "ok".to_string(),
                findings: vec![],
            }),
            ExecutionStatus::Failure => None,
        },
    })
    .unwrap()
}

fn history_row(run_id: &str, sha: &str, generation: u64) -> ReviewHistoryRow {
    ReviewHistoryRow {
        review_run_id: run_id.to_string(),
        repository: repo_id(),
        pull_request: PR,
        head_sha: sha.to_string(),
        review_generation: generation,
        completed_at: Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap(),
        result_json: valid_result_json(ExecutionStatus::Success),
    }
}

/// Listener-loop harness: wiremock serves the pulls list, the
/// single-PR fetch, and the comment list; the remote resolver points
/// at a local bare `origin` so the mirror bootstrap works hermetically.
struct StepHarness {
    cfg: Config,
    server: MockServer,
    store: ReviewStore,
    remote_dir: std::path::PathBuf,
    base_sha: String,
    tip_sha: String,
}

impl StepHarness {
    async fn start(allowlist: &[&str]) -> Self {
        let root = tempdir("rerun-step");
        let server = MockServer::start().await;
        let mut cfg = rerun_config(&root, &server.uri(), allowlist);
        cfg.repo_storage_root = root.join("repos");
        cfg.git_timeout_seconds = 30;

        let remote_dir = root.join("origin.git");
        let (base_sha, _mid_sha, tip_sha) = init_bare_remote_with_feature(&remote_dir);

        let store = ReviewStore::open(&root.join("state")).expect("review store opens");
        StepHarness {
            cfg,
            server,
            store,
            remote_dir,
            base_sha,
            tip_sha,
        }
    }

    fn client(&self) -> Client {
        let cache = HttpCache::open(&self.cfg.state_dir).expect("cache opens");
        Client::with_cache(&self.cfg, cache).expect("client builds")
    }

    fn remote_url(&self) -> String {
        format!("file://{}", self.remote_dir.display())
    }

    /// A wire row for PR 7 whose head SHA is fetchable from the local
    /// remote.
    fn pr_row(&self, head_sha: &str) -> serde_json::Value {
        serde_json::json!({
            "number": PR,
            "title": "rerun row",
            "state": "open",
            "draft": false,
            "base": {"ref": "main", "sha": self.base_sha, "repo": {"full_name": "owner/r"}},
            "head": {"ref": "feature-x", "sha": head_sha, "repo": {"full_name": "owner/r"}}
        })
    }

    async fn mount_prs(&self, rows: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rows))
            .mount(&self.server)
            .await;
    }

    async fn mount_pr_fetch(&self, row: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(row))
            .mount(&self.server)
            .await;
    }

    async fn mount_comments(&self, comments: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path("/repos/owner/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(comments))
            .mount(&self.server)
            .await;
    }

    fn comment(author: &str, body: &str) -> serde_json::Value {
        serde_json::json!({"id": 1, "user": {"login": author}, "body": body})
    }

    /// A comment row carrying `created_at` (RFC 3339) — required to
    /// exercise the consumed-trigger watermark (review feedback,
    /// #335).
    fn comment_at(author: &str, body: &str, created_at: &str) -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "user": {"login": author},
            "body": body,
            "created_at": created_at
        })
    }

    async fn run_rerun(&self) -> ReviewRerunStats {
        let remote_url = self.remote_url();
        poll_rerun_step_for_tests(
            &[format!("{OWNER}/{REPO}")],
            &self.client(),
            &self.cfg,
            &self.store,
            &GitRunner::new(&self.cfg),
            &move |_owner: &str, _repo: &str| Ok(remote_url.clone()),
        )
        .await
        .expect("rerun step returns Ok")
    }

    fn target(&self, head_sha: &str) -> ReviewTarget {
        ReviewTarget {
            repository: repo_id(),
            pull_request: PR,
            head_sha: head_sha.to_string(),
            base_sha: self.base_sha.clone(),
            base_ref: "main".to_string(),
            merge_base: self.base_sha.clone(),
        }
    }
}

async fn received_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|req| req.url.path().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// AC1 — trusted author enqueues an explicit re-review
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trusted_author_rerun_enqueues_explicit_review() {
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment("alice", "/caduceus review")])
        .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1, "explicit re-review enqueued");
    assert_eq!(stats.trigger_matched, 1);
    assert_eq!(stats.skipped_untrusted, 0);

    let q = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(q.entries.len(), 1);
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(entry.target.head_sha, h.tip_sha, "current head SHA (AC1)");
    assert_eq!(entry.review_generation, 1);
    assert!(entry.phase.is_active());
}

// ---------------------------------------------------------------------------
// AC2 — untrusted author ignored with event, polling never triggers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn untrusted_author_ignored_with_event() {
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_comments(vec![StepHarness::comment("mallory", "/caduceus review")])
        .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.skipped_untrusted, 1, "skip event counted");
    assert_eq!(stats.trigger_matched, 1);
    assert_eq!(stats.enqueued, 0, "never enqueued");
    assert_eq!(
        h.store
            .review_queue_snapshot()
            .expect("snapshot")
            .entries
            .len(),
        0
    );
    // Security posture: the untrusted path must not even fetch the
    // current PR — one trigger is enough to classify.
    let paths = received_paths(&h.server).await;
    assert!(
        !paths.iter().any(|p| p == "/repos/owner/r/pulls/7"),
        "untrusted path must not fetch the PR: {paths:?}"
    );
}

#[tokio::test]
async fn empty_allowlist_means_no_trusted_triggers() {
    // Fail-closed: an operator who has not populated the allowlist
    // gets skip events, never enqueues.
    let h = StepHarness::start(&[]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_comments(vec![StepHarness::comment("alice", "/caduceus review")])
        .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.skipped_untrusted, 1);
    assert_eq!(stats.enqueued, 0);
    assert_eq!(
        h.store
            .review_queue_snapshot()
            .expect("snapshot")
            .entries
            .len(),
        0
    );
}

#[tokio::test]
async fn polling_never_triggers_same_sha_rerun() {
    // Pre-seed a completed review of the tip SHA: Done entry + the
    // #377 completion-gated `last_reviewed_head_sha` pointer.
    let h = StepHarness::start(&["alice"]).await;
    let store = &h.store;
    assert!(matches!(
        store.enqueue_review(&h.target(&h.tip_sha)).unwrap(),
        caduceus::state::review::ReviewEnqueueOutcome::Inserted
    ));
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-1", 4242, now)
        .unwrap()
        .expect("claim");
    store
        .append_history(history_row("run-1", &h.tip_sha, 1))
        .unwrap();
    store.complete_review(claimed.claim).unwrap();
    let mut st = store.review_state(&repo_id(), PR).unwrap().unwrap();
    st.last_reviewed_head_sha = Some(h.tip_sha.clone());
    store.save_review_state(&st).unwrap();
    assert_eq!(
        store.review_queue_snapshot().unwrap().entries.len(),
        1,
        "pre-seeded Done entry"
    );

    // Wire the GitHub endpoints up front (discovery and the listener
    // share the same pulls list).
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment("alice", "/caduceus review")])
        .await;

    // Auto-discovery poll: the identical head SHA is already reviewed
    // → SkipAlreadyComplete, NO re-admission (AC2: polling never
    // triggers same-SHA re-review; the #377 completion-gated dedup is
    // intact).
    let remote_url = h.remote_url();
    let discovery_stats = poll_review_step_for_tests(
        &[format!("{OWNER}/{REPO}")],
        &h.client(),
        &h.cfg,
        store,
        &GitRunner::new(&h.cfg),
        &move |_owner: &str, _repo: &str| Ok(remote_url.clone()),
    )
    .await
    .expect("discovery step returns Ok");
    assert_eq!(discovery_stats.skipped_already_complete, 1);
    assert_eq!(discovery_stats.admitted, 0);
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(
        entry.phase,
        ReviewPhase::Done,
        "no new active entry from polling"
    );
    assert_eq!(
        entry.review_generation, 1,
        "generation untouched by polling"
    );

    // The explicit path, in contrast, re-admits the SAME SHA: the
    // listener bumps the generation and replaces the entry (AC1).
    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1, "explicit path re-admits the same SHA");
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(
        entry.review_generation, 2,
        "generation bumped by the explicit re-review"
    );
    assert!(entry.phase.is_active());
}

// ---------------------------------------------------------------------------
// AC3 — same-SHA re-review appends a second history row (no migration)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_sha_rerun_after_completion_adds_history_row() {
    let h = StepHarness::start(&["alice"]).await;
    let store = &h.store;
    // Run 1 (auto-discovery admission): complete with a history row.
    assert!(matches!(
        store.enqueue_review(&h.target(&h.tip_sha)).unwrap(),
        caduceus::state::review::ReviewEnqueueOutcome::Inserted
    ));
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed1 = store
        .acquire_next_review("run-1", 4242, now)
        .unwrap()
        .expect("claim run 1");
    store
        .append_history(history_row("run-1", &h.tip_sha, 1))
        .unwrap();
    store.complete_review(claimed1.claim).unwrap();
    assert_eq!(
        store
            .history_for_pull_request(&repo_id(), PR)
            .unwrap()
            .len(),
        1
    );

    // Explicit re-review of the SAME SHA (AC1).
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment("alice", "/caduceus review")])
        .await;
    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1);

    // Run 2 completes against the same SHA → a SECOND history row,
    // keyed by its own review_run_id — no migration, no SHA-uniqueness
    // constraint (DAR §4.3).
    let claimed2 = store
        .acquire_next_review("run-2", 4243, now)
        .unwrap()
        .expect("claim run 2");
    assert_eq!(claimed2.entry.review_generation, 2);
    store
        .append_history(history_row("run-2", &h.tip_sha, 2))
        .unwrap();
    store.complete_review(claimed2.claim).unwrap();

    let rows = store.history_for_pull_request(&repo_id(), PR).unwrap();
    assert_eq!(rows.len(), 2, "one row per run for the same SHA (AC3)");
    assert_eq!(rows[0].head_sha, h.tip_sha);
    assert_eq!(rows[0].review_run_id, "run-1");
    assert_eq!(rows[1].head_sha, h.tip_sha, "same SHA, second row");
    assert_eq!(rows[1].review_run_id, "run-2");
}

// ---------------------------------------------------------------------------
// Per-tick dedup (§3.5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_trigger_same_tick_enqueues_once() {
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    // Two trigger comments in the same tick (both trusted).
    h.mount_comments(vec![
        StepHarness::comment("alice", "/caduceus review"),
        StepHarness::comment("alice", "please /caduceus review"),
    ])
    .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1, "one tick = one enqueue per PR");
    assert_eq!(stats.trigger_matched, 1, "first matching line wins");
    let q = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(q.entries.len(), 1);
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(entry.review_generation, 1);
}

#[tokio::test]
async fn case_insensitive_trigger_matches() {
    // Operators type commands casually; the matcher folds case (DAR
    // §17 decision).
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment("alice", "/Caduceus Review")])
        .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1);
}

#[tokio::test]
async fn no_trigger_comment_means_no_enqueue() {
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_comments(vec![StepHarness::comment(
        "alice",
        "please /caduceus review this PR",
    )])
    .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.skipped_no_trigger, 1);
    assert_eq!(stats.enqueued, 0);
    assert_eq!(
        h.store
            .review_queue_snapshot()
            .expect("snapshot")
            .entries
            .len(),
        0
    );
}

// ---------------------------------------------------------------------------
// Persisted trigger dedup (review feedback, #335 — MUST FIX 1)
// ---------------------------------------------------------------------------

/// The past timestamp used for "already-consumed" trigger comments.
const TRIGGER_CREATED_AT_PAST: &str = "2026-09-05T12:00:00Z";

#[tokio::test]
async fn same_trigger_fires_exactly_once_across_ticks() {
    // The every-tick re-admission loop regression: one trusted trigger
    // comment must enqueue EXACTLY ONCE, not once per poll forever.
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment_at(
        "alice",
        "/caduceus review",
        TRIGGER_CREATED_AT_PAST,
    )])
    .await;

    // Tick 1: fresh trigger → enqueued.
    let stats1 = h.run_rerun().await;
    assert_eq!(stats1.enqueued, 1, "first tick enqueues the re-review");
    let q = h.store.review_queue_snapshot().expect("snapshot");
    assert_eq!(q.entries.len(), 1);
    assert_eq!(q.entries.values().next().unwrap().review_generation, 1);

    // Tick 2 (same comment still present): the entry's `queued_at` is
    // AFTER the comment's `created_at` → the trigger is consumed →
    // skipped, NOT re-enqueued. This is the exact MUST FIX 1 bug.
    let stats2 = h.run_rerun().await;
    assert_eq!(stats2.enqueued, 0, "second tick must not re-enqueue");
    assert_eq!(stats2.trigger_matched, 1, "comment still matches");
    assert_eq!(stats2.skipped_no_trigger, 1, "consumed trigger skipped");
    let q = h.store.review_queue_snapshot().expect("snapshot");
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(
        entry.review_generation, 1,
        "generation untouched by the consumed trigger"
    );
    assert!(
        entry.phase.is_active(),
        "the queued review is still waiting to run"
    );
}

#[tokio::test]
async fn new_trigger_comment_fires_after_old_consumed() {
    // A genuinely NEW comment must fire even while an older consumed
    // trigger comment is still present — the newest-first scan must
    // not let the stale comment shadow the fresh one.
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    h.mount_comments(vec![StepHarness::comment_at(
        "alice",
        "/caduceus review",
        TRIGGER_CREATED_AT_PAST,
    )])
    .await;

    let stats1 = h.run_rerun().await;
    assert_eq!(stats1.enqueued, 1);

    // A second trigger comment posted AFTER the first review was
    // queued (future-dated so it is unambiguously fresh relative to
    // `queued_at`). The old comment is still on the thread.
    h.server.reset().await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    let fresh = Utc::now() + Duration::seconds(3600);
    h.mount_comments(vec![
        StepHarness::comment_at("alice", "/caduceus review", TRIGGER_CREATED_AT_PAST),
        StepHarness::comment_at("alice", "/caduceus review", &fresh.to_rfc3339()),
    ])
    .await;

    let stats2 = h.run_rerun().await;
    assert_eq!(stats2.enqueued, 1, "new comment fires a new re-review");
    let q = h.store.review_queue_snapshot().expect("snapshot");
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(
        entry.review_generation, 2,
        "new trigger bumps the generation"
    );
}

#[tokio::test]
async fn consumed_trigger_does_not_shadow_untrusted_fresh_comment() {
    // Security posture: an untrusted FRESH comment is skipped with
    // the untrusted event and does not suppress an older TRUSTED
    // fresh trigger — but it can never enqueue anything.
    let h = StepHarness::start(&["alice"]).await;
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    // GitHub lists comments oldest-first; the newest (untrusted,
    // fresh) comment is scanned first and skipped with the untrusted
    // event, and must NOT suppress the older TRUSTED fresh trigger.
    let fresh = Utc::now() + Duration::seconds(3600);
    h.mount_comments(vec![
        StepHarness::comment_at("alice", "/caduceus review", TRIGGER_CREATED_AT_PAST),
        StepHarness::comment_at("mallory", "/caduceus review", &fresh.to_rfc3339()),
    ])
    .await;

    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 1, "trusted fresh trigger still fires");
    assert_eq!(stats.skipped_untrusted, 1, "untrusted comment emitted");
    assert_eq!(stats.trigger_matched, 1);
}

// ---------------------------------------------------------------------------
// Explicit re-enqueue while InProgress (review feedback, #335 — MUST
// FIX 2): no replacement, no claim orphan, next-tick enqueue after
// completion.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trigger_while_in_progress_skips_then_fires_after_completion() {
    let h = StepHarness::start(&["alice"]).await;
    let store = &h.store;

    // Pre-seed an auto-discovered review and claim it: the entry is
    // now InProgress with a live digest-keyed claim file.
    assert!(matches!(
        store.enqueue_review(&h.target(&h.tip_sha)).unwrap(),
        caduceus::state::review::ReviewEnqueueOutcome::Inserted
    ));
    let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
    let claimed = store
        .acquire_next_review("run-inprog-1", 4242, now)
        .unwrap()
        .expect("claim");
    assert_eq!(claimed.entry.phase, ReviewPhase::InProgress);

    // A fresh trusted trigger arrives while the review is running
    // (future-dated so it is unambiguously newer than `queued_at`).
    h.mount_prs(vec![h.pr_row(&h.tip_sha)]).await;
    h.mount_pr_fetch(h.pr_row(&h.tip_sha)).await;
    let fresh = Utc::now() + Duration::seconds(3600);
    h.mount_comments(vec![StepHarness::comment_at(
        "alice",
        "/caduceus review",
        &fresh.to_rfc3339(),
    )])
    .await;

    // Tick 1: the explicit enqueue is REFUSED (InProgress) — benign
    // skip with the dedicated event, NO replacement, NO claim orphan.
    let stats = h.run_rerun().await;
    assert_eq!(stats.enqueued, 0, "in-flight review is never replaced");
    assert_eq!(stats.skipped_in_progress, 1, "dedicated skip event");
    assert_eq!(stats.failed_prs, 0, "benign skip, not a PR failure");
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(entry.phase, ReviewPhase::InProgress, "entry not replaced");
    assert_eq!(entry.review_generation, 1, "generation not bumped");
    assert_eq!(
        entry.last_run_id.as_deref(),
        Some("run-inprog-1"),
        "running claim identity intact"
    );

    // The running claim still completes cleanly — no
    // `review-claim-terminal-mismatch` (the wedge would have made
    // this fail and stranded the claim file).
    store.complete_review(claimed.claim).unwrap();

    // Tick 2: after completion the same fresh trigger fires — the
    // requested re-review is NOT wedged.
    let stats2 = h.run_rerun().await;
    assert_eq!(stats2.enqueued, 1, "re-review runs after completion");
    let q = store.review_queue_snapshot().unwrap();
    let entry = q.entries.values().next().expect("entry");
    assert_eq!(
        entry.review_generation, 2,
        "generation bumped after completion"
    );
    assert!(entry.phase.is_active());
}
