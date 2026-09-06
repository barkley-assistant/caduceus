//! OCI review sandbox profile tests (issue #303, DAR §6.3-6.4): the
//! review-worktree host-path admission and the profile façade.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_spec::{resolve_with_env, NetworkMode, RuntimeFacts};
use caduceus::executor::{ExecutorSpec, IssueWorkTarget, WorkTarget};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;
use caduceus::repo::review_worktree::review_worktrees_root;
use caduceus::review::{RepositoryId, ReviewTarget};

/// Build a config plus its canonical review-worktree root. `resolve`
/// does no I/O, so the paths never need to exist.
fn cfg_with_review_root() -> (Config, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let review_root = review_worktrees_root(&cfg.repo_storage_root);
    (cfg, review_root)
}

fn pr_target() -> ReviewTarget {
    ReviewTarget {
        repository: RepositoryId {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        },
        pull_request: 9,
        head_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        base_sha: "cafebabecafebabecafebabecafebabecafebabe".to_string(),
        base_ref: "main".to_string(),
        merge_base: "abcdef01abcdef01abcdef01abcdef01abcdef01".to_string(),
    }
}

/// Build `RuntimeFacts` from *cfg* (mirroring the shared executor
/// fixture `tests/executor/support/mod.rs`), with explicit run id,
/// worktree, and review-worktree root.
fn facts(
    cfg: &Config,
    run_id: &str,
    worktree: PathBuf,
    review_root: Option<PathBuf>,
) -> RuntimeFacts {
    RuntimeFacts {
        run_id: run_id.to_string(),
        target: "owner/repo#pr/9".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree,
        output_dir: cfg.state_dir.join("oci-runs").join(run_id).join("output"),
        daemon_id: "test-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: 4242,
        worktree_gid: 4242,
        engine_mode: caduceus::executor::sandbox_spec::EngineMode::Rootful,
        git_shadow_kind: caduceus::executor::sandbox_spec::GitShadowKind::File,
        git_shadow_host: cfg
            .state_dir
            .join("oci-runs")
            .join(run_id)
            .join("git-shadow"),
        review_worktree_root: review_root,
    }
}

/// PR-target `ExecutorSpec` at `worktree`.
fn pr_spec(worktree: &Path, run_id: &str) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: WorkTarget::PullRequest(pr_target()),
        worktree: worktree.to_path_buf(),
        run_id: run_id.to_string(),
        context_json: "{\"context\":true}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

/// Issue-target `ExecutorSpec` at `worktree`.
fn issue_spec(worktree: &Path, run_id: &str) -> ExecutorSpec {
    let key = IssueKey::parse("owner/repo#9").expect("fixture key");
    ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: WorkTarget::Issue(IssueWorkTarget {
            key,
            title: "Fix the thing".to_string(),
            body: "Body text".to_string(),
            labels: vec!["bug".to_string()],
            branch_name: "caduceus/owner/repo#9".to_string(),
        }),
        worktree: worktree.to_path_buf(),
        run_id: run_id.to_string(),
        context_json: "{\"context\":true}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

fn env() -> BTreeMap<OsString, OsString> {
    BTreeMap::new()
}

// ---------------------------------------------------------------------------
// Review-worktree admission (D8a)
// ---------------------------------------------------------------------------

#[test]
fn review_worktrees_root_shape_matches_worktree_module() {
    let root = PathBuf::from("/srv/repos");
    assert_eq!(
        review_worktrees_root(&root),
        root.join("worktrees").join("review")
    );
}

#[test]
fn pr_target_worktree_under_review_root_is_admitted() {
    let (cfg, review_root) = cfg_with_review_root();
    let worktree = review_root.join("owner").join("repo").join("run-pr-1");
    let runtime = facts(&cfg, "run-pr-1", worktree.clone(), Some(review_root));
    let resolved = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &pr_spec(&worktree, "run-pr-1"),
        &env(),
    )
    .expect("review worktree must be admitted");
    assert_eq!(resolved.workspace_mount().host_path, worktree);
}

#[test]
fn pr_target_worktree_under_workdir_base_is_refused() {
    // PR runs may NOT use the issue root — the review-root admission is
    // exclusive to PR targets, and the issue root stays exclusive to
    // issue targets.
    let (cfg, _review_root) = cfg_with_review_root();
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-pr-2");
    // Even WITH a review root declared, a PR worktree outside it is
    // refused.
    let runtime = facts(
        &cfg,
        "run-pr-2",
        worktree.clone(),
        Some(review_worktrees_root(&cfg.repo_storage_root)),
    );
    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &pr_spec(&worktree, "run-pr-2"),
        &env(),
    )
    .expect_err("PR worktree outside the review root must be refused");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

