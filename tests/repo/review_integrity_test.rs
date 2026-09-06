//! Review integrity enforcement tests (issue #306, DAR §6.4, §10,
//! §10.1, §10.2, §15).
//!
//! Acceptance coverage:
//!
//! - AC1 — tracked-file modification detected with the repo's own git
//!   config: modification / deletion / staged variants plus the
//!   autocrlf=true and `.gitattributes eol=crlf` no-false-positive
//!   matrix (each with a positive control).
//! - AC2 — control-file integrity: pre/post SHA-256 digests detect
//!   `worker-prompt.md` modification and deletion, and sidecar
//!   tampering; the dirty check alone does NOT flag the untracked
//!   prompt (the §10.2 rationale).
//! - D3 — a git-status FAILURE is Infrastructure, never a violation.
//! - DAR §13 — `review_mutation_violation` event shape.
//! - Terminal routing — `finish_mutation_violation` persists
//!   `blocked_source` / `blocked_recovery_hint` (pointing at the
//!   archived worktree), never touches `attempts`, releases the
//!   claim, and emits the event; wrong-variant input is rejected.

use std::path::Path;
use std::process::{Command, Stdio};

use caduceus::config::Config;
use caduceus::error::CaduceusError;
use caduceus::repo::review_integrity::{
    capture_control_file_digests, check_tracked_files_clean, enforce_review_read_only,
    finish_mutation_violation, verify_control_file_digests, MUTATION_VIOLATION_EVENT,
    MUTATION_VIOLATION_SOURCE,
};
use caduceus::repo::BareMirror;
use caduceus::review::ReviewTarget;
use caduceus::state::review::{review_queue_key, ReviewPhase, ReviewStore};
use caduceus::worktree::GitRunner;
use chrono::Utc;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

// ---------------------------------------------------------------------------
// Git fixture helpers (per-file locals; the worktree_review_test pattern)
// ---------------------------------------------------------------------------

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

fn hash_blob_stdin(git_dir: &Path, content: &str) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(git_dir)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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

