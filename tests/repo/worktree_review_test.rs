//! Review worktree mode (issue #299).
//!
//! Acceptance coverage:
//! - AC1 — exact-SHA checkout (HEAD == head_sha, detached).
//! - AC2 — metadata round-trip; merge-base diff three-dot, incl. the
//!   base-moved fixture.
//! - AC3 — no refs/heads/* artefacts (show-ref before/after).
//! - AC4 — HeadShaUnavailable at the worktree boundary, pre-path.
//! - AC5 — remove leaves no refs/registrations; reaper reclaims
//!   stale entries, retains fresh/claimed ones, sweeps orphans,
//!   refuses symlinks.

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::repo::BareMirror;
use caduceus::review::ReviewTarget;
use caduceus::worktree::GitRunner;
use caduceus::RepoReviewWorktree;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::path::Path;
use std::process::Command;

fn run_command(cmd: &mut Command) {
    let output = cmd.output().expect("spawn command");
    if !output.status.success() {
        panic!(
            "command {:?} failed: status={:?}\nstdout={}\nstderr={}",
            cmd,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Run `git` in *dir* and return trimmed stdout, panicking on
/// failure.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// main = A; feature = B (child of A). Returns (A, B).
fn init_bare_remote_with_feature(path: &Path) -> (String, String) {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    let output = Command::new("git")
        .current_dir(path)
        .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
        .output()
        .expect("hash-object");
    let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-m", "initial"])
        .output()
        .expect("commit-tree");
    let commit_a = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &commit_a,
    ]));
    let output = Command::new("git")
        .current_dir(path)
        .args(["commit-tree", &tree, "-p", &commit_a, "-m", "feature"])
        .output()
        .expect("commit-tree feature");
    let commit_b = String::from_utf8_lossy(&output.stdout).trim().to_string();
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &commit_b,
    ]));
    (commit_a, commit_b)
}

/// Hash blob content into the object store (piped stdin).
fn hash_blob_stdin(git_dir: &Path, content: &str) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(git_dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git hash-object");
    child
        .stdin
        .take()
        .expect("hash-object stdin")
        .write_all(content.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait hash-object");
    assert!(
        out.status.success(),
        "git hash-object --stdin failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Build a single-file tree object; returns the tree OID.
fn tree_with_file(git_dir: &Path, name: &str, content: &str) -> String {
    use std::io::Write;
    let blob = hash_blob_stdin(git_dir, content);
    let entry = format!("100644 blob {blob}\t{name}\n");
    let mut child = Command::new("git")
        .current_dir(git_dir)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git mktree");
    child
        .stdin
        .take()
        .expect("mktree stdin")
        .write_all(entry.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait mktree");
    assert!(
        out.status.success(),
        "git mktree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// main = A → A' (base moved); feature = B (child of A).
/// Returns (A, A_prime, B).
///
/// B's tree touches `feature.txt`; A' touches `base-moved.txt`, so
/// endpoint-to-endpoint `git diff A' B` would list both files while
/// the merge-base form `git diff A B` lists only `feature.txt`.
fn init_bare_remote_base_moved(path: &Path) -> (String, String, String) {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));

    // A: empty tree on main.
    let empty_tree = git_out(path, &["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let a = git_out(path, &["commit-tree", &empty_tree, "-m", "initial"]);
    run_command(
        Command::new("git")
            .current_dir(path)
            .args(["update-ref", "refs/heads/main", &a]),
    );

    // B: child of A touching feature.txt.
    let feature_tree = tree_with_file(path, "feature.txt", "feature content\n");
    let b = git_out(
        path,
        &["commit-tree", &feature_tree, "-p", &a, "-m", "feature"],
    );
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &b,
    ]));

    // A': child of A touching base-moved.txt; main moves past B.
    let moved_tree = tree_with_file(path, "base-moved.txt", "moved content\n");
    let a_prime = git_out(
        path,
        &["commit-tree", &moved_tree, "-p", &a, "-m", "base moved"],
    );
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/main",
        &a_prime,
    ]));

    (a, a_prime, b)
}

/// Sorted `git show-ref` output for deterministic before/after
/// comparison (ref order across ls-refs walks is not guaranteed).
fn sorted_show_refs(git_dir: &Path) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(git_dir)
        .args(["show-ref"])
        .output()
        .expect("show-ref");
    assert!(
        output.status.success(),
        "show-ref failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut refs: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.to_string())
        .collect();
    refs.sort();
    refs
}

