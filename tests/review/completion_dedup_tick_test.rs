//! Real-tick regression: completion-gated discovery dedup (gate #333,
//! finding 1).
//!
//! Drives the REAL `caduceus::tick::tick` loop TWICE (not the manual
//! step drivers) with the wiremock `/pulls` list returning the SAME
//! head SHA X on both ticks, a real `git://` origin, and a worker that
//! completes successfully on the first claim:
//!
//! * Tick 1: discovery admits X (gen 1); the drain runs the review to
//!   completion (Done + history row). Nothing is published yet — 5.6
//!   ran before the drain.
//! * Tick 2: discovery MUST report 0 admissions and
//!   `skipped_already_complete == 1`; the finalizer MUST publish tick
//!   1's result (exactly one sticky comment created).
//!
//! Against the pre-fix code this test is RED: the completed review is
//! re-admitted on tick 2 (generation bump + publication reset), the
//! completed row is suppressed by `due_finalizations`, the worker
//! re-runs the same SHA, and no sticky comment is ever created.
//!
//! Git plumbing: the tick derives every review remote from
//! `cfg.api_base` via `git_https_remote`, which drops the port
//! (`https://127.0.0.1/owner/r.git`). A per-test `GIT_CONFIG_GLOBAL`
//! rewrites that URL to a local `git://` daemon through git's
//! `url.<base>.insteadOf` mechanism; the GitRunner inherits the test
//! process environment (no `env_clear`), so every git subprocess the
//! tick spawns sees the rewrite.

use std::sync::Arc;

use caduceus::infra::logging::build_test_subscriber;
use caduceus::review::PublicationState;
use caduceus::state::review::ReviewPhase;

#[path = "../fixtures/mod.rs"]
mod fixtures;
#[path = "lifecycle_harness.rs"]
#[allow(dead_code)] // shared harness: only the subset this binary drives is used
mod harness;

use fixtures::GitDaemon;
use harness::{Backend, LifecycleHarness, STICKY_COMMENT_ID};

// ---------------------------------------------------------------------------
// Local helpers (the harness's `pr_wire_row` / drivers are private and
// manual-step-shaped; this test drives the REAL tick)
// ---------------------------------------------------------------------------

fn pr_wire_row(base_sha: &str, head_sha: &str) -> serde_json::Value {
    serde_json::json!({
        "number": harness::PR,
        "title": "re-admission loop regression PR",
        "body": "Same head SHA on both ticks.",
        "state": "open",
        "draft": false,
        "merged": false,
        "merged_at": null,
        "user": { "login": "octocat" },
        "base": { "ref": "main", "sha": base_sha, "repo": { "full_name": "owner/r" } },
        "head": { "ref": "feature-x", "sha": head_sha, "repo": { "full_name": "owner/r" } }
    })
}