/// Build a tree object from (name, content) pairs; returns the tree OID.
fn tree_with_files(git_dir: &Path, files: &[(&str, &str)]) -> String {
    use std::io::Write;
    let mut entries = String::new();
    for (name, content) in files {
        let blob = hash_blob_stdin(git_dir, content);
        entries.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let mut child = Command::new("git")
        .current_dir(git_dir)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git mktree");
    child
        .stdin
        .take()
        .expect("mktree stdin")
        .write_all(entries.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait mktree");
    assert!(
        out.status.success(),
        "git mktree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Build `src/<name>` (a nested tracked file) and return the root tree OID.
fn tree_with_nested_file(git_dir: &Path, name: &str, content: &str) -> String {
    use std::io::Write;
    let blob = hash_blob_stdin(git_dir, content);
    let subtree = {
        let entry = format!("100644 blob {blob}\t{name}\n");
        let mut child = Command::new("git")
            .current_dir(git_dir)
            .args(["mktree"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn git mktree subtree");
        child
            .stdin
            .take()
            .expect("mktree stdin")
            .write_all(entry.as_bytes())
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait mktree");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let root_entry = format!("040000 tree {subtree}\tsrc\n");
    let mut child = Command::new("git")
        .current_dir(git_dir)
        .args(["mktree"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git mktree root");
    child
        .stdin
        .take()
        .expect("mktree stdin")
        .write_all(root_entry.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait mktree");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// main = A (empty tree); returns A.
fn init_bare_remote(path: &Path) -> String {
    run_command(Command::new("git").arg("init").arg("--bare").arg(path));
    run_command(Command::new("git").current_dir(path).args([
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]));
    let empty_tree = git_out(path, &["hash-object", "-w", "-t", "tree", "/dev/null"]);
    let a = git_out(path, &["commit-tree", &empty_tree, "-m", "initial"]);
    run_command(
        Command::new("git")
            .current_dir(path)
            .args(["update-ref", "refs/heads/main", &a]),
    );
    a
}

/// feature = B (child of `base`, tree `tree`); returns B.
fn create_feature_commit(path: &Path, base: &str, tree: &str) -> String {
    let b = git_out(path, &["commit-tree", tree, "-p", base, "-m", "feature"]);
    run_command(Command::new("git").current_dir(path).args([
        "update-ref",
        "refs/heads/feature",
        &b,
    ]));
    b
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

/// Standard fixture: mirror + review worktree at B whose tree carries
/// the tracked file `src/lib.rs`. Returns the worktree handle.
async fn review_worktree_with_lib_rs(
    tag: &str,
    repo: &str,
    pr: u64,
) -> (caduceus::repo::ReviewWorktree, GitRunner) {
    let root = tempdir(tag);
    let remote_dir = root.join("remote.git");
    let a = init_bare_remote(&remote_dir);
    let tree = tree_with_nested_file(&remote_dir, "lib.rs", "fn main() {}\n");
    let b = create_feature_commit(&remote_dir, &a, &tree);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, repo).await;
    let target = target_for(repo, pr, &b, &a, &a);
    let wt = caduceus::repo::ReviewWorktree::create_review(&runner, &mirror, "run-int-1", &target)
        .await
        .expect("create_review");
    (wt, runner)
}

/// Daemon control files present in the worktree (the sidecar was
/// written by create_review; the prompt is written the way #339 will
/// place it, before the worker runs).
fn write_prompt(worktree: &Path) {
    std::fs::write(worktree.join("worker-prompt.md"), "# prompt\n").expect("write prompt");
}

// ---------------------------------------------------------------------------
// Dirty check (DAR §10.1) — AC1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clean_worktree_passes_dirty_check() {
    let (wt, runner) = review_worktree_with_lib_rs("ri-clean", "rirepo", 21).await;
    // Three-way separation: untracked worker output (TrustedHost-shaped
    // result file) + build artefacts are NOT violations.
    std::fs::write(
        wt.path.join("worker-result.json"),
        "{\"status\":\"success\"}",
    )
    .expect("write result file");
    std::fs::create_dir_all(wt.path.join("target/debug")).expect("mkdir target");
    std::fs::write(wt.path.join("target/debug/foo"), "artefact").expect("write artefact");
    check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect("clean worktree must pass");
}

#[tokio::test]
async fn tracked_file_modification_is_a_violation() {
    let (wt, runner) = review_worktree_with_lib_rs("ri-mod", "rirepo", 22).await;
    let mut path = wt.path.join("src/lib.rs");
    let existing = std::fs::read_to_string(&path).unwrap();
    path = wt.path.join("src/lib.rs");
    std::fs::write(&path, format!("{existing}MUTATED\n")).expect("append byte");
    let err = check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect_err("modification must be flagged");
    assert!(
        matches!(err, CaduceusError::ReviewSourceMutation { .. }),
        "got: {err:?}"
    );
    // Porcelain evidence names the modified file.
    assert!(err.to_string().contains("src/lib.rs"), "got: {err}");
}

#[tokio::test]
async fn tracked_file_deletion_is_a_violation() {
    let (wt, runner) = review_worktree_with_lib_rs("ri-del", "rirepo", 23).await;
    std::fs::remove_file(wt.path.join("src/lib.rs")).expect("delete tracked file");
    let err = check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect_err("deletion must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
}

#[tokio::test]
async fn staged_modification_is_a_violation() {
    let (wt, runner) = review_worktree_with_lib_rs("ri-stage", "rirepo", 24).await;
    let existing = std::fs::read_to_string(wt.path.join("src/lib.rs")).unwrap();
    std::fs::write(wt.path.join("src/lib.rs"), format!("{existing}STAGED\n")).expect("append byte");
    run_command(
        Command::new("git")
            .current_dir(&wt.path)
            .args(["add", "src/lib.rs"]),
    );
    let err = check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect_err("staged change must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
    assert!(err.to_string().contains("src/lib.rs"));
}

/// D3: a dirty check that cannot run (not a git repository) is
/// Infrastructure, never a mutation violation — it detected nothing.
#[tokio::test]
async fn git_status_failure_is_infrastructure_not_violation() {
    let dir = tempdir("ri-not-git");
    let runner = GitRunner::new(&review_config(&dir));
    let err = check_tracked_files_clean(&runner, &dir)
        .await
        .expect_err("non-repo must fail");
    assert!(matches!(err, CaduceusError::Git { .. }), "got: {err:?}");
    let class = caduceus::orchestration::classify_error(&err);
    assert_eq!(class, caduceus::orchestration::FailureClass::Infrastructure);
    assert!(!class.is_terminal());
}

// ---------------------------------------------------------------------------
// Control-file integrity (DAR §10.2) — AC2
// ---------------------------------------------------------------------------

/// THE §10.2 rationale: worker-prompt.md is untracked, so the dirty
/// check alone must NOT flag it — but the digest check must.
#[tokio::test]
async fn prompt_modification_detected_by_digest_not_dirty_check() {
    let (wt, runner) = review_worktree_with_lib_rs("ri-prompt", "rirepo", 25).await;
    write_prompt(&wt.path);
    let pre = capture_control_file_digests(&wt.path).expect("pre digests");
    // The dirty check passes (prompt is untracked).
    check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect("untracked prompt is invisible to the dirty check");
    // The worker modifies it; the digest check flags it.
    let body = std::fs::read_to_string(wt.path.join("worker-prompt.md")).unwrap();
    std::fs::write(
        wt.path.join("worker-prompt.md"),
        format!("{body}tampered\n"),
    )
    .expect("append byte");
    let err = verify_control_file_digests(&wt.path, &pre)
        .expect_err("prompt modification must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
    assert!(err.to_string().contains("worker-prompt.md"), "got: {err}");
}

#[tokio::test]
async fn prompt_deletion_detected_by_digest() {
    let (wt, _runner) = review_worktree_with_lib_rs("ri-prompt-del", "rirepo", 26).await;
    write_prompt(&wt.path);
    let pre = capture_control_file_digests(&wt.path).expect("pre digests");
    std::fs::remove_file(wt.path.join("worker-prompt.md")).expect("delete prompt");
    let err =
        verify_control_file_digests(&wt.path, &pre).expect_err("prompt deletion must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
    assert!(err.to_string().contains("missing"), "got: {err}");
}

#[tokio::test]
async fn sidecar_modification_detected_by_digest() {
    let (wt, _runner) = review_worktree_with_lib_rs("ri-sidecar", "rirepo", 27).await;
    write_prompt(&wt.path);
    let pre = capture_control_file_digests(&wt.path).expect("pre digests");
    // A worker setting a future created_at would exempt the directory
    // from GC — exactly what the digest check must catch.
    let raw = std::fs::read_to_string(&wt.metadata_path).unwrap();
    let tampered = raw.replace("\"schema_version\": 1", "\"schema_version\": 1 ");
    std::fs::write(&wt.metadata_path, tampered).expect("tamper sidecar");
    let err =
        verify_control_file_digests(&wt.path, &pre).expect_err("sidecar tampering must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
    assert!(
        err.to_string().contains("review-worktree.json"),
        "got: {err}"
    );
}

#[test]
fn unchanged_control_files_pass_digest_check() {
    let dir = tempdir("ri-digest-ok");
    std::fs::write(dir.join("worker-prompt.md"), "# prompt\n").expect("write prompt");
    std::fs::write(dir.join("review-worktree.json"), "{}\n").expect("write sidecar");
    let pre = capture_control_file_digests(&dir).expect("pre digests");
    verify_control_file_digests(&dir, &pre).expect("unchanged digests must pass");
}

#[test]
fn pre_capture_on_missing_prompt_is_io_not_terminal() {
    let dir = tempdir("ri-pre-io");
    // No worker has run yet — a missing control file is a daemon-setup
    // bug (Io / Infrastructure), never a mutation violation.
    let err = capture_control_file_digests(&dir).expect_err("missing prompt");
    assert!(matches!(err, CaduceusError::Io(_)), "got: {err:?}");
    let class = caduceus::orchestration::classify_error(&err);
    assert_ne!(class, caduceus::orchestration::FailureClass::Terminal);
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enforce_review_read_only_composes_both_checks() {
    // 1. Modified tracked file + intact prompt → porcelain detail.
    let (wt, runner) = review_worktree_with_lib_rs("ri-comp-dirty", "rirepo", 28).await;
    write_prompt(&wt.path);
    let pre = capture_control_file_digests(&wt.path).expect("pre digests");
    let existing = std::fs::read_to_string(wt.path.join("src/lib.rs")).unwrap();
    std::fs::write(wt.path.join("src/lib.rs"), format!("{existing}X\n")).expect("modify");
    let err = enforce_review_read_only(&runner, &wt.path, &pre)
        .await
        .expect_err("dirty tree must be flagged");
    assert!(err.to_string().contains("tracked files modified"));
    let _ = std::fs::remove_dir_all(wt.path.ancestors().nth(4).unwrap());

    // 2. Clean tree + tampered sidecar → control-file detail.
    let (wt2, runner2) = review_worktree_with_lib_rs("ri-comp-sidecar", "rirepo2", 29).await;
    write_prompt(&wt2.path);
    let pre2 = capture_control_file_digests(&wt2.path).expect("pre digests");
    std::fs::write(&wt2.metadata_path, "{}\n").expect("tamper sidecar");
    let err2 = enforce_review_read_only(&runner2, &wt2.path, &pre2)
        .await
        .expect_err("tampered sidecar must be flagged");
    assert!(err2.to_string().contains("daemon control file modified"));
    let _ = std::fs::remove_dir_all(wt2.path.ancestors().nth(4).unwrap());

    // 3. Both clean → Ok.
    let (wt3, runner3) = review_worktree_with_lib_rs("ri-comp-clean", "rirepo3", 30).await;
    write_prompt(&wt3.path);
    let pre3 = capture_control_file_digests(&wt3.path).expect("pre digests");
    enforce_review_read_only(&runner3, &wt3.path, &pre3)
        .await
        .expect("both checks clean");
    let _ = std::fs::remove_dir_all(wt3.path.ancestors().nth(4).unwrap());
}

// ---------------------------------------------------------------------------
// Autocrlf / eol matrix (AC1 — the false-positive guards)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn autocrlf_true_checkout_is_not_a_false_positive() {
    // Repo commits LF files; the mirror config sets core.autocrlf=true
    // so the worktree checks out CRLF on disk. On-disk bytes differ
    // from the index — a naive byte-compare would flag it; git status
    // with the repository's own config must not.
    let root = tempdir("ri-crlf");
    let remote_dir = root.join("remote.git");
    let a = init_bare_remote(&remote_dir);
    let tree = tree_with_nested_file(&remote_dir, "lib.rs", "fn main() {}\nsecond\n");
    let b = create_feature_commit(&remote_dir, &a, &tree);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rirepo-crlf").await;
    // Set the filter BEFORE the checkout so the smudge applies.
    run_command(Command::new("git").current_dir(&mirror.path).args([
        "config",
        "core.autocrlf",
        "true",
    ]));
    let target = target_for("rirepo-crlf", 31, &b, &a, &a);
    let wt = caduceus::repo::ReviewWorktree::create_review(&runner, &mirror, "run-crlf-1", &target)
        .await
        .expect("create_review");

    // Precondition: the checkout really is CRLF on disk.
    let on_disk = std::fs::read(wt.path.join("src/lib.rs")).expect("read lib.rs");
    let as_text = String::from_utf8_lossy(&on_disk).to_string();
    assert!(
        as_text.contains("\r\n"),
        "fixture broken: checkout must be CRLF, got {as_text:?}"
    );

    // Filter-aware status must be clean for the untouched checkout.
    check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect("filter-aware status must not flag a normalized checkout");

    // Positive control: a real modification is still a violation.
    std::fs::write(wt.path.join("src/lib.rs"), format!("{as_text}MUTATED\r\n"))
        .expect("append line");
    let err = check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect_err("real modification must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
}

#[tokio::test]
async fn gitattributes_eol_crlf_is_not_a_false_positive() {
    // The tree carries `.gitattributes` with `* text eol=crlf`; the
    // checkout applies the attribute on materialisation. Untouched
    // worktree → clean; modified file → violation.
    let root = tempdir("ri-attrs");
    let remote_dir = root.join("remote.git");
    let a = init_bare_remote(&remote_dir);
    let tree = tree_with_files(
        &remote_dir,
        &[
            (".gitattributes", "* text eol=crlf\n"),
            ("feature.txt", "feature content\n"),
        ],
    );
    let b = create_feature_commit(&remote_dir, &a, &tree);
    let cfg = review_config(&root);
    let runner = GitRunner::new(&cfg);
    let mirror = ensure_mirror(&runner, &cfg, &remote_dir, "rirepo-attrs").await;
    let target = target_for("rirepo-attrs", 32, &b, &a, &a);
    let wt =
        caduceus::repo::ReviewWorktree::create_review(&runner, &mirror, "run-attrs-1", &target)
            .await
            .expect("create_review");

    // Precondition: the attribute smudged the file to CRLF.
    let on_disk =
        String::from_utf8_lossy(&std::fs::read(wt.path.join("feature.txt")).unwrap()).to_string();
    assert!(
        on_disk.contains("\r\n"),
        "fixture broken: eol=crlf must apply, got {on_disk:?}"
    );

    check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect("attribute-normalized checkout must be clean");

    std::fs::write(wt.path.join("feature.txt"), format!("{on_disk}MUTATED\r\n"))
        .expect("append line");
    let err = check_tracked_files_clean(&runner, &wt.path)
        .await
        .expect_err("real modification must be flagged");
    assert!(matches!(err, CaduceusError::ReviewSourceMutation { .. }));
    assert!(err.to_string().contains("feature.txt"));
}

// ---------------------------------------------------------------------------
// DAR §13 event (issue-#167 capture discipline)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn mutation_violation_event_is_emitted_with_dar13_shape() {
    use caduceus::logging::build_test_subscriber;

    let root = tempfile::tempdir().expect("tempdir");
    let log_path = root.path().join("mutation.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let target = target_for(
        "evrepo",
        42,
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "a",
        "a",
    );
    let wt_path = root.path().join("wt");
    std::fs::create_dir_all(&wt_path).expect("mkdir wt");
    tracing::subscriber::with_default(subscriber, || {
        caduceus::repo::review_integrity::emit_review_mutation_violation(
            &target,
            &wt_path,
            "tracked files modified: M src/lib.rs",
        );
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read log");
    assert!(
        body.contains(&format!("\"event\":\"{MUTATION_VIOLATION_EVENT}\"")),
        "{body}"
    );
    assert!(body.contains("\"repo\":\"rvowner/evrepo\""), "{body}");
    assert!(body.contains("\"pr\":42"), "{body}");
    assert!(
        body.contains("\"head_sha\":\"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\""),
        "{body}"
    );
    assert!(body.contains("tracked files modified"), "{body}");
}

// ---------------------------------------------------------------------------
// Composed terminal route (Task 4)
// ---------------------------------------------------------------------------

#[test]
fn finish_mutation_violation_routes_needs_attention_with_hint() {
    let dir = tempdir("ri-finish");
    let store = ReviewStore::open(&dir).expect("open store");
    let target = target_for(
        "routerrepo",
        33,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "b",
        "b",
    );
    store.enqueue_review(&target).expect("enqueue");
    let claimed = store
        .acquire_next_review("run-mv-1", 4242, Utc::now())
        .expect("claim")
        .expect("claim");

    let wt_path = dir.join("archived-wt");
    std::fs::create_dir_all(&wt_path).expect("mkdir archived wt");
    let err = CaduceusError::ReviewSourceMutation {
        worktree_path: wt_path.clone(),
        detail: "tracked files modified:\n M src/lib.rs".to_string(),
    };
    finish_mutation_violation(&store, claimed.claim, &target, &err).expect("finish");

    let snap = store.review_queue_snapshot().expect("snapshot");
    let entry = snap.entries.get(&review_queue_key(&target)).expect("entry");
    assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
    assert_eq!(
        entry.blocked_source.as_deref(),
        Some(MUTATION_VIOLATION_SOURCE)
    );
    // Hint points at the archived worktree (DAR §8.1).
    let hint = entry.blocked_recovery_hint.as_deref().expect("hint");
    assert!(
        hint.contains(wt_path.display().to_string().as_str()),
        "got: {hint}"
    );
    assert!(hint.contains("preserved for inspection"), "got: {hint}");
    // AC4: retry budget not burned.
    assert_eq!(entry.attempts, 0);
    assert!(entry
        .last_error
        .as_deref()
        .unwrap()
        .contains("review mutation violation"));
    // Claim released: nothing re-acquirable.
    assert!(store
        .acquire_next_review("run-mv-2", 4243, Utc::now())
        .expect("acquire")
        .is_none());
}

#[test]
fn finish_mutation_violation_rejects_non_mutation_variants() {
    let dir = tempdir("ri-finish-wrong");
    let store = ReviewStore::open(&dir).expect("open store");
    let target = target_for(
        "wrongrepo",
        34,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "b",
        "b",
    );
    store.enqueue_review(&target).expect("enqueue");
    let claimed = store
        .acquire_next_review("run-mv-3", 4242, Utc::now())
        .expect("claim")
        .expect("claim");
    let other = CaduceusError::Git {
        operation: "x",
        stderr: "y".to_string(),
    };
    let err = finish_mutation_violation(&store, claimed.claim, &target, &other)
        .expect_err("wrong variant rejected");
    assert!(matches!(err, CaduceusError::Config(_)), "got: {err:?}");
}

#[test]
#[serial_test::serial]
fn finish_mutation_violation_emits_event_and_persists_hint() {
    use caduceus::logging::build_test_subscriber;

    // AC3 pairing: the structured event AND the persisted hint land
    // from one call. Serial + non-blocking appender + temp file +
    // drop(guard) — the callsite-interest caching discipline.
    let root = tempdir("ri-finish-event");
    let log_path = root.join("finish.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open capture file");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);

    let store = ReviewStore::open(&root).expect("open store");
    let target = target_for(
        "evrouter",
        35,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "b",
        "b",
    );
    store.enqueue_review(&target).expect("enqueue");
    let claimed = store
        .acquire_next_review("run-mv-4", 4242, Utc::now())
        .expect("claim")
        .expect("claim");
    let wt_path = root.join("evidence-wt");
    std::fs::create_dir_all(&wt_path).expect("mkdir");
    let err = CaduceusError::ReviewSourceMutation {
        worktree_path: wt_path.clone(),
        detail: "daemon control file modified: worker-prompt.md".to_string(),
    };

    tracing::subscriber::with_default(subscriber, || {
        finish_mutation_violation(&store, claimed.claim, &target, &err)
            .expect("finish inside subscriber");
    });
    drop(guard);

    let body = std::fs::read_to_string(&log_path).expect("read log");
    assert!(
        body.contains(&format!("\"event\":\"{MUTATION_VIOLATION_EVENT}\"")),
        "{body}"
    );
    assert!(body.contains("\"repo\":\"rvowner/evrouter\""), "{body}");
    let snapshot = store.review_queue_snapshot().expect("snapshot");
    let entry = snapshot
        .entries
        .get(&review_queue_key(&target))
        .expect("entry");
    assert_eq!(entry.phase, ReviewPhase::NeedsAttention);
    assert!(entry.blocked_recovery_hint.is_some());
}
