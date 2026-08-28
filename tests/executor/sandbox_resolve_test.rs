//! Resolution contract tests for `SandboxConfig + RuntimeFacts ->
//! SandboxSpec`.
//!
//! These tests pin the per-root host-path allow-list, the
//! cross-root containment rejections, the dynamic identity matrix,
//! the type-aware `.git` shadow, the canonical container paths,
//! the writable-surface invariant, `pass_env` filtering, and label
//! order.

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_spec::{
    resolve, select_git_shadow, EngineMode, GitShadowKind, MountSpec, NetworkMode,
    ResolvedIdentity, RuntimeFacts, SandboxSpec,
};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, OciPullPolicy};
use caduceus::infra::error::CaduceusError;

mod support;

/// Build a config and worktree path under its `workdir_base`.
/// `resolve` does no I/O, so the paths never need to exist and the
/// tempdir can be dropped immediately.
fn base() -> (Config, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    (cfg, worktree)
}

/// Build runtime facts from a config's workdir_base via the shared
/// fixture (owner 4242:4242, rootful, pointer-file shadow kind).
fn runtime_for(cfg: &Config, worktree: &Path) -> RuntimeFacts {
    support::runtime_facts(cfg, "run-001", worktree)
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// (a) non-digest image → OciImageNotDigestPinned.
#[test]
fn rejects_non_digest_image() {
    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").image = "caduceus-worker:latest".to_string();
    let runtime = runtime_for(&cfg, &worktree);
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
    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").pull_policy = OciPullPolicy::Always;
    let runtime = runtime_for(&cfg, &worktree);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciPullPolicyIncompatible { .. } => {}
        other => panic!("expected OciPullPolicyIncompatible; got: {other:?}"),
    }
}

/// (c) worktree outside workdir_base → OciUndeclaredMount.
#[test]
fn rejects_worktree_outside_workdir_base() {
    let (cfg, _) = base();
    let runtime = runtime_for(&cfg, Path::new("/tmp/elsewhere/worktree"));
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

/// (c) output_dir outside the daemon state dir → OciUndeclaredMount
/// (the legacy worktree-sibling `result` path is no longer
/// allow-listed; `/output` is daemon-owned now).
#[test]
fn rejects_output_outside_state_dir() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.output_dir = PathBuf::from("/tmp/elsewhere/result");
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
    let (cfg, _) = base();
    let runtime = runtime_for(&cfg, Path::new("relative/worktree"));
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciUndeclaredMount { .. }),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

/// (d) output_dir equal to worktree → OciMountConflict.
#[test]
fn rejects_output_equal_to_worktree() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.output_dir = worktree.clone();
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciMountConflict { detail } => {
            assert!(
                detail.contains("disjoint"),
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
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.output_dir = worktree.parent().expect("parent").to_path_buf();
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict; got: {err:?}"
    );
}

/// (d) state_dir nested inside workdir_base → OciMountConflict: a
/// misconfigured state dir would put the daemon-owned `/output` and
/// `.git` shadow surfaces inside the worker-visible tree.
#[test]
fn rejects_state_dir_inside_workdir_base() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.state_dir = cfg.workdir_base.join("state");
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict; got: {err:?}"
    );
}

/// (d) `.git` shadow overlapping the output dir or the worktree →
/// OciMountConflict (the shadow must never equal or contain either).
#[test]
fn rejects_shadow_overlapping_output_or_worktree() {
    let (cfg, worktree) = base();
    // shadow == output_dir
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_host = runtime.output_dir.clone();
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict for shadow == output; got: {err:?}"
    );

    // shadow inside the worktree
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_host = worktree.join(".git-shadow");
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict for shadow inside worktree; got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Positive resolution contract
// ---------------------------------------------------------------------------

