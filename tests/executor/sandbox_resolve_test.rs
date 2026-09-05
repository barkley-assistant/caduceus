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
    is_engine_socket_path, resolve, select_git_shadow, validate_no_host_escalation, EngineMode,
    GitShadowKind, MountSpec, NetworkMode, ResolvedIdentity, RuntimeFacts, SandboxSpec,
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
    match err {
        CaduceusError::OciImageNotDigestPinned { reference } => {
            assert_eq!(reference, "caduceus-worker:latest");
        }
        other => panic!("expected OciImageNotDigestPinned; got: {other:?}"),
    }
}

/// (b) pull_policy Always + digest image is accepted and remains available
/// to the acquisition layer.
#[test]
fn accepts_pull_policy_always_with_digest() {
    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").pull_policy = OciPullPolicy::Always;
    let runtime = runtime_for(&cfg, &worktree);
    let resolved = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect("always must resolve");
    assert_eq!(resolved.image_ref(), cfg.sandbox().image);
}

/// (c) worktree outside workdir_base → OciUndeclaredMount.
#[test]
fn rejects_worktree_outside_workdir_base() {
    let (cfg, _) = base();
    let runtime = runtime_for(&cfg, Path::new("/tmp/elsewhere/worktree"));
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
    assert!(
        matches!(err, CaduceusError::OciMountConflict { .. }),
        "expected OciMountConflict for shadow == output; got: {err:?}"
    );

    // shadow inside the worktree
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_host = worktree.join(".git-shadow");
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
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
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");

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

/// (g) pass_env filtering — FROZEN v1 semantics (issue #249): a
/// name PRESENT in the daemon process environment is included; a
/// name ABSENT fails the run with a typed error naming the variable
/// (never warn-and-skip). The full resolution matrix lives in
/// `sandbox_spec_env_test.rs`.
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
    // ABSENT ⇒ typed error; nothing is silently skipped.
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("absent pass_env name must fail the run");
    match &err {
        CaduceusError::Config(msg) => {
            assert!(
                msg.contains("CADUCEUS_RESOLVE_TEST_UNSET")
                    && msg.contains("not present in daemon environment"),
                "error must name the absent variable: {msg}"
            );
        }
        other => panic!("expected CaduceusError::Config; got: {other:?}"),
    }

    // PRESENT ⇒ included (alongside the canonical + compat set).
    cfg.sandbox.as_mut().expect("sandbox").pass_env =
        vec!["CADUCEUS_RESOLVE_TEST_PASS_ENV".to_string()];
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
    let env = spec.environment();
    assert!(
        env.contains(&(
            "CADUCEUS_RESOLVE_TEST_PASS_ENV".to_string(),
            "present-value".to_string()
        )),
        "resolved value must be present, got: {env:?}"
    );
    // Canonical 11 + compat 2 + the one resolved entry.
    assert_eq!(env.len(), 14, "got: {env:?}");
}

/// (g') The resolved environment carries the FULL canonical
/// `CADUCEUS_*` set with CONTAINER-side path values: worktree and
/// result paths are the fixed container paths, never the host
/// worktree/output paths. Canonical free-text values mirror the spec
/// inputs with newline normalization (the OCI env file is
/// line-based; the spec input here is deliberately multi-line).
#[test]
fn resolves_full_canonical_environment_with_container_paths() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.run_id = "run-100".to_string();
    runtime.issue = IssueKey::parse("octocat/hello#42").expect("valid key");
    let spec_input = caduceus::executor::ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: runtime.issue.clone(),
            title: "A title".to_string(),
            body: "A body\nwith newline".to_string(),
            labels: vec!["p1".to_string(), "p2".to_string()],
            branch_name: "caduceus/octocat/hello#42".to_string(),
        }),
        worktree: worktree.clone(),
        run_id: "run-100".to_string(),
        context_json: "{\"context\":true}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let resolved = resolve(cfg.sandbox(), &runtime, &spec_input).expect("must resolve");
    let env = resolved.environment();

    // Canonical 11 + compat HOME/TMPDIR, sorted by key (issue #249).
    let expected: &[(&str, String)] = &[
        (
            "CADUCEUS_BRANCH_NAME",
            "caduceus/octocat/hello#42".to_string(),
        ),
        ("CADUCEUS_CONTEXT_JSON", "{\"context\":true}".to_string()),
        ("CADUCEUS_ISSUE_BODY", "A body with newline".to_string()),
        ("CADUCEUS_ISSUE_ID", "octocat/hello#42".to_string()),
        ("CADUCEUS_ISSUE_LABELS_JSON", "[\"p1\",\"p2\"]".to_string()),
        ("CADUCEUS_ISSUE_NUMBER", "42".to_string()),
        ("CADUCEUS_ISSUE_REPO", "octocat/hello".to_string()),
        ("CADUCEUS_ISSUE_TITLE", "A title".to_string()),
        (
            "CADUCEUS_RESULT_PATH",
            "/output/worker-result.json".to_string(),
        ),
        ("CADUCEUS_RUN_ID", "run-100".to_string()),
        // Container-side paths, never host paths.
        ("CADUCEUS_WORKTREE_PATH", "/workspace".to_string()),
        ("HOME", "/tmp".to_string()),
        ("TMPDIR", "/tmp".to_string()),
    ];
    assert_eq!(
        env.len(),
        expected.len(),
        "exactly the canonical set plus compat"
    );
    for (entry, (key, value)) in env.iter().zip(expected.iter()) {
        assert_eq!(entry.0, *key);
        assert_eq!(&entry.1, value);
    }
    assert!(
        !env.iter().any(|entry| entry.0.contains("TOKEN")
            || entry.0.contains("SECRET")
            || entry.0.contains("GITHUB_TOKEN")),
        "no credential variable may be emitted: {env:?}"
    );
    // Host paths must not leak into the container environment.
    let host_worktree = worktree.display().to_string();
    assert!(
        !env.iter().any(|(_, v)| v.contains(&host_worktree)),
        "host worktree path must not appear in container env, got: {env:?}"
    );
}

