//! Fork-quarantine orphan sweep tests (issue #337, Phase 2, Task 4).
//!
//! Proves the crash-recovery backstop: a per-PR quarantine clone
//! whose review queue key is no longer in any queued or in-progress
//! review entry is removed with a forensic removal marker under
//! `<state_dir>/fork-quarantine/.removed/`; a clone whose key IS
//! active is left alone; and the daemon TICK wires the sweep (the
//! plan's step 3 — "call the sweep at the end of the review step,
//! alongside the existing worktree-GC step").

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{AutoReviewConfig, Config, LoadContext, PublicationMode, RawConfig};
use caduceus::github::{Client, HttpCache};
use caduceus::orchestration::SystemClock;
use caduceus::repo::fork_quarantine::{quarantine_queue_key, ForkQuarantine, REMOVED_DIRNAME};
use caduceus::scheduler::{DrainConfig, Pool};
use caduceus::state::review::ReviewStore;
use caduceus::worktree::GitRunner;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

const OWNER: &str = "owner";
const REPO: &str = "r";

/// Removal-marker file extension written by
/// `ForkQuarantine::write_removal_marker`.
const REMOVAL_MARKER_EXTENSION: &str = ".removed";

fn git(args: &[&str], cwd: &Path) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Bare remote with one root commit on `main` (fork_quarantine_test.rs
/// shape); returns `(remote_dir, root_sha)`.
fn base_remote(root: &Path) -> (PathBuf, String) {
    let dir = root.join("base.git");
    std::fs::create_dir_all(&dir).expect("create bare dir");
    git(&["init", "--bare"], &dir);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"], &dir);
    let tree = git(&["hash-object", "-w", "-t", "tree", "/dev/null"], &dir);
    let commit = {
        let output = Command::new("git")
            .current_dir(&dir)
            .args(["commit-tree", &tree, "-m", "root"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("commit-tree");
        assert!(
            output.status.success(),
            "commit-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["update-ref", "refs/heads/main", &commit], &dir);
    (dir, commit)
}

fn ar_config() -> AutoReviewConfig {
    AutoReviewConfig {
        enabled: true,
        draft_pull_requests: false,
        rerun_command: "/caduceus review".to_string(),
        fork_policy: None,
        publication_mode: PublicationMode::Update,
    }
}

fn tick_cfg(base: &Path, api_base: &str) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(base.join("state")),
        workdir_base: Some(base.to_path_buf()),
        watched_repos: Some(vec!["owner/r".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(base.to_path_buf()),
        ..Default::default()
    };
    let mut cfg = Config::from_raw(raw, &ctx).expect("config");
    cfg.api_base = api_base.to_string();
    cfg.auto_review = Some(ar_config());
    cfg.repo_storage_root = base.join("repos");
    cfg.git_timeout_seconds = 30;
    cfg
}

async fn run_tick(cfg: Config, server: &MockServer) -> caduceus::error::CaduceusResult<()> {
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/r/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
    let mut cfg = cfg;
    cfg.api_base = server.uri();
    let cache = HttpCache::open(&cfg.state_dir).expect("cache opens");
    let client = Client::with_cache(&cfg, cache).expect("client builds");
    let clock: Arc<dyn caduceus::orchestration::Clock> = Arc::new(SystemClock);
    let git = GitRunner::new(&cfg);
    let pool = Arc::new(
        Pool::new(
            cfg.worker_parallelism,
            DrainConfig::from_seconds_and_ms(cfg.drain_timeout_seconds, cfg.backpressure_budget_ms),
        )
        .with_lease_store_dir(
            cfg.state_dir.clone(),
            std::time::Duration::from_secs(cfg.worker_lease_ttl_seconds),
        ),
    );
    let services = caduceus::orchestration::Services::production(
        &cfg,
        clock,
        Arc::new(client),
        git,
        Arc::clone(&pool),
        Arc::new(caduceus::infra::disk::DiskPressureGuard::disabled()),
    );
    let _outcome = caduceus::tick::tick(cfg, services, pool, CancellationToken::new()).await?;
    Ok(())
}

/// The removal-marker file name for one quarantine clone.
fn removal_marker_path(state_dir: &Path, key: &str) -> PathBuf {
    // `write_removal_marker` names files `<timestamp>-<leaf>.removed`
    // under `<state_dir>/fork-quarantine/.removed/`.
    let removed_dir = state_dir.join("fork-quarantine").join(REMOVED_DIRNAME);
    let mut best: Option<PathBuf> = None;
    if let Ok(entries) = std::fs::read_dir(&removed_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(REMOVAL_MARKER_EXTENSION) {
                let leaf = key.split('#').next_back().unwrap_or("");
                if name.contains(leaf) {
                    best = Some(entry.path());
                }
            }
        }
    }
    best.expect("a removal marker exists")
}

// ---------------------------------------------------------------------------
// Primitive sweep behaviour
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sweep_removes_orphan_and_writes_removal_marker() {
    let root = tempdir("sweep-orphan");
    let (remote, base_sha) = base_remote(&root);
    let remote_url = format!("file://{}", remote.display());
    let runner = GitRunner::new(&Config::test_defaults(&root));
    let state_dir = root.join("state");

    let quarantine = ForkQuarantine::create(
        &runner,
        &state_dir,
        OWNER,
        REPO,
        7,
        "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222",
        &base_sha,
        &remote_url,
        "forkuser/r",
    )
    .await
    .expect("quarantine creates");
    assert!(quarantine.path.exists(), "clone exists before sweep");

    let key = quarantine_queue_key(
        OWNER,
        REPO,
        &format!("{}@{}", 7, "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222"),
    );

    // The key is NOT active → swept.
    let removed = ForkQuarantine::sweep(&state_dir, &runner, &[])
        .await
        .expect("sweep");
    assert_eq!(removed, 1, "the orphan clone is removed");
    assert!(
        !quarantine.path.exists(),
        "clone directory is gone after sweep"
    );
    // Forensic marker under `.removed/`.
    let marker = removal_marker_path(&state_dir, &key);
    let body = std::fs::read_to_string(&marker).expect("marker readable");
    assert!(
        body.contains("forkuser/r") && body.contains(&base_sha),
        "marker records provenance, got: {body}"
    );
}

#[tokio::test]
async fn sweep_keeps_active_quarantine_untouched() {
    let root = tempdir("sweep-active");
    let (remote, base_sha) = base_remote(&root);
    let remote_url = format!("file://{}", remote.display());
    let runner = GitRunner::new(&Config::test_defaults(&root));
    let state_dir = root.join("state");

    let quarantine = ForkQuarantine::create(
        &runner,
        &state_dir,
        OWNER,
        REPO,
        7,
        "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222",
        &base_sha,
        &remote_url,
        "forkuser/r",
    )
    .await
    .expect("quarantine creates");

    let key = quarantine_queue_key(
        OWNER,
        REPO,
        &format!("{}@{}", 7, "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222"),
    );
    let removed = ForkQuarantine::sweep(&state_dir, &runner, &[key])
        .await
        .expect("sweep");
    assert_eq!(removed, 0, "an active key is never swept");
    assert!(
        quarantine.path.exists(),
        "clone survives when its review entry is queued/in-progress"
    );
}

// ---------------------------------------------------------------------------
// Tick wiring (the plan's step 3): the daemon tick calls the sweep at
// the end of the review step, so a crashed run's leftover clone is
// reclaimed even though no guard ever attached to it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_sweeps_orphaned_quarantine_clones() {
    let root = tempdir("tick-sweep");
    let server = MockServer::start().await;
    let cfg = tick_cfg(&root, &server.uri());

    // Seed an orphan quarantine clone: created at "admission", then
    // the daemon crashed before any review entry was enqueued.
    let (remote, base_sha) = base_remote(&root);
    let remote_url = format!("file://{}", remote.display());
    let runner = GitRunner::new(&cfg);
    let state_dir = cfg.state_dir.clone();
    let quarantine = ForkQuarantine::create(
        &runner,
        &state_dir,
        OWNER,
        REPO,
        7,
        "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222",
        &base_sha,
        &remote_url,
        "forkuser/r",
    )
    .await
    .expect("quarantine creates");
    assert!(quarantine.path.exists(), "orphan clone seeded");

    // Sanity: no review entry exists, so the key is not active.
    let store = ReviewStore::open(&state_dir).expect("review store opens");
    let snapshot = store.review_queue_snapshot().expect("snapshot");
    assert!(snapshot.entries.is_empty(), "no active review entries");

    run_tick(cfg, &server).await.expect("tick succeeds");

    assert!(
        !quarantine.path.exists(),
        "the tick's quarantine sweep reclaims the orphan clone"
    );
    // The sweep ran as part of the review step.
    assert!(
        state_dir
            .join("fork-quarantine")
            .join(REMOVED_DIRNAME)
            .exists(),
        "removal markers are written by the tick-driven sweep"
    );
}
