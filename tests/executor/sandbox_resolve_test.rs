//! Resolution contract tests for `SandboxConfig + RuntimeFacts ->
//! SandboxSpec`.
//!
//! These tests pin the host-path allow-list, the overlap rejection,
//! the fixed identity, the canonical container paths, `pass_env`
//! filtering, and label order.

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_spec::{
    resolve, NetworkMode, ResolvedIdentity, RuntimeFacts, SandboxSpec, SANDBOX_GID, SANDBOX_UID,
};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, OciPullPolicy};
use caduceus::infra::error::CaduceusError;

/// Build a config and worktree/output paths under its `workdir_base`.
/// `resolve` does no I/O, so the paths never need to exist and the
/// tempdir can be dropped immediately.
fn base() -> (Config, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let output = cfg.workdir_base.join("owner").join("repo").join("result");
    (cfg, worktree, output)
}

/// Build runtime facts from a config's workdir_base.
fn runtime_for(cfg: &Config, worktree: &Path, output: &Path) -> RuntimeFacts {
    RuntimeFacts {
        run_id: "run-001".to_string(),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree: worktree.to_path_buf(),
        output_dir: output.to_path_buf(),
        daemon_id: "test-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
    }
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// (a) non-digest image → OciImageNotDigestPinned.
#[test]
fn rejects_non_digest_image() {
    let (mut cfg, worktree, output) = base();
    cfg.sandbox.as_mut().expect("sandbox").image = "caduceus-worker:latest".to_string();
    let runtime = runtime_for(&cfg, &worktree, &output);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciImageNotDigestPinned { reference } => {
            assert_eq!(reference, "caduceus-worker:latest");
        }
        other => panic!("expected OciImageNotDigestPinned; got: {other:?}"),
    }
}

/// (b) pull_policy Always + digest image → OciPullPolicyIncompatible.
#[test]
fn rejects_pull_policy_always_with_digest() {
    let (mut cfg, worktree, output) = base();
    cfg.sandbox.as_mut().expect("sandbox").pull_policy = OciPullPolicy::Always;
    let runtime = runtime_for(&cfg, &worktree, &output);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciPullPolicyIncompatible { .. } => {}
        other => panic!("expected OciPullPolicyIncompatible; got: {other:?}"),
    }
}

/// (c) worktree outside workdir_base → OciUndeclaredMount.
#[test]
fn rejects_worktree_outside_workdir_base() {
    let (cfg, _, _) = base();
    let runtime = runtime_for(
        &cfg,
        Path::new("/tmp/elsewhere/worktree"),
        Path::new("/tmp/elsewhere/result"),
    );
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciUndeclaredMount { path } => {
            assert!(
                path.contains("elsewhere"),
                "error should name the offending path, got: {path}"
            );
        }
        other => panic!("expected OciUndeclaredMount; got: {other:?}"),
    }
}