/// (g'') A multi-line issue title/body (routine GitHub input) is
/// newline-normalized at resolution, so the resolved environment
/// assembles into a well-formed single-line env file and the run
/// proceeds — no pre-create error. The full multi-line content still
/// reaches the worker via the prompt file written into the worktree
/// (`write_prompt`); the normalization is transport-format only.
#[test]
fn multi_line_issue_text_resolves_to_single_line_env_file() {
    use std::collections::BTreeMap;

    use caduceus::executor::oci_env_file::OciEnvFile;

    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec_input = caduceus::executor::ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: runtime.issue.clone(),
            title: "A title\nwith lines\r\nand CR".to_string(),
            body: "A body\r\nwith CRLF\nand LF".to_string(),
            labels: vec!["p1".to_string()],
            branch_name: "caduceus/owner/repo#1".to_string(),
        }),
        worktree: worktree.clone(),
        run_id: "run-101".to_string(),
        context_json: "{}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let resolved = resolve(cfg.sandbox(), &runtime, &spec_input).expect(
        "multi-line issue title/body must resolve (newline-normalized, \
         not rejected)",
    );
    let env: BTreeMap<String, String> = resolved.environment().iter().cloned().collect();
    assert_eq!(
        env.get("CADUCEUS_ISSUE_BODY").map(String::as_str),
        Some("A body with CRLF and LF"),
        "CRLF and LF must collapse to single spaces"
    );
    assert_eq!(
        env.get("CADUCEUS_ISSUE_TITLE").map(String::as_str),
        Some("A title with lines and CR"),
        "newlines must collapse to single spaces"
    );

    // The whole resolved environment must assemble into a
    // well-formed env file: exactly one KEY=VALUE line per entry,
    // with no embedded newline inside any value.
    let tmp = tempfile::tempdir().expect("tempdir");
    let run_dir = tmp.path().join("oci-runs").join("run-multi");
    let file = OciEnvFile::create(&run_dir, &env)
        .expect("multi-line issue text must produce a valid env file");
    let body = std::fs::read_to_string(file.path()).expect("read env file");
    assert_eq!(
        body.lines().count(),
        env.len(),
        "exactly one line per env entry, got: {body:?}"
    );
    drop(file);
}

/// (h) labels in fixed order daemon_id, run_id, issue_id.
#[test]
fn resolves_labels_in_fixed_order() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.run_id = "run-007".to_string();
    runtime.issue = IssueKey::parse("owner/repo#7").expect("valid key");
    runtime.daemon_id = "daemon-42".to_string();
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
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
    let spec: SandboxSpec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
    assert!(!spec.name().is_empty());
    assert!(spec.image().contains("@sha256:"));
    assert!(!spec.command().is_empty());
    assert_eq!(spec.tmpfs().len(), 2);
    assert!(!spec.environment().is_empty());
    assert!(!spec.labels().is_empty());
    assert!(spec.resources().memory_mb > 0);
    // SAN-NET-2: the default network mode is `None` (`--network
    // none`, loopback-only); `unrestricted` opts into the engine's
    // default isolated bridge.
    assert!(matches!(spec.network(), NetworkMode::None));
    let _ = spec.security();
}