/// (e) dynamic identity: the resolved uid/gid are the probed
/// worktree-owner facts (never a hard-coded 1000), with the rootful
/// `--user` rule from the default fixture.
#[test]
fn resolves_owner_identity() {
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(
        spec.identity(),
        ResolvedIdentity {
            uid: 4242,
            gid: 4242,
            emit_user: true,
            userns: None,
        }
    );
}

/// (f) canonical container paths `/workspace` and `/output`; the
/// workspace binds directly to the per-run worktree (no copied or
/// stripped workspace) and `/output` is daemon-owned under the state
/// directory.
#[test]
fn resolves_canonical_container_paths() {
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");

    let ws = spec.workspace_mount();
    assert_eq!(ws.host_path, worktree);
    assert_eq!(ws.container_path, PathBuf::from("/workspace"));
    assert!(!ws.read_only, "workspace is RW");

    let out = spec.output_mount();
    assert_eq!(
        out.host_path,
        support::oci_output_dir(&cfg, "run-001"),
        "/output host path must be daemon-owned under state_dir"
    );
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

    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").pass_env = vec![
        "CADUCEUS_RESOLVE_TEST_PASS_ENV".to_string(),
        "CADUCEUS_RESOLVE_TEST_UNSET".to_string(),
    ];
    let runtime = runtime_for(&cfg, &worktree);
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
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
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
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec: SandboxSpec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert!(!spec.name().is_empty());
    assert!(spec.image().contains("@sha256:"));
    assert!(!spec.command().is_empty());
    assert_eq!(spec.tmpfs().len(), 2);
    assert!(!spec.environment().is_empty());
    assert!(!spec.labels().is_empty());
    assert!(spec.resources().memory_mb > 0);
    assert!(matches!(
        spec.network(),
        NetworkMode::None | NetworkMode::Unrestricted
    ));
    let _ = spec.security();
}

/// The tmpfs sizes come from `resources.{tmpfs_mb, shm_mb}`.
#[test]
fn tmpfs_sizes_from_resources() {
    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").resources.tmpfs_mb = 512;
    cfg.sandbox.as_mut().expect("sandbox").resources.shm_mb = 96;
    let runtime = runtime_for(&cfg, &worktree);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(spec.tmpfs()[0].target, "/tmp");
    assert_eq!(spec.tmpfs()[0].size_mb, 512);
    assert_eq!(spec.tmpfs()[1].target, "/dev/shm");
    assert_eq!(spec.tmpfs()[1].size_mb, 96);
}

// ---------------------------------------------------------------------------
// .git shadow selection (design D5)
// ---------------------------------------------------------------------------

/// Pure `select_git_shadow`: File and Dir both yield the read-only
/// mount of the shadow at `/workspace/.git`; Absent yields `None`.
#[test]
fn select_git_shadow_covers_all_kinds() {
    let shadow_host = Path::new("/state/oci-runs/run-001/git-shadow");
    for kind in [GitShadowKind::File, GitShadowKind::Dir] {
        let expected = MountSpec {
            host_path: shadow_host.to_path_buf(),
            container_path: PathBuf::from("/workspace/.git"),
            read_only: true,
        };
        assert_eq!(select_git_shadow(kind, shadow_host), Some(expected));
    }
    assert_eq!(select_git_shadow(GitShadowKind::Absent, shadow_host), None);
}

/// `resolve` with each `GitShadowKind`: Absent ⇒ no shadow and the
/// rest of the spec is unchanged; File/Dir ⇒ the shadow mount is
/// present and read-only.
#[test]
fn resolve_wires_git_shadow_per_kind() {
    for kind in [GitShadowKind::File, GitShadowKind::Dir] {
        let (cfg, worktree) = base();
        let mut runtime = runtime_for(&cfg, &worktree);
        runtime.git_shadow_kind = kind;
        let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
        let shadow = spec.git_shadow().expect("shadow must be present");
        assert_eq!(shadow.host_path, support::git_shadow_host(&cfg, "run-001"));
        assert_eq!(shadow.container_path, PathBuf::from("/workspace/.git"));
        assert!(shadow.read_only, "shadow must be read-only");
    }

    // Absent ⇒ no shadow.
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_kind = GitShadowKind::Absent;
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert!(
        spec.git_shadow().is_none(),
        "absent .git must yield no shadow"
    );
}