#[test]
fn pr_target_without_review_root_is_refused() {
    let (cfg, _review_root) = cfg_with_review_root();
    let worktree = review_worktrees_root(&cfg.repo_storage_root)
        .join("owner")
        .join("repo")
        .join("run-pr-3");
    let runtime = facts(&cfg, "run-pr-3", worktree.clone(), None);
    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &pr_spec(&worktree, "run-pr-3"),
        &env(),
    )
    .expect_err("PR target without a review root must be refused");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

#[test]
fn issue_target_under_review_root_is_refused() {
    // Issue runs keep the workdir_base allow-list; the review root
    // does not become a general admission.
    let (cfg, review_root) = cfg_with_review_root();
    let worktree = review_root.join("owner").join("repo").join("run-1");
    let runtime = facts(&cfg, "run-1", worktree.clone(), Some(review_root));
    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &issue_spec(&worktree, "run-1"),
        &env(),
    )
    .expect_err("issue worktree outside workdir_base must be refused");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

#[test]
fn issue_target_under_workdir_base_still_admitted_unchanged() {
    // Byte-identical issue-path behaviour: the pre-existing
    // acceptance (workdir_base allow-list, no review root).
    let (cfg, _review_root) = cfg_with_review_root();
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-2");
    let runtime = facts(&cfg, "run-2", worktree.clone(), None);
    let resolved = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &issue_spec(&worktree, "run-2"),
        &env(),
    )
    .expect("issue worktree under workdir_base must be admitted");
    assert_eq!(resolved.workspace_mount().host_path, worktree);
}

// ---------------------------------------------------------------------------
// Review profile façade (D8b)
// ---------------------------------------------------------------------------

#[test]
fn review_profile_requires_pr_target() {
    let (cfg, review_root) = cfg_with_review_root();
    let worktree = cfg.workdir_base.join("owner").join("repo").join("r1");
    let runtime = facts(&cfg, "r1", worktree.clone(), Some(review_root));
    let err = caduceus::executor::sandbox_spec::resolve_review_sandbox_with_env(
        cfg.sandbox(),
        &runtime,
        &issue_spec(&worktree, "r1"),
        &env(),
    )
    .expect_err("issue target must be refused");
    assert!(format!("{err}").contains("PullRequest"), "{err}");
}

#[test]
fn review_profile_resolves_with_dar_6_4_posture() {
    let (cfg, review_root) = cfg_with_review_root();
    let worktree = review_root.join("owner").join("repo").join("r2");
    let runtime = facts(&cfg, "r2", worktree.clone(), Some(review_root));
    let resolved = caduceus::executor::sandbox_spec::resolve_review_sandbox_with_env(
        cfg.sandbox(),
        &runtime,
        &pr_spec(&worktree, "r2"),
        &env(),
    )
    .expect("review profile resolves");
    // DAR §6.4 posture assertions (already structural — documented
    // here): .git shadow present + read-only (File kind on review
    // worktrees).
    let shadow = resolved.git_shadow().expect("shadow present");
    assert!(shadow.read_only);
    assert_eq!(shadow.container_path, PathBuf::from("/workspace/.git"));
    // Workspace RW (structurally cannot be RO, DAR §6.4).
    assert!(!resolved.workspace_mount().read_only);
    // Result mount is the daemon-owned /output (DAR §6.2).
    assert_eq!(
        resolved.output_mount().container_path,
        PathBuf::from("/output")
    );
    // PR env contract carries the /output result path (DAR §6.2).
    let result_path = resolved
        .environment()
        .iter()
        .find(|(k, _)| k == "CADUCEUS_RESULT_PATH")
        .expect("result path env");
    assert_eq!(result_path.1, "/output/worker-result.json");
}

#[test]
fn review_profile_network_is_inherited_not_widened() {
    // With sandbox.network = None (the default) the resolved spec is
    // NetworkMode::None; with Unrestricted it is the engine bridge —
    // host networking is unrepresentable either way (D8c: the network
    // mode is inherited from sandbox.network, not forced).
    let (cfg, review_root) = cfg_with_review_root();
    let worktree = review_root.join("owner").join("repo").join("r3");
    let runtime = facts(&cfg, "r3", worktree.clone(), Some(review_root));
    let resolved = caduceus::executor::sandbox_spec::resolve_review_sandbox_with_env(
        cfg.sandbox(),
        &runtime,
        &pr_spec(&worktree, "r3"),
        &env(),
    )
    .expect("review profile resolves with default network");
    assert_eq!(resolved.network(), NetworkMode::None);

    let mut cfg_unrestricted = cfg.clone();
    cfg_unrestricted.sandbox.as_mut().expect("sandbox").network =
        caduceus::infra::config::SandboxNetwork::Unrestricted;
    let resolved_unrestricted = caduceus::executor::sandbox_spec::resolve_review_sandbox_with_env(
        cfg_unrestricted.sandbox(),
        &runtime,
        &pr_spec(&worktree, "r3"),
        &env(),
    )
    .expect("review profile resolves with unrestricted network");
    assert_eq!(resolved_unrestricted.network(), NetworkMode::Unrestricted);
}