/// The tmpfs sizes come from `resources.{tmpfs_mb, shm_mb}`.
#[test]
fn tmpfs_sizes_from_resources() {
    let (mut cfg, worktree) = base();
    cfg.sandbox.as_mut().expect("sandbox").resources.tmpfs_mb = 512;
    cfg.sandbox.as_mut().expect("sandbox").resources.shm_mb = 96;
    let runtime = runtime_for(&cfg, &worktree);
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
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
        let spec = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
            .expect("must resolve");
        let shadow = spec.git_shadow().expect("shadow must be present");
        assert_eq!(shadow.host_path, support::git_shadow_host(&cfg, "run-001"));
        assert_eq!(shadow.container_path, PathBuf::from("/workspace/.git"));
        assert!(shadow.read_only, "shadow must be read-only");
    }

    // Absent ⇒ no shadow.
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_kind = GitShadowKind::Absent;
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
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
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");

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
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
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
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("must reject");
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
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
    assert_eq!(spec.identity().uid, 4242);
    assert_eq!(spec.identity().gid, 4242);
    assert!(!spec.identity().emit_user, "rootless emits no --user");
    assert_eq!(spec.identity().userns, None);
}

// ---------------------------------------------------------------------------
// Host-escalation deny (design D2, issue #245)
// ---------------------------------------------------------------------------

/// (a) A host-backed mount whose host path is named `docker.sock` /
/// `podman.sock` is rejected at resolution time, before any container
/// exists. Reached through `resolve`'s validate_mount_policy →
/// validate_no_host_escalation call ordering via the `.git` shadow
/// host path (the only caller-supplied host path that stays inside
/// the state-dir allow-list).
#[test]
fn resolve_rejects_socket_named_host_path() {
    for socket in ["docker.sock", "podman.sock"] {
        let (cfg, worktree) = base();
        let mut runtime = runtime_for(&cfg, &worktree);
        runtime.git_shadow_host = cfg.state_dir.join("oci-runs").join("run-001").join(socket);
        let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
            .expect_err("socket-named host path must be rejected");
        match &err {
            CaduceusError::OciMountConflict { detail } => {
                assert!(
                    detail.contains(socket),
                    "conflict must name the socket path, got: {detail}"
                );
            }
            other => panic!("expected OciMountConflict; got: {other:?}"),
        }
    }
}

/// (b) Container paths are fixed canonical constants, so the
/// socket-named *container* path branch of the validator is
/// structurally unreachable through `resolve`; the socket-name check
/// itself is pinned via `is_engine_socket_path` on container-path
/// shapes, and the validator rejects anything it is ever fed.
#[test]
fn engine_socket_detection_covers_container_path_shapes() {
    assert!(is_engine_socket_path(Path::new("/var/run/docker.sock")));
    assert!(is_engine_socket_path(Path::new("/run/podman/podman.sock")));
    assert!(is_engine_socket_path(Path::new("docker.sock")));
    assert!(!is_engine_socket_path(Path::new("/workspace")));
    assert!(!is_engine_socket_path(Path::new("/output")));
    assert!(!is_engine_socket_path(Path::new("/workspace/.git")));
    assert!(!is_engine_socket_path(Path::new(
        "/var/run/docker.sock.bak"
    )));
    assert!(!is_engine_socket_path(Path::new("")));
}

/// (c) The default spec passes the escalation validator — a plain
/// resolved spec carries no socket paths and `NetworkMode::None`.
#[test]
fn default_spec_passes_escalation_validator() {
    let (cfg, worktree) = base();
    let runtime = runtime_for(&cfg, &worktree);
    let spec =
        resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve");
    validate_no_host_escalation(&spec).expect("default spec must pass");
}

/// (d) The validator runs inside `resolve` (ordering: after the mount
/// policy, before the spec is returned) — a socket-named shadow host
/// path is refused by `resolve` itself with a typed error, proving
/// the call site is reachable end-to-end.
#[test]
fn escalation_validator_is_reachable_through_resolve() {
    let (cfg, worktree) = base();
    let mut runtime = runtime_for(&cfg, &worktree);
    runtime.git_shadow_host = cfg
        .state_dir
        .join("oci-runs")
        .join("run-001")
        .join("docker.sock");
    let result = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime));
    assert!(
        matches!(
            result,
            Err(CaduceusError::OciMountConflict { ref detail }) if detail.contains("socket")
        ),
        "resolve must run the escalation validator; got: {result:?}"
    );
}
