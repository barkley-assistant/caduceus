//! End-to-end fork review lifecycle (issue #337, Phase 2, Task 7).
//!
//! Exercises the FULL fork path in the REAL pipeline: discovery
//! classifies the fork PR as `AdmitFork` (policy allow-list), the
//! fork URL is resolved via the GitHub REST `repos/{owner}/{repo}`
//! lookup, the quarantine clone is created from the trusted base URL,
//! the fork head SHA is fetched into it, the review worktree
//! materialises against the quarantine mirror, the real supervisor
//! runs the real `review_harness.py` worker (scripted PASS result),
//! the run reaches `Done`, and the quarantine clone is removed at
//! terminal status.
//!
//! No-stub harness (mirror of `tests/review/lifecycle_test.rs`):
//! wiremock GitHub + real local bare remotes (base AND fork) + real
//! store + the real `supervise()` production supervisor with the real
//! `caduceus` binary as `self_exe`. No Docker.
//!
//! The full `tick()` entry point cannot be used here: its
//! `resolve_remote` is hardwired to `git_https_remote(api_base)` which
//! derives `https://{host}/{owner}/{repo}.git` URLs that cannot reach
//! the local test remotes. Like the #322 lifecycle tests, this test
//! drives the production seams (`poll_review_step_for_tests` +
//! `run_review_claim_for_tests` + `poll_publication_step_for_tests`)
//! with the resolvers injected — the quarantine resolver seam is
//! identical to the one `tick()` wires.

use caduceus::config::{AutoReviewConfig, ForkPolicy};
use caduceus::infra::logging::build_test_subscriber;
use caduceus::meta::TickOutcome;
use caduceus::review::{PublicationState, Verdict};
use caduceus::state::review::{ReviewPhase, ReviewQueueEntry};

#[path = "../fixtures/mod.rs"]
mod fixtures;
#[path = "../review/lifecycle_harness.rs"]
mod harness;

use harness::{Backend, LifecycleHarness, STICKY_COMMENT_ID};

fn state(h: &LifecycleHarness) -> caduceus::review::ReviewState {
    h.store
        .review_state(&h.repository(), harness::PR)
        .expect("state read")
        .expect("state row exists")
}

fn queue_entry(h: &LifecycleHarness, head_sha: &str) -> ReviewQueueEntry {
    h.store
        .review_queue_snapshot()
        .expect("queue snapshot")
        .entries
        .values()
        .find(|e| e.target.head_sha == head_sha)
        .expect("queue entry for head sha")
        .clone()
}

fn assert_entry_phase(h: &LifecycleHarness, head_sha: &str, phase: ReviewPhase, attempts: u32) {
    let entry = queue_entry(h, head_sha);
    assert_eq!(entry.phase, phase, "phase for head {head_sha}");
    assert_eq!(entry.attempts, attempts, "attempts for head {head_sha}");
}

/// The quarantine clone path for the fork PR target:
/// `<state_dir>/fork-quarantine/owner/r/{PR}@{head_sha>`.
fn quarantine_dir(h: &LifecycleHarness, head_sha: &str) -> std::path::PathBuf {
    h.cfg
        .state_dir
        .join("fork-quarantine")
        .join("owner")
        .join("r")
        .join(format!("{}@{head_sha}", harness::PR))
}