// ---------------------------------------------------------------------------
// Writable-surface invariant (design D7)
// ---------------------------------------------------------------------------

/// The resolved writable host-backed set is exactly
/// `{/workspace, /output}`, the tmpfs set is exactly
/// `[/tmp, /dev/shm]` with the configured sizes, and the `.git`
/// shadow is the only extra host-backed mount and is read-only.
#[test]
fn writable_surface_invariant_holds() {
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");

    let ws = spec.workspace_mount();
    assert_eq!(ws.container_path, PathBuf::from("/workspace"));
    assert!(!ws.read_only);
    let out = spec.output_mount();
    assert_eq!(out.container_path, PathBuf::from("/output"));
    assert!(!out.read_only);

    let shadow = spec.git_shadow().expect("fixture has a .git pointer file");
    assert!(
        shadow.read_only,
        "the shadow is the only extra mount and is RO"
    );

    let tmpfs: Vec<(String, u64)> = spec
        .tmpfs()
        .iter()
        .map(|m| (m.target.clone(), m.size_mb))
        .collect();
    assert_eq!(
        tmpfs,
        vec![
            ("/tmp".to_string(), cfg.sandbox().resources.tmpfs_mb),
            ("/dev/shm".to_string(), cfg.sandbox().resources.shm_mb),
        ],
        "tmpfs set must be exactly the two bounded ephemeral surfaces"
    );
}

/// The spec is closed: an extra writable host-backed mount at a
/// container path other than `/workspace` or `/output` is
/// unrepresentable, and the nearest misconfiguration attempt (a state
/// dir under `workdir_base`, which would smuggle extra writable host
/// paths into the worker-visible tree) is rejected at resolution
/// time with a typed error — no container is ever started.
#[test]
fn extra_writable_host_paths_are_rejected_at_resolution() {
    // Structural closure: the resolved mount inventory is exactly the
    // two writable surfaces plus the optional read-only shadow. There
    // is no public constructor or builder that could add another
    // host-backed mount.
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    let mut rw_container_paths: Vec<PathBuf> = vec![
        spec.workspace_mount().container_path.clone(),
        spec.output_mount().container_path.clone(),
    ];
    if let Some(shadow) = spec.git_shadow() {
        assert!(shadow.read_only, "any extra mount must be read-only");
    }
    rw_container_paths.sort();
    assert_eq!(
        rw_container_paths,
        vec![PathBuf::from("/output"), PathBuf::from("/workspace")],
        "exactly two writable host-backed surfaces may exist"
    );

    // A state dir inside workdir_base would hand the worker extra
    // writable host paths — rejected at resolution time.
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.state_dir = cfg.workdir_base.join("state");
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    let err = resolve(cfg.sandbox(), &runtime).expect_err("must reject");
    match err {
        CaduceusError::OciMountConflict { detail } => {
            assert!(
                detail.contains("state_dir"),
                "expected state_dir/workdir_base conflict, got: {detail}"
            );
        }
        other => panic!("expected OciMountConflict; got: {other:?}"),
    }
}

/// Engine-mode facts flow into identity without I/O: a rootless-mode
/// fact set resolves to no `--user` emission (Docker) even though the
/// owner uid/gid are still carried.
#[test]
fn engine_mode_fact_flows_into_identity() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.engine_mode = EngineMode::Rootless;
    let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
    assert_eq!(spec.identity().uid, 4242);
    assert_eq!(spec.identity().gid, 4242);
    assert!(!spec.identity().emit_user, "rootless emits no --user");
    assert_eq!(spec.identity().userns, None);
}