fn target_for(repo: &str, pr: u64, head: &str, base: &str, merge_base: &str) -> ReviewTarget {
    ReviewTarget {
        repository: caduceus::review::RepositoryId {
            owner: "rvowner".to_string(),
            repo: repo.to_string(),
        },
        pull_request: pr,
        head_sha: head.to_string(),
        base_sha: base.to_string(),
        base_ref: "main".to_string(),
        merge_base: merge_base.to_string(),
    }
}

fn review_config(root: &Path) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.repo_storage_root = root.join("repos");
    cfg.git_timeout_seconds = 30;
    cfg
}

/// Mirror for the standard rvowner/rvrepo fixture.
async fn ensure_mirror(
    runner: &GitRunner,
    cfg: &Config,
    remote_dir: &Path,
    repo: &str,
) -> BareMirror {
    BareMirror::ensure(
        runner,
        cfg,
        "rvowner",
        repo,
        &format!("file://{}", remote_dir.display()),
        "main",
    )
    .await
    .expect("ensure mirror")
}

// AC1: the worktree is materialised at exactly head_sha, detached,
// with no branch refs written (AC3).
#[tokio::test]
async fn review_worktree_checks_out_exact_head_sha() {
    let root = tempdir("rv-exact");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let refs_before = sorted_show_refs(&mirror.path);
    let target = target_for("rvrepo", 7, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-exact-1", &target)
        .await
        .expect("create_review");

    // AC1: HEAD is exactly head_sha.
    let head = git_out(&wt.path, &["rev-parse", "HEAD"]);
    assert_eq!(head, b);

    // Detached: abbrev-ref reports "HEAD" for a detached checkout.
    let abbrev = git_out(&wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(abbrev, "HEAD", "review worktree must be detached");

    // AC3: no refs/heads/* written anywhere.
    assert_eq!(
        sorted_show_refs(&mirror.path),
        refs_before,
        "create_review must not create branch refs"
    );
}

// AC2 (first half): every context field round-trips through the
// metadata sidecar, and the sidecar rejects unknown fields.
#[tokio::test]
async fn review_worktree_metadata_round_trip() {
    let root = tempdir("rv-meta");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 8, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-meta-1", &target)
        .await
        .expect("create_review");

    let meta = wt.load_metadata().expect("load sidecar");
    assert_eq!(meta.schema_version, 1);
    assert_eq!(meta.run_id, "run-meta-1");
    assert_eq!(meta.repository, target.repository);
    assert_eq!(meta.pull_request, target.pull_request);
    assert_eq!(meta.head_sha, target.head_sha);
    assert_eq!(meta.base_sha, target.base_sha);
    assert_eq!(meta.base_ref, target.base_ref);
    assert_eq!(meta.merge_base, target.merge_base);

    // deny_unknown_fields: an extra key must fail to parse. The
    // sidecar is pretty-printed, so inject the key after the first
    // `{` regardless of surrounding whitespace.
    let raw = std::fs::read_to_string(&wt.metadata_path).unwrap();
    let tampered = raw.replacen('{', "{\"extra\":1,", 1);
    assert!(
        serde_json::from_str::<caduceus::repo::ReviewWorktreeMetadata>(&tampered).is_err(),
        "sidecar must reject unknown fields"
    );
}

// AC2 (second half): the persisted merge_base anchors the three-dot
// diff even after the base branch moves.
#[tokio::test]
async fn review_worktree_base_moved_diff_uses_persisted_merge_base() {
    let root = tempdir("rv-basemoved");
    let remote_dir = root.join("remote.git");
    let (a, a_prime, b) = init_bare_remote_base_moved(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo2").await;

    // Admission persisted merge_base = A (computed when base was A);
    // base_ref main has since advanced to A'. The worktree must
    // materialise at B and the three-dot diff must be anchored at
    // the PERSISTED merge base.
    let target = target_for("rvrepo2", 9, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-moved-1", &target)
        .await
        .expect("create_review");
    assert_eq!(git_out(&wt.path, &["rev-parse", "HEAD"]), b);

    let meta = wt.load_metadata().expect("metadata");
    assert_eq!(meta.merge_base, a, "persisted merge base is A");

    // Sanity: git's own merge-base(B, A') == A in this fixture —
    // proves the fixture is the base-moved shape.
    let live_mb = git_out(&mirror.path, &["merge-base", &b, &a_prime]);
    assert_eq!(live_mb, a);

    // The three-dot diff (DAR §2.2): git diff <merge_base> <head>.
    // MUST list B's file; must NOT list A''s base-side file.
    let diff = git_out(
        &wt.path,
        &["diff", "--name-only", &meta.merge_base, &meta.head_sha],
    );
    assert!(diff.contains("feature.txt"));
    assert!(
        !diff.contains("base-moved.txt"),
        "endpoint-to-endpoint (..) semantics would include base-side \
         changes; the merge-base form must not"
    );
}

// AC4: an unavailable head SHA rejects at the worktree boundary
// before any path work happens.
#[tokio::test]
async fn review_worktree_unavailable_sha_rejected() {
    let root = tempdir("rv-unavailable");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    // Simulate force-push + GC between discovery and execution: the
    // feature branch is deleted and B (unreachable, non-ancestor of
    // main) is pruned from the remote.
    run_command(Command::new("git").current_dir(&remote_dir).args([
        "update-ref",
        "-d",
        "refs/heads/feature",
    ]));
    run_command(Command::new("git").current_dir(&remote_dir).args([
        "reflog",
        "expire",
        "--expire=now",
        "--all",
    ]));
    run_command(
        Command::new("git")
            .current_dir(&remote_dir)
            .args(["gc", "--prune=now"]),
    );

    let target = target_for("rvrepo", 10, &b, &a, &a);
    let err = RepoReviewWorktree::create_review(&runner, &mirror, "run-gone-1", &target)
        .await
        .expect_err("unavailable SHA must reject");

    assert!(
        matches!(&err, CaduceusError::HeadShaUnavailable { sha } if sha == &b),
        "got: {err:?}"
    );

    // The fetch happens before any path work: nothing on disk.
    let expected_path = cfg
        .repo_storage_root
        .join("worktrees")
        .join("review")
        .join("rvowner")
        .join("rvrepo")
        .join("run-gone-1");
    assert!(!expected_path.exists());
}

// D4: reusing the same run_id after a materialised worktree is
// refused with the existing WorktreeReuseAfterFailure variant.
#[tokio::test]
async fn review_worktree_reuse_refused() {
    let root = tempdir("rv-reuse");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 11, &b, &a, &a);
    let _wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-reuse-1", &target)
        .await
        .expect("first create_review");

    let err = RepoReviewWorktree::create_review(&runner, &mirror, "run-reuse-1", &target)
        .await
        .expect_err("second create_review must refuse reuse");
    assert!(
        matches!(&err, CaduceusError::WorktreeReuseAfterFailure { .. }),
        "got: {err:?}"
    );
}

// AC5 (first half): remove leaves no refs, no registrations, no
// directory — but the per-repo review parent stays (reaper territory).
#[tokio::test]
async fn review_worktree_remove_leaves_no_artefacts() {
    let root = tempdir("rv-remove");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 12, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-remove-1", &target)
        .await
        .expect("create_review");

    let refs_before = sorted_show_refs(&mirror.path);
    RepoReviewWorktree::remove(&runner, &wt)
        .await
        .expect("remove");
    assert!(!wt.path.exists(), "worktree directory must be gone");

    assert_eq!(
        sorted_show_refs(&mirror.path),
        refs_before,
        "remove must not change refs (none were ever created)"
    );

    let list = git_out(&mirror.path, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains(&wt.path.to_string_lossy().to_string()),
        "worktree registration must be gone"
    );

    // The per-repo review parent survives — sweeping it is the
    // reaper's job, not remove's.
    let parent = wt
        .path
        .parent()
        .expect("review dir has a parent")
        .to_path_buf();
    assert!(parent.is_dir(), "per-repo review dir must remain");
}

// ---------------------------------------------------------------------------
// Reaper (AC 5 second half): gc_review_worktrees
// ---------------------------------------------------------------------------

/// Backdate a directory's mtime so the review GC treats it as stale.
fn backdate_to_older_than(path: &Path, days: i64) {
    let target = chrono::Utc::now() - chrono::Duration::days(days);
    let ft =
        filetime::FileTime::from_unix_time(target.timestamp(), target.timestamp_subsec_nanos());
    filetime::set_file_mtime(path, ft).expect("set mtime");
}

fn review_gc_config(root: &Path) -> Config {
    let mut cfg = review_config(root);
    cfg.watched_repos = vec!["rvowner/rvrepo".to_string()];
    cfg
}

// The reaper removes a stale, unregistered review worktree and leaves
// the mirror's refs untouched.
#[tokio::test]
async fn review_gc_removes_stale_review_worktree() {
    let root = tempdir("rv-gc-stale");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_gc_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 20, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-stale-1", &target)
        .await
        .expect("create_review");
    let refs_before = sorted_show_refs(&mirror.path);
    backdate_to_older_than(&wt.path, 30);

    let removed = caduceus::repo::review_worktree::gc_review_worktrees(&cfg, &runner, 7, false)
        .await
        .expect("gc");
    assert_eq!(removed, 1, "one stale review worktree should be removed");
    assert!(!wt.path.exists(), "stale worktree path should be gone");

    let list = git_out(&mirror.path, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains(&wt.path.to_string_lossy().to_string()),
        "stale worktree registration should be gone"
    );
    assert_eq!(
        sorted_show_refs(&mirror.path),
        refs_before,
        "gc must not change refs (none were ever created)"
    );
}

// A fresh review worktree survives the sweep.
#[tokio::test]
async fn review_gc_retains_fresh_review_worktree() {
    let root = tempdir("rv-gc-fresh");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_gc_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 21, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-fresh-1", &target)
        .await
        .expect("create_review");

    let removed = caduceus::repo::review_worktree::gc_review_worktrees(&cfg, &runner, 7, false)
        .await
        .expect("gc");
    assert_eq!(removed, 0, "fresh review worktree must be retained");
    assert!(wt.path.exists());
}

// A stale worktree referenced by a live claim file is in use and
// survives the sweep.
#[tokio::test]
async fn review_gc_retains_claimed_review_worktree() {
    let root = tempdir("rv-gc-claimed");
    let remote_dir = root.join("remote.git");
    let (a, b) = init_bare_remote_with_feature(&remote_dir);
    let cfg = review_gc_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rvrepo").await;

    let target = target_for("rvrepo", 22, &b, &a, &a);
    let wt = RepoReviewWorktree::create_review(&runner, &mirror, "run-claimed-1", &target)
        .await
        .expect("create_review");
    backdate_to_older_than(&wt.path, 30);

    // A claim file whose parsed worktree_path matches protects the
    // worktree (the reaper reads all .claim files, path-keyed).
    let claims_dir = cfg.state_dir.join("claims");
    std::fs::create_dir_all(&claims_dir).expect("claims dir");
    let claim_path = claims_dir.join("review-claimed.claim");
    let body = serde_json::json!({
        "version": 1,
        "key": caduceus::issue::IssueKey {
            owner: "rvowner".to_string(),
            repo: "rvrepo".to_string(),
            number: 22,
        },
        "run_id": "run-claimed-1",
        "pid": 4_000_022_u32,
        "process_start_identity": "<boot>:0",
        "started_at": chrono::Utc::now(),
        "worktree_path": wt.path,
    });
    std::fs::write(&claim_path, serde_json::to_vec(&body).unwrap()).expect("write claim");

    let removed = caduceus::repo::review_worktree::gc_review_worktrees(&cfg, &runner, 7, false)
        .await
        .expect("gc");
    assert_eq!(removed, 0, "an in-use review worktree must not be removed");
    assert!(wt.path.exists());
}

// An unregistered orphan is swept; a symlinked orphan is refused.
#[tokio::test]
async fn review_gc_removes_orphan_without_registration() {
    let root = tempdir("rv-gc-orphan");
    let cfg = review_gc_config(&root);
    let runner = GitRunner::new(&cfg);

    // No mirror exists — the orphan sweep must still run (crash
    // leftovers with no registration and no mirror are swept).
    let review_repo_dir = cfg
        .repo_storage_root
        .join("worktrees")
        .join("review")
        .join("rvowner")
        .join("rvrepo");
    std::fs::create_dir_all(&review_repo_dir).expect("review dir");

    let orphan = review_repo_dir.join("orphan-run");
    std::fs::create_dir_all(&orphan).expect("orphan");
    backdate_to_older_than(&orphan, 30);

    let removed = caduceus::repo::review_worktree::gc_review_worktrees(&cfg, &runner, 7, false)
        .await
        .expect("gc");
    assert_eq!(removed, 1, "orphan should be removed");
    assert!(!orphan.exists(), "orphan should be gone");

    // A symlinked entry is refused and survives.
    let target = root.join("symlink-target");
    std::fs::create_dir_all(&target).expect("target");
    backdate_to_older_than(&target, 30);
    let link = review_repo_dir.join("evil-link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let removed2 = caduceus::repo::review_worktree::gc_review_worktrees(&cfg, &runner, 7, false)
        .await
        .expect("gc");
    assert_eq!(removed2, 0, "symlinks must not be removed");
    assert!(link.exists(), "symlink must remain on disk");
}