/// Drive the complete fork lifecycle on the JSON backend and return
/// the finished harness so cleanup assertions can run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn fork_review_lifecycle_end_to_end() {
    let mut h = LifecycleHarness::start("fork-lifecycle", Backend::Json).await;
    let repo = h.repository();

    // Trust policy: the repo opts INTO fork PR review.
    h.cfg.auto_review = Some(AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: Some(ForkPolicy {
            allow_fork_prs: vec!["owner/r".to_string()],
        }),
    });

    let fork_url = h.fork_url();
    h.mount_pulls_fork(&h.tip_sha).await;
    h.mount_fork_remote_lookup(&fork_url).await;
    h.mount_pr_fetch_and_discussion().await;
    h.mount_comment_create(201, STICKY_COMMENT_ID).await;
    h.mount_comment_get(STICKY_COMMENT_ID).await;

    // Phase 1 — discovery classifies the fork PR as AdmitFork (the
    // policy allow-list replaces the Phase-1 gate skip) and admits it
    // through the quarantine path.
    let stats = h.drive_discovery_fork().await;
    assert_eq!(stats.discovered, 1, "fork PR discovered");
    assert_eq!(stats.admitted, 1, "fork PR admitted via quarantine path");
    assert_eq!(
        stats.skipped_fork, 0,
        "policy allow-list replaced the gate skip"
    );

    let entry = queue_entry(&h, &h.tip_sha);
    assert_eq!(entry.phase, ReviewPhase::Queued);
    assert_eq!(
        entry.target.merge_base, h.base_sha,
        "merge base computed INSIDE the quarantine clone (DAR §11.2)"
    );
    assert_eq!(entry.target.head_sha, h.tip_sha);

    // The quarantine clone exists at admission with its provenance
    // marker and hardened config.
    let quarantine = quarantine_dir(&h, &h.tip_sha);
    assert!(
        quarantine.join("HEAD").exists(),
        "quarantine clone materialised at admission"
    );
    assert!(
        quarantine.join("QUARANTINE_MARKER").exists(),
        "quarantine provenance marker written"
    );

    // Phase 2 — claim + real worker run → PASS → terminal Done: the
    // guard tears down BOTH the review worktree and the quarantine.
    // The tracing subscriber is installed around the run so the
    // canonical `fork_quarantine_removed` teardown event is captured.
    let capture = h.cfg.state_dir.join("fork-events.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open capture");
    let (writer, appender_guard) = tracing_appender::non_blocking(log_file);
    let subscriber = build_test_subscriber(writer);
    let outcome = {
        let _guard = tracing::subscriber::set_default(subscriber);
        h.drive_claim("run-1", "pass").await
    };
    drop(appender_guard);
    assert_eq!(outcome, TickOutcome::Processed, "PASS run processes");
    harness::assert_history(&h.store, &repo, &[("run-1", 1, &h.tip_sha, "pass")]);
    assert_entry_phase(&h, &h.tip_sha, ReviewPhase::Done, 0);

    let events = std::fs::read_to_string(&capture).expect("read capture");
    assert!(
        events.contains("fork_quarantine_removed"),
        "terminal teardown emits the canonical event; got:\n{events}"
    );

    assert!(
        !quarantine.exists(),
        "quarantine clone must be removed at terminal status"
    );
    let removed_markers =
        std::fs::read_dir(h.cfg.state_dir.join("fork-quarantine").join(".removed"))
            .map(|dir| dir.into_iter().filter_map(Result::ok).count())
            .unwrap_or(0);
    assert!(
        removed_markers >= 1,
        "a forensic .removed marker must exist for the torn-down quarantine"
    );

    let worktree_root = h
        .cfg
        .repo_storage_root
        .join("worktrees")
        .join("review")
        .join("owner")
        .join("r");
    let stale_worktrees = std::fs::read_dir(&worktree_root)
        .map(|dir| dir.into_iter().filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(
        stale_worktrees, 0,
        "no review worktrees may survive a Done run"
    );

    // Phase 3 — finalizer publishes the PASS sticky comment.
    let fstats = h.drive_finalize().await;
    assert_eq!(fstats.published, 1, "PASS publication succeeds");
    let s = state(&h);
    assert_eq!(s.publication_state, PublicationState::Published);
    assert_eq!(s.sticky_comment_id, Some(STICKY_COMMENT_ID));
    assert_eq!(s.last_verdict, Some(Verdict::Pass));
    assert_eq!(
        s.last_reviewed_head_sha.as_deref(),
        Some(h.tip_sha.as_str())
    );
    assert_eq!(h.gh.counts().post, 1, "one sticky comment created");
}