fn run_git(bare: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(bare)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git spawn");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        bare.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A `git://` origin serving `owner/r.git` with main = A (the base) and
/// a `feature` ref at X (the head). Returns `(daemon, base_sha, head_sha)`.
fn setup_git_daemon(label: &str) -> (GitDaemon, String, String) {
    let daemon = GitDaemon::start(label, "owner", "r.git");
    let bare = daemon.path().to_path_buf();
    let base_sha = run_git(&bare, &["rev-parse", "refs/heads/main"]);
    let tree = run_git(&bare, &["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let head_sha = run_git(
        &bare,
        &["commit-tree", &tree, "-p", &base_sha, "-m", "feature"],
    );
    run_git(&bare, &["update-ref", "refs/heads/feature", &head_sha]);
    (daemon, base_sha, head_sha)
}

/// Wire the fake GitHub the real tick needs: issue poll, the `/pulls`
/// list (SAME head on both ticks), the single-PR fetch, the PR
/// discussion page, and the sticky-comment create.
async fn mount_github(h: &LifecycleHarness, base_sha: &str, head_sha: &str) {
    h.gh.mount("GET", "/repos/owner/r/issues", serde_json::json!([]))
        .await;
    h.gh.mount(
        "GET",
        "/repos/owner/r/pulls",
        serde_json::json!([pr_wire_row(base_sha, head_sha)]),
    )
    .await;
    h.gh.mount(
        "GET",
        &format!("/repos/owner/r/pulls/{}", harness::PR),
        pr_wire_row(base_sha, head_sha),
    )
    .await;
    h.gh.mount_paged(
        &format!("/repos/owner/r/issues/{}/comments", harness::PR),
        vec![serde_json::json!([])],
    )
    .await;
    h.gh.mount_status(
        "POST",
        &format!("/repos/owner/r/issues/{}/comments", harness::PR),
        201,
        serde_json::json!({ "id": STICKY_COMMENT_ID, "body": "" }),
    )
    .await;
}

/// Drive the REAL per-tick controller once (mirror of
/// `tests/tick/review_drain_test.rs::run_tick`, with the harness's
/// supervisor-backed executor). `poll_interval_seconds = 0` so the
/// persisted cadence gate never skips the second back-to-back tick.
async fn run_tick(
    h: &LifecycleHarness,
) -> caduceus::error::CaduceusResult<caduceus::meta::TickOutcome> {
    let mut cfg = h.cfg.clone();
    cfg.poll_interval_seconds = 0;
    let services = h.services.clone();
    let pool = Arc::clone(&services.pool);
    caduceus::tick::tick(
        cfg,
        services,
        pool,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
}

fn history_len(h: &LifecycleHarness) -> usize {
    h.store
        .history_for_pull_request(&h.repository(), harness::PR)
        .expect("history read")
        .len()
}

fn queue_entry(h: &LifecycleHarness, head_sha: &str) -> caduceus::state::review::ReviewQueueEntry {
    h.store
        .review_queue_snapshot()
        .expect("queue snapshot")
        .entries
        .values()
        .find(|e| e.target.head_sha == head_sha)
        .expect("queue entry for head sha")
        .clone()
}

fn state(h: &LifecycleHarness) -> caduceus::review::ReviewState {
    h.store
        .review_state(&h.repository(), harness::PR)
        .expect("state read")
        .expect("state row exists")
}

// ---------------------------------------------------------------------------
// The regression
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn completion_gates_discovery_dedup_in_real_tick() {
    for backend in [Backend::Json, Backend::Sqlite] {
        let h = LifecycleHarness::start("re-dedup", backend).await;

        // GIT_CONFIG_GLOBAL reaches every git subprocess the tick
        // spawns (the GitRunner inherits the test process environment
        // — no env_clear). The `[user]` section gives the fixture's
        // bare-repo seeding commands an ident (they run git without
        // GIT_AUTHOR_* overrides); the `[url ...] insteadOf` section is
        // appended after the daemon starts because it needs the port.
        let gitconfig = h._root.join("gitconfig");
        std::fs::write(&gitconfig, "[user]\n\tname = t\n\temail = t@example.com\n")
            .expect("write gitconfig (user)");
        std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig);

        // The tick derives every review git remote from `cfg.api_base`
        // via git_https_remote, which drops the port
        // (`https://127.0.0.1/owner/r.git`). Redirect that URL to the
        // local git:// daemon through git's `url.<base>.insteadOf`.
        let (daemon, base_sha, head_sha) = setup_git_daemon("rededup");
        std::fs::write(
            &gitconfig,
            format!(
                "[user]\n\tname = t\n\temail = t@example.com\n[url \"{}\"]\n\tinsteadOf = https://127.0.0.1/owner/r.git\n",
                daemon.uri()
            ),
        )
        .expect("write gitconfig (url rewrite)");

        mount_github(&h, &base_sha, &head_sha).await;

        // Tick 1: discovery admits X (gen 1); the drain runs the review
        // to completion (Done + history row). Publication is NOT due
        // yet — 5.6 ran before the drain.
        run_tick(&h).await.expect("tick 1 succeeds");
        assert_eq!(history_len(&h), 1, "tick 1 ran the worker exactly once");
        assert_eq!(queue_entry(&h, &head_sha).phase, ReviewPhase::Done);
        assert_eq!(
            queue_entry(&h, &head_sha).review_generation,
            1,
            "first generation"
        );
        assert_eq!(
            state(&h).publication_state,
            PublicationState::Pending,
            "5.6 ran before the drain; tick 2 publishes"
        );

        // Tick 2: SAME head SHA on /pulls. Capture the discovery stats
        // from the structured log (the tick logs, never returns them).
        let capture = h._root.join("tick2.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&capture)
            .expect("open capture file");
        let (writer, appender_guard) = tracing_appender::non_blocking(file);
        let subscriber = build_test_subscriber(writer);
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            run_tick(&h).await.expect("tick 2 succeeds");
        }
        drop(appender_guard);
        let body = std::fs::read_to_string(&capture).expect("read tick2 log");
        assert!(
            body.contains("review discovery complete")
                && body.contains("admitted: 0")
                && body.contains("skipped_already_complete: 1"),
            "tick 2 discovery must report 0 admissions and skipped_already_complete == 1;\n{body}"
        );

        // The finalizer MUST publish tick 1's result — exactly one
        // sticky comment created, for the run the drain completed.
        assert_eq!(h.gh.counts().post, 1, "exactly one sticky comment created");
        let s = state(&h);
        assert_eq!(
            s.publication_state,
            PublicationState::Published,
            "5.6 published on tick 2"
        );
        assert_eq!(s.sticky_comment_id, Some(STICKY_COMMENT_ID));
        assert_eq!(s.last_reviewed_head_sha.as_deref(), Some(head_sha.as_str()));

        // The worker was NEVER re-run (DAR §9.1) and the queue was
        // never re-admitted (no generation bump).
        assert_eq!(history_len(&h), 1, "no second history row");
        assert_eq!(queue_entry(&h, &head_sha).phase, ReviewPhase::Done);
        assert_eq!(
            queue_entry(&h, &head_sha).review_generation,
            1,
            "no re-admission generation bump"
        );

        drop(daemon);
    }
}