/// (c) output_dir outside workdir_base → OciUndeclaredMount.
#[test]
fn rejects_output_outside_workdir_base() {
    let (cfg, worktree, _) = base();
    let runtime = runtime_for(&cfg, &worktree, Path::new("/tmp/elsewhere/result"));
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

/// (c) relative paths are undeclared (cannot be anchored to
/// workdir_base).
#[test]
fn rejects_relative_worktree() {
    let (cfg, _, output) = base();
    let runtime = runtime_for(&cfg, Path::new("relative/worktree"), &output);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

/// (d) output_dir equal to worktree → OciMountConflict.
#[test]
fn rejects_output_equal_to_worktree() {
    let (cfg, worktree, _) = base();
    let runtime = runtime_for(&cfg, &worktree, &worktree);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciMountConflict { detail } => {
            assert!(
                detail.contains("double-RW"),
                "conflict detail should reference the double-RW bug, got: {detail}"
            );
        }
        other => panic!("expected OciMountConflict; got: {other:?}"),
    }
}

/// (d) output_dir containing worktree (parent of worktree) →
/// OciMountConflict.
#[test]
fn rejects_output_containing_worktree() {
    let (cfg, worktree, _) = base();
    let output = worktree.parent().expect("parent").to_path_buf();
    let runtime = runtime_for(&cfg, &worktree, &output);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict; got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Positive resolution contract
// ---------------------------------------------------------------------------

/// (e) fixed identity 1000:1000.
#[test]
fn resolves_fixed_identity() {
    let (cfg, worktree, output) = base();
    let runtime = runtime_for(&cfg, &worktree, &output);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(
        spec.identity(),
        ResolvedIdentity {
            uid: SANDBOX_UID,
            gid: SANDBOX_GID,
        }
    );
    assert_eq!(spec.identity().uid, 1000);
    assert_eq!(spec.identity().gid, 1000);
}

/// (f) canonical container paths `/workspace` and `/output`; exactly
/// one workspace mount and one output mount (double-RW bug fixed).
#[test]
fn resolves_canonical_container_paths() {
    let (cfg, worktree, output) = base();
    let runtime = runtime_for(&cfg, &worktree, &output);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");

    let ws = spec.workspace_mount();
    assert_eq!(ws.host_path, worktree);
    assert_eq!(ws.container_path, PathBuf::from("/workspace"));
    assert!(!ws.read_only, "workspace is RW");

    let out = spec.output_mount();
    assert_eq!(out.host_path, output);
    assert_eq!(out.container_path, PathBuf::from("/output"));
    assert!(!out.read_only, "output is RW");
}

/// (g) pass_env filtering — names present in the process environment
/// are appended after the two CADUCEUS_* entries, in config order;
/// unset names are skipped.
#[test]
fn pass_env_filtering() {
    struct EnvGuard(&'static str);
    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            EnvGuard(name)
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }
    let _guard = EnvGuard::set("CADUCEUS_RESOLVE_TEST_PASS_ENV", "present-value");

    let (mut cfg, worktree, output) = base();
    cfg.sandbox.as_mut().expect("sandbox").pass_env = vec![
        "CADUCEUS_RESOLVE_TEST_PASS_ENV".to_string(),
        "CADUCEUS_RESOLVE_TEST_UNSET".to_string(),
    ];
    let runtime = runtime_for(&cfg, &worktree, &output);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    let env = spec.environment();
    assert_eq!(
        env[0],
        ("CADUCEUS_RUN_ID".to_string(), "run-001".to_string())
    );
    assert_eq!(
        env[1],
        ("CADUCEUS_ISSUE_ID".to_string(), "owner/repo#1".to_string())
    );
    assert_eq!(
        env[2],
        (
            "CADUCEUS_RESOLVE_TEST_PASS_ENV".to_string(),
            "present-value".to_string()
        )
    );
    assert_eq!(
        env.len(),
        3,
        "unset pass_env names must be skipped, got: {env:?}"
    );
}

/// (h) labels in fixed order daemon_id, run_id, issue_id.
#[test]
fn resolves_labels_in_fixed_order() {
    let (cfg, worktree, output) = base();
    let mut runtime = runtime_for(&cfg, &worktree, &output);
    runtime.run_id = "run-007".to_string();
    runtime.issue = IssueKey::parse("owner/repo#7").expect("valid key");
    runtime.daemon_id = "daemon-42".to_string();
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(
        spec.labels(),
        &[
            ("caduceus.daemon_id".to_string(), "daemon-42".to_string()),
            ("caduceus.run_id".to_string(), "run-007".to_string()),
            ("caduceus.issue_id".to_string(), "owner/repo#7".to_string()),
        ]
    );
}

/// Resolution yields a fully populated spec: every mandatory field is
/// a total (non-`Option`) value.
#[test]
fn resolution_yields_fully_populated_spec() {
    let (cfg, worktree, output) = base();
    let runtime = runtime_for(&cfg, &worktree, &output);
    let spec: SandboxSpec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert!(!spec.name().is_empty());
    assert!(spec.image().contains("@sha256:"));
    assert!(!spec.command().is_empty());
    assert_eq!(spec.tmpfs().len(), 1);
    assert!(!spec.environment().is_empty());
    assert!(!spec.labels().is_empty());
    assert!(spec.resources().memory_mb > 0);
    assert!(matches!(
        spec.network(),
        NetworkMode::None | NetworkMode::Unrestricted
    ));
    let _ = spec.security();
}

/// The tmpfs size comes from `resources.tmpfs_mb` (was hard-coded
/// 64M before the renderer).
#[test]
fn tmpfs_size_from_resources() {
    let (mut cfg, worktree, output) = base();
    cfg.sandbox.as_mut().expect("sandbox").resources.tmpfs_mb = 512;
    let runtime = runtime_for(&cfg, &worktree, &output);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(spec.tmpfs()[0].target, "/tmp");
    assert_eq!(spec.tmpfs()[0].size_mb, 512);
}
