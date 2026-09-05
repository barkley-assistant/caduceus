//! Golden argv snapshot tests for the pure deterministic renderer.
//!
//! The goldens are literal expected `Vec<String>` values. Because
//! `SandboxSpec` is closed (the only constructor is `resolve`), the
//! fixtures resolve a fixed `Config` + `RuntimeFacts` first; `resolve`
//! does no I/O, so the fixed host paths never need to exist and the
//! goldens are byte-for-byte deterministic.
//!
//! Identity is now dynamic (worktree-owner facts from the shared
//! fixture: `4242:4242`, rootful by default) and the `.git` shadow
//! mount and dual tmpfs list are part of every golden.

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_renderer::{render, render_with_env_files};
use caduceus::executor::sandbox_spec::{
    EngineMode, GitShadowKind, NetworkMode, SandboxEngine, SandboxSpec,
};
use caduceus::infra::config::{Config, SandboxConfig, SandboxNetwork};

/// Fixed root for the golden fixtures. Never touched on disk.
const ROOT: &str = "/tmp/caduceus-renderer-goldens";

/// Build an `ExecutorSpec` fixture mirroring the runtime facts, with
/// fixed representative canonical values (title/body/labels/branch/
/// context) that the environment goldens interpolate.
fn executor_spec_for(
    runtime: &caduceus::executor::sandbox_spec::RuntimeFacts,
) -> caduceus::executor::ExecutorSpec {
    caduceus::executor::ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: caduceus::github::issue::IssueKey::parse(&runtime.target)
                .expect("fixture target parses as issue key"),
            title: "Fix login bug".to_string(),
            body: "Steps to reproduce".to_string(),
            labels: vec!["bug".to_string()],
            branch_name: "caduceus/owner/repo#1".to_string(),
        }),
        worktree: runtime.worktree.clone(),
        run_id: runtime.run_id.clone(),
        context_json: "{}".to_string(),
        worker_command: runtime.worker_command.clone(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

/// Resolve a fixture spec from `Config::test_defaults` with an
/// optional sandbox mutation and runtime-fact overrides. Returns the
/// spec plus the host paths the goldens interpolate into mount args.
fn fixture_with(
    run_id: &str,
    engine_mode: EngineMode,
    shadow_kind: GitShadowKind,
    mutate: impl FnOnce(&mut SandboxConfig),
) -> (SandboxSpec, PathBuf, PathBuf, PathBuf, String) {
    let root = Path::new(ROOT);
    let mut cfg = Config::test_defaults(root);
    mutate(cfg.sandbox.as_mut().expect("test_defaults has a sandbox"));
    let worktree = root
        .join("workdirs")
        .join("owner")
        .join("repo")
        .join(run_id);
    let output = cfg.state_dir.join("oci-runs").join(run_id).join("output");
    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: run_id.to_string(),
        target: "owner/repo#1".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree: worktree.clone(),
        output_dir: output.clone(),
        daemon_id: "test-daemon".to_string(),
        workdir_base: root.join("workdirs"),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: 4242,
        worktree_gid: 4242,
        engine_mode,
        git_shadow_kind: shadow_kind,
        git_shadow_host: cfg
            .state_dir
            .join("oci-runs")
            .join(run_id)
            .join("git-shadow"),
    };
    let spec = caduceus::executor::sandbox_spec::resolve(
        cfg.sandbox(),
        &runtime,
        &executor_spec_for(&runtime),
    )
    .expect("fixture must resolve");
    (
        spec,
        worktree,
        output,
        runtime.git_shadow_host,
        cfg.sandbox().image.clone(),
    )
}

/// The default fixture: rootful, pointer-file `.git` shadow.
fn default_fixture() -> (SandboxSpec, PathBuf, PathBuf, PathBuf, String) {
    fixture_with("run-001", EngineMode::Rootful, GitShadowKind::File, |_| {})
}

// ---------------------------------------------------------------------------
// Golden snapshots — one per engine×mode, byte-for-byte.
// ---------------------------------------------------------------------------

#[test]
fn docker_rootful_golden_argv() {
    let (spec, worktree, output, shadow, image) = default_fixture();
    let expected = vec![
        "docker".to_string(),
        "create".to_string(),
        "--user".to_string(),
        "4242:4242".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--network".to_string(),
        "none".to_string(),
        "--cpus".to_string(),
        "2".to_string(),
        "--memory".to_string(),
        "2048m".to_string(),
        "--memory-swap".to_string(),
        "2048m".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--log-opt".to_string(),
        "max-size=10m".to_string(),
        "--log-opt".to_string(),
        "max-file=3".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "-v".to_string(),
        format!("{}:/workspace/.git:ro", shadow.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "--tmpfs".to_string(),
        "/dev/shm:size=64m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_NUMBER=1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_REPO=owner/repo".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_TITLE=Fix login bug".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_BODY=Steps to reproduce".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_LABELS_JSON=[\"bug\"]".to_string(),
        "-e".to_string(),
        "CADUCEUS_CONTEXT_JSON={}".to_string(),
        "-e".to_string(),
        "CADUCEUS_BRANCH_NAME=caduceus/owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_WORKTREE_PATH=/workspace".to_string(),
        "-e".to_string(),
        "CADUCEUS_RESULT_PATH=/output/worker-result.json".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=test-daemon".to_string(),
        "-l".to_string(),
        "caduceus.run_id=run-001".to_string(),
        "-l".to_string(),
        "caduceus.issue_id=owner/repo#1".to_string(),
        "--entrypoint".to_string(),
        "python3".to_string(),
        image.clone(),
        "bridge.py".to_string(),
    ];
    assert_eq!(render(&spec, SandboxEngine::Docker), expected);
}

#[test]
fn podman_rootless_golden_argv() {
    let (spec, worktree, output, shadow, image) =
        fixture_with("run-001", EngineMode::Rootless, GitShadowKind::File, |sb| {
            sb.engine = SandboxEngine::Podman
        });
    let expected = vec![
        "podman".to_string(),
        "create".to_string(),
        // Rootless: no --user; plain keep-id user namespace.
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--userns".to_string(),
        "keep-id".to_string(),
        "--network".to_string(),
        "none".to_string(),
        "--cpus".to_string(),
        "2".to_string(),
        "--memory".to_string(),
        "2048m".to_string(),
        "--memory-swap".to_string(),
        "2048m".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--log-opt".to_string(),
        "max-size=10m".to_string(),
        "--log-opt".to_string(),
        "max-file=3".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "-v".to_string(),
        format!("{}:/workspace/.git:ro", shadow.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "--tmpfs".to_string(),
        "/dev/shm:size=64m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_NUMBER=1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_REPO=owner/repo".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_TITLE=Fix login bug".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_BODY=Steps to reproduce".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_LABELS_JSON=[\"bug\"]".to_string(),
        "-e".to_string(),
        "CADUCEUS_CONTEXT_JSON={}".to_string(),
        "-e".to_string(),
        "CADUCEUS_BRANCH_NAME=caduceus/owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_WORKTREE_PATH=/workspace".to_string(),
        "-e".to_string(),
        "CADUCEUS_RESULT_PATH=/output/worker-result.json".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=test-daemon".to_string(),
        "-l".to_string(),
        "caduceus.run_id=run-001".to_string(),
        "-l".to_string(),
        "caduceus.issue_id=owner/repo#1".to_string(),
        "--entrypoint".to_string(),
        "python3".to_string(),
        image.clone(),
        "bridge.py".to_string(),
    ];
    assert_eq!(render(&spec, SandboxEngine::Podman), expected);
}

/// Golden: Docker + `NetworkMode::Unrestricted` renders the engine's
/// default isolated bridge — exactly `--network bridge` (NAT'd
/// outbound egress), never `--network host` and never `none`
/// (SAN-NET-3). Every other token matches the Docker rootful `none`
/// golden.
#[test]
fn docker_unrestricted_golden_argv() {
    let (spec, worktree, output, shadow, image) =
        fixture_with("run-001", EngineMode::Rootful, GitShadowKind::File, |sb| {
            sb.network = SandboxNetwork::Unrestricted
        });
    assert_eq!(spec.network(), NetworkMode::Unrestricted);
    let expected = vec![
        "docker".to_string(),
        "create".to_string(),
        "--user".to_string(),
        "4242:4242".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--network".to_string(),
        "bridge".to_string(),
        "--cpus".to_string(),
        "2".to_string(),
        "--memory".to_string(),
        "2048m".to_string(),
        "--memory-swap".to_string(),
        "2048m".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--log-opt".to_string(),
        "max-size=10m".to_string(),
        "--log-opt".to_string(),
        "max-file=3".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "-v".to_string(),
        format!("{}:/workspace/.git:ro", shadow.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "--tmpfs".to_string(),
        "/dev/shm:size=64m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_NUMBER=1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_REPO=owner/repo".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_TITLE=Fix login bug".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_BODY=Steps to reproduce".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_LABELS_JSON=[\"bug\"]".to_string(),
        "-e".to_string(),
        "CADUCEUS_CONTEXT_JSON={}".to_string(),
        "-e".to_string(),
        "CADUCEUS_BRANCH_NAME=caduceus/owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_WORKTREE_PATH=/workspace".to_string(),
        "-e".to_string(),
        "CADUCEUS_RESULT_PATH=/output/worker-result.json".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=test-daemon".to_string(),
        "-l".to_string(),
        "caduceus.run_id=run-001".to_string(),
        "-l".to_string(),
        "caduceus.issue_id=owner/repo#1".to_string(),
        "--entrypoint".to_string(),
        "python3".to_string(),
        image.clone(),
        "bridge.py".to_string(),
    ];
    let argv = render(&spec, SandboxEngine::Docker);
    // Exact token: the only value after --network is `bridge`.
    let net_pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[net_pos + 1], "bridge");
    assert_ne!(argv.get(net_pos + 1).map(String::as_str), Some("host"));
    assert_ne!(argv.get(net_pos + 1).map(String::as_str), Some("none"));
    // Byte-for-byte golden.
    assert_eq!(argv, expected);
}

/// Golden: Podman + `NetworkMode::Unrestricted` renders Podman's
/// default isolated bridge — exactly `--network bridge` (NAT'd
/// outbound egress; token pinned per `podman-create(1)`: "bridge:
/// Create a network stack on the default bridge"), never
/// `--network host` and never `none` (SAN-NET-3). Every other token
/// matches the Podman rootless `none` golden.
#[test]
fn podman_unrestricted_golden_argv() {
    let (spec, worktree, output, shadow, image) =
        fixture_with("run-001", EngineMode::Rootless, GitShadowKind::File, |sb| {
            sb.engine = SandboxEngine::Podman;
            sb.network = SandboxNetwork::Unrestricted;
        });
    assert_eq!(spec.network(), NetworkMode::Unrestricted);
    let expected = vec![
        "podman".to_string(),
        "create".to_string(),
        // Rootless: no --user; plain keep-id user namespace.
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--userns".to_string(),
        "keep-id".to_string(),
        "--network".to_string(),
        "bridge".to_string(),
        "--cpus".to_string(),
        "2".to_string(),
        "--memory".to_string(),
        "2048m".to_string(),
        "--memory-swap".to_string(),
        "2048m".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--log-opt".to_string(),
        "max-size=10m".to_string(),
        "--log-opt".to_string(),
        "max-file=3".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "-v".to_string(),
        format!("{}:/workspace/.git:ro", shadow.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "--tmpfs".to_string(),
        "/dev/shm:size=64m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_NUMBER=1".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_REPO=owner/repo".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_TITLE=Fix login bug".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_BODY=Steps to reproduce".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_LABELS_JSON=[\"bug\"]".to_string(),
        "-e".to_string(),
        "CADUCEUS_CONTEXT_JSON={}".to_string(),
        "-e".to_string(),
        "CADUCEUS_BRANCH_NAME=caduceus/owner/repo#1".to_string(),
        "-e".to_string(),
        "CADUCEUS_WORKTREE_PATH=/workspace".to_string(),
        "-e".to_string(),
        "CADUCEUS_RESULT_PATH=/output/worker-result.json".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=test-daemon".to_string(),
        "-l".to_string(),
        "caduceus.run_id=run-001".to_string(),
        "-l".to_string(),
        "caduceus.issue_id=owner/repo#1".to_string(),
        "--entrypoint".to_string(),
        "python3".to_string(),
        image.clone(),
        "bridge.py".to_string(),
    ];
    let argv = render(&spec, SandboxEngine::Podman);
    // Exact token: the only value after --network is `bridge`.
    let net_pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[net_pos + 1], "bridge");
    assert_ne!(argv.get(net_pos + 1).map(String::as_str), Some("host"));
    assert_ne!(argv.get(net_pos + 1).map(String::as_str), Some("none"));
    // Byte-for-byte golden.
    assert_eq!(argv, expected);
}

/// The per-engine deltas are fully encoded in the spec's identity:
/// - Podman rootful vs Docker rootful differ only by the binary name;
/// - Docker rootless vs Docker rootful differ only by the missing
///   `--user`/`4242:4242` pair;
/// - Podman rootless vs Podman rootful swap the `--user` pair for the
///   `--userns keep-id` pair.
#[test]
fn per_engine_mode_deltas() {
    let (spec_docker_rootful, _, _, _, _) = default_fixture();
    let (spec_docker_rootless, _, _, _, _) =
        fixture_with("run-001", EngineMode::Rootless, GitShadowKind::File, |_| {});
    let (spec_podman_rootful, _, _, _, _) =
        fixture_with("run-001", EngineMode::Rootful, GitShadowKind::File, |sb| {
            sb.engine = SandboxEngine::Podman
        });
    let (spec_podman_rootless, _, _, _, _) =
        fixture_with("run-001", EngineMode::Rootless, GitShadowKind::File, |sb| {
            sb.engine = SandboxEngine::Podman
        });

    let docker_rootful = render(&spec_docker_rootful, SandboxEngine::Docker);
    let docker_rootless = render(&spec_docker_rootless, SandboxEngine::Docker);
    let podman_rootful = render(&spec_podman_rootful, SandboxEngine::Podman);
    let podman_rootless = render(&spec_podman_rootless, SandboxEngine::Podman);

    // Rootful across engines: binary name only.
    let mut podman_rootful_expected = docker_rootful.clone();
    podman_rootful_expected[0] = "podman".to_string();
    assert_eq!(podman_rootful, podman_rootful_expected);

    // Docker rootless = Docker rootful minus the --user pair.
    let user_pos = docker_rootful
        .iter()
        .position(|a| a == "--user")
        .expect("--user");
    let mut docker_rootless_expected = docker_rootful.clone();
    docker_rootless_expected.drain(user_pos..user_pos + 2);
    assert_eq!(docker_rootless, docker_rootless_expected);

    // Podman rootless = Podman rootful minus the --user pair, plus
    // the --userns keep-id pair at the renderer's structural
    // position — after `--read-only`, before `--network`.
    let mut podman_rootless_expected = podman_rootful.clone();
    let user_pos = podman_rootless_expected
        .iter()
        .position(|a| a == "--user")
        .expect("--user");
    podman_rootless_expected.drain(user_pos..user_pos + 2);
    let userns_pos = podman_rootless_expected
        .iter()
        .position(|a| a == "--read-only")
        .expect("--read-only")
        + 1;
    podman_rootless_expected.splice(
        userns_pos..userns_pos,
        ["--userns".to_string(), "keep-id".to_string()],
    );
    assert_eq!(podman_rootless, podman_rootless_expected);
}

/// Determinism contract: two invocations on the same inputs are
/// byte-identical.
#[test]
fn render_is_deterministic() {
    let (spec, _, _, _, _) = default_fixture();
    assert_eq!(
        render(&spec, SandboxEngine::Docker),
        render(&spec, SandboxEngine::Docker)
    );
    assert_eq!(
        render(&spec, SandboxEngine::Podman),
        render(&spec, SandboxEngine::Podman)
    );
    let env_files = vec![
        PathBuf::from("/tmp/secrets-a.env"),
        PathBuf::from("/tmp/secrets-b.env"),
    ];
    assert_eq!(
        render_with_env_files(&spec, SandboxEngine::Docker, &env_files),
        render_with_env_files(&spec, SandboxEngine::Docker, &env_files)
    );
}

// ---------------------------------------------------------------------------
// Per-field coverage — every SandboxSpec field reaches the argv.
// ---------------------------------------------------------------------------

#[test]
fn renders_identity() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--user"));
    assert!(argv.iter().any(|a| a == "4242:4242"));
}

#[test]
fn renders_workspace_mount() {
    let (spec, worktree, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "-v"));
    assert!(argv
        .iter()
        .any(|a| a == &format!("{}:/workspace:rw", worktree.display())));
}

#[test]
fn renders_output_mount() {
    let (spec, _, output, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv
        .iter()
        .any(|a| a == &format!("{}:/output:rw", output.display())));
}

/// The `.git` shadow is rendered read-only AFTER the `/workspace`
/// bind (mount precedence: the nested bind wins by target depth; the
/// deterministic argv order matches the depth rule).
#[test]
fn renders_git_shadow_after_workspace() {
    let (spec, _, _, shadow, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let ws_pos = argv
        .iter()
        .position(|a| a.ends_with(":/workspace:rw"))
        .expect("workspace mount");
    let shadow_pos = argv
        .iter()
        .position(|a| a == &format!("{}:/workspace/.git:ro", shadow.display()))
        .expect("shadow mount");
    assert!(
        shadow_pos > ws_pos,
        "shadow must be emitted after the workspace bind"
    );
}

#[test]
fn renders_tmpfs() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--tmpfs"));
    assert!(argv.iter().any(|a| a == "/tmp:size=256m"));
    assert!(argv.iter().any(|a| a == "/dev/shm:size=64m"));
}

/// `--shm-size` is gone; `/dev/shm` is declared via the dual tmpfs
/// list (design D7).
#[test]
fn shm_size_is_not_emitted() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        !argv.iter().any(|a| a == "--shm-size"),
        "--shm-size must not be emitted, got: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "64m"),
        "the standalone shm-size value must be gone, got: {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "/dev/shm:size=64m"),
        "/dev/shm must be declared as --tmpfs with the configured bound"
    );
}

#[test]
fn renders_environment() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "-e"));
    assert!(argv.iter().any(|a| a == "CADUCEUS_RUN_ID=run-001"));
    assert!(argv.iter().any(|a| a == "CADUCEUS_ISSUE_ID=owner/repo#1"));
}

/// The full canonical `CADUCEUS_*` set is emitted as `-e` entries in
/// canonical order, with the container-side worktree/result paths
/// (issue #243). No credential variable is ever emitted.
#[test]
fn renders_canonical_environment_entries() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let expected: &[&str] = &[
        "CADUCEUS_RUN_ID=run-001",
        "CADUCEUS_ISSUE_ID=owner/repo#1",
        "CADUCEUS_ISSUE_NUMBER=1",
        "CADUCEUS_ISSUE_REPO=owner/repo",
        "CADUCEUS_ISSUE_TITLE=Fix login bug",
        "CADUCEUS_ISSUE_BODY=Steps to reproduce",
        "CADUCEUS_ISSUE_LABELS_JSON=[\"bug\"]",
        "CADUCEUS_CONTEXT_JSON={}",
        "CADUCEUS_BRANCH_NAME=caduceus/owner/repo#1",
        "CADUCEUS_WORKTREE_PATH=/workspace",
        "CADUCEUS_RESULT_PATH=/output/worker-result.json",
    ];
    let env_entries: Vec<&String> = argv.iter().filter(|a| a.starts_with("CADUCEUS_")).collect();
    assert_eq!(
        env_entries
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>(),
        expected,
        "canonical -e entries must be present in canonical order"
    );
    assert!(
        !env_entries
            .iter()
            .any(|a| a.contains("TOKEN") || a.contains("SECRET")),
        "no credential -e entry may be emitted: {env_entries:?}"
    );
    // Host paths must never leak into the rendered environment.
    assert!(
        !env_entries.iter().any(|a| a.contains(ROOT)),
        "host paths must not appear in container env entries: {env_entries:?}"
    );
}

#[test]
fn renders_env_files_in_slice_order() {
    let (spec, _, _, _, _) = default_fixture();
    let env_files = vec![
        PathBuf::from("/tmp/secret-1.env"),
        PathBuf::from("/tmp/secret-2.env"),
    ];
    let argv = render_with_env_files(&spec, SandboxEngine::Docker, &env_files);
    let env_file_positions: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--env-file")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(env_file_positions.len(), 2, "one --env-file per path");
    assert_eq!(argv[env_file_positions[0] + 1], "/tmp/secret-1.env");
    assert_eq!(argv[env_file_positions[1] + 1], "/tmp/secret-2.env");
    assert!(
        env_file_positions[0] < env_file_positions[1],
        "env files must stay in slice order"
    );
}

// ---------------------------------------------------------------------------
// File-only transport (issue #249; design D4): a supplied env file is
// authoritative — zero `-e` tokens, so no env value reaches argv.
// ---------------------------------------------------------------------------

/// OCI file-mode render emits exactly one `--env-file <path>` and
/// ZERO `-e` tokens for any `spec.environment` entry.
#[test]
fn file_mode_emits_exactly_one_env_file_and_zero_e() {
    let (spec, _, _, _, _) = default_fixture();
    let env_file = PathBuf::from("/state/oci-runs/run-001/caduceus_env_01J.env");
    let argv = render_with_env_files(&spec, SandboxEngine::Docker, &[env_file]);
    let env_file_positions: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--env-file")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        env_file_positions.len(),
        1,
        "exactly one --env-file for the OCI environment, got: {argv:?}"
    );
    assert_eq!(
        argv[env_file_positions[0] + 1],
        "/state/oci-runs/run-001/caduceus_env_01J.env"
    );
    assert_eq!(
        argv.iter().filter(|a| a.as_str() == "-e").count(),
        0,
        "file mode must emit zero -e tokens, got: {argv:?}"
    );
}

/// Precedence pin: `env_files` non-empty ⇒ zero `-e` tokens even with
/// a non-empty `spec.environment`. The renderer's `-e` fallback runs
/// only for callers that render without env files.
#[test]
fn env_files_non_empty_implies_zero_e_even_with_non_empty_environment() {
    let (spec, _, _, _, _) = default_fixture();
    assert!(
        !spec.environment().is_empty(),
        "fixture spec has a non-empty environment"
    );
    // Plain render (no files) still emits the -e fallback.
    assert!(
        render(&spec, SandboxEngine::Docker)
            .iter()
            .any(|a| a.as_str() == "-e"),
        "the -e fallback must exist for callers without env files"
    );
    // File mode suppresses it entirely.
    let env_file = PathBuf::from("/state/oci-runs/run-001/caduceus_env_01K.env");
    let argv = render_with_env_files(&spec, SandboxEngine::Docker, &[env_file]);
    assert!(
        !argv.iter().any(|a| a.as_str() == "-e"),
        "env file is authoritative; -e must be suppressed, got: {argv:?}"
    );
}

/// No canonical env value bytes appear in file-mode argv: the
/// `--env-file` argument is the only env surface in argv.
#[test]
fn file_mode_argv_carries_no_env_value_bytes() {
    let (spec, _, _, _, _) = default_fixture();
    let env_file = PathBuf::from("/state/oci-runs/run-001/caduceus_env_01L.env");
    let argv = render_with_env_files(&spec, SandboxEngine::Docker, &[env_file]);
    // Representative canonical value bytes must not appear in argv.
    for value_bytes in ["Fix login bug", "Steps to reproduce", "run-001="] {
        assert!(
            !argv.iter().any(|a| a.contains(value_bytes)),
            "canonical value byte {value_bytes:?} must not appear in file-mode argv: {argv:?}"
        );
    }
    // No KEY=VALUE token for any env entry.
    assert!(
        !argv.iter().any(|a| a.contains('=')
            && (a.starts_with("CADUCEUS_") || a == "HOME=/tmp" || a == "TMPDIR=/tmp")),
        "no env KEY=VALUE token may appear in file-mode argv: {argv:?}"
    );
}

#[test]
fn renders_labels_in_fixed_order() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let labels: Vec<&String> = argv.iter().filter(|a| a.starts_with("caduceus.")).collect();
    assert_eq!(
        labels,
        vec![
            &"caduceus.daemon_id=test-daemon".to_string(),
            &"caduceus.run_id=run-001".to_string(),
            &"caduceus.issue_id=owner/repo#1".to_string(),
        ]
    );
}

#[test]
fn renders_resources() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--cpus"));
    assert!(argv.iter().any(|a| a == "2"));
    assert!(argv.iter().any(|a| a == "--memory"));
    assert!(argv.iter().any(|a| a == "2048m"));
    assert!(argv.iter().any(|a| a == "--pids-limit"));
    assert!(argv.iter().any(|a| a == "256"));
    assert!(argv.iter().any(|a| a == "--tmpfs"));
    assert!(argv.iter().any(|a| a == "/dev/shm:size=64m"));
}

#[test]
fn renders_network_none_by_default() {
    let (spec, _, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[pos + 1], "none");
}

/// Host networking was removed (breaking, issue #245): the rendered
/// argv must never carry `--network host` — the only value after
/// `--network` is `none`, for both engines.
#[test]
fn renders_network_none_never_host() {
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        let (spec, _, _, _, _) = default_fixture();
        let argv = render(&spec, engine);
        let pos = argv
            .iter()
            .position(|a| a == "--network")
            .expect("--network");
        assert_ne!(
            argv.get(pos + 1).map(String::as_str),
            Some("host"),
            "--network host must never be rendered ({engine:?}); got: {argv:?}"
        );
        assert_eq!(argv[pos + 1], "none");
    }
}

/// `--memory-swap` is pinned EQUAL to `--memory` (no swap doubling)
/// and both `--log-opt max-size=10m` / `--log-opt max-file=3` entries
/// are present, for BOTH engines (issue #245).
#[test]
fn renders_memory_swap_pinned_and_bounded_log_opts() {
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        let (spec, _, _, _, _) = default_fixture();
        let argv = render(&spec, engine);
        let mem_pos = argv.iter().position(|a| a == "--memory").expect("--memory");
        assert_eq!(argv[mem_pos + 1], "2048m");
        let swap_pos = argv
            .iter()
            .position(|a| a == "--memory-swap")
            .expect("--memory-swap");
        assert_eq!(
            argv[swap_pos + 1],
            argv[mem_pos + 1],
            "--memory-swap must equal --memory ({engine:?}); got: {argv:?}"
        );
        assert!(
            swap_pos == mem_pos + 2,
            "--memory-swap must immediately follow --memory's value"
        );
        let log_opt_positions: Vec<usize> = argv
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "--log-opt")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            log_opt_positions.len(),
            2,
            "exactly two --log-opt entries for {engine:?}: {argv:?}"
        );
        assert_eq!(argv[log_opt_positions[0] + 1], "max-size=10m");
        assert_eq!(argv[log_opt_positions[1] + 1], "max-file=3");
    }
}

/// Deny tripwire (issue #245): the rendered argv for BOTH engines
/// contains NO `--device`, NO `--pid host` / `--ipc host` /
/// `--uts host`, NO `--network host`, and no engine/runtime socket
/// substring. The renderer structurally cannot emit these; this test
/// pins that against regressions.
#[test]
fn renders_no_host_escalation_tokens() {
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        let (spec, _, _, _, _) = default_fixture();
        let argv = render(&spec, engine);
        for denied in ["--device", "--pid", "--ipc", "--uts"] {
            assert!(
                !argv.iter().any(|a| a == denied),
                "{denied} must never be rendered ({engine:?}); got: {argv:?}"
            );
        }
        let net_pos = argv
            .iter()
            .position(|a| a == "--network")
            .expect("--network");
        assert_eq!(argv[net_pos + 1], "none", "--network host is denied");
        for socket in ["docker.sock", "podman.sock"] {
            assert!(
                !argv.iter().any(|a| a.contains(socket)),
                "engine socket {socket} must never be mounted ({engine:?}); got: {argv:?}"
            );
        }
    }
}

#[test]
fn renders_entrypoint_image_and_worker_args() {
    let (spec, _, _, _, image) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let entrypoint_pos = argv
        .iter()
        .position(|a| a == "--entrypoint")
        .expect("--entrypoint");
    assert_eq!(argv[entrypoint_pos + 1], "python3");
    let image_pos = argv
        .iter()
        .position(|a| a.contains("@sha256:"))
        .expect("image");
    assert_eq!(argv[image_pos], image);
    // The image sits at a structural position: directly after
    // --entrypoint's value and before the worker args.
    assert_eq!(image_pos, entrypoint_pos + 2);
    assert_eq!(argv[image_pos + 1], "bridge.py");
    // No worker arg may precede the image.
    assert!(
        !argv[..image_pos].iter().any(|a| a == "bridge.py"),
        "worker args must trail the image"
    );
}

/// An absent host `.git` renders no shadow mount — the rest of the
/// argv is unchanged.
#[test]
fn absent_git_renders_no_shadow() {
    let (spec_with, _, _, _, _) = default_fixture();
    let (spec_absent, _, _, _, _) = fixture_with(
        "run-001",
        EngineMode::Rootful,
        GitShadowKind::Absent,
        |_| {},
    );
    let with = render(&spec_with, SandboxEngine::Docker);
    let absent = render(&spec_absent, SandboxEngine::Docker);
    let shadow_tokens: Vec<&String> = with
        .iter()
        .filter(|a| a.ends_with(":/workspace/.git:ro"))
        .collect();
    assert_eq!(shadow_tokens.len(), 1);
    // Drop the shadow mount's trailing `-v` flag token as well as the
    // path token, so the expected argv has no dangling `-v`.
    let mut drop_flags = vec![false; with.len()];
    for (i, a) in with.iter().enumerate() {
        if a.ends_with(":/workspace/.git:ro") {
            assert_eq!(with[i - 1], "-v", "shadow mount is `-v <spec>`");
            drop_flags[i - 1] = true;
            drop_flags[i] = true;
        }
    }
    let expected_without_shadow: Vec<String> = with
        .iter()
        .zip(&drop_flags)
        .filter(|(_, drop)| !**drop)
        .map(|(a, _)| a.clone())
        .collect();
    assert_eq!(absent, expected_without_shadow);
    assert!(spec_absent.git_shadow().is_none());
}

// ---------------------------------------------------------------------------
// Structural contract — legacy argv-mutation helpers are gone.
// ---------------------------------------------------------------------------

/// Spec scenario "Legacy helpers are absent from the source": none of
/// the deleted argv-mutation symbols may be defined or referenced in
/// the OCI executor source tree.
#[test]
fn legacy_argv_mutation_symbols_absent_from_source() {
    let project_root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let executor_dir = format!("{project_root}/src/executor");
    let forbidden = [
        "build_argv",
        "find_image_position",
        "inject_baseline_flags",
        "EnforcedSpec",
        "default_mounts",
        "git_snapshot_path",
    ];
    for entry in std::fs::read_dir(&executor_dir).expect("read src/executor") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for symbol in forbidden {
            assert!(
                !src.contains(symbol),
                "{symbol} must not appear in {}",
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// SandboxEngine detection (moved from the deleted oci_args module).
// ---------------------------------------------------------------------------

#[test]
fn sandbox_engine_detects_docker_or_podman() {
    assert_eq!(
        SandboxEngine::from_binary_name("docker"),
        SandboxEngine::Docker
    );
    assert_eq!(
        SandboxEngine::from_binary_name("/usr/bin/docker"),
        SandboxEngine::Docker
    );
    assert_eq!(
        SandboxEngine::from_binary_name("podman"),
        SandboxEngine::Podman
    );
    assert_eq!(
        SandboxEngine::from_binary_name("/usr/local/bin/podman"),
        SandboxEngine::Podman
    );
    assert_eq!(
        SandboxEngine::from_binary_name("nerdctl"),
        SandboxEngine::Docker,
        "unknown binary defaults to Docker"
    );
}

#[test]
fn sandbox_engine_binary_names() {
    assert_eq!(SandboxEngine::Docker.binary_name(), "docker");
    assert_eq!(SandboxEngine::Podman.binary_name(), "podman");
}

/// The pure-module discipline: sandbox_renderer.rs must not import
/// tokio::process, std::fs, or std::env.
#[test]
fn renderer_has_no_side_effect_imports() {
    let project_root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let source =
        std::fs::read_to_string(format!("{project_root}/src/executor/sandbox_renderer.rs"))
            .unwrap_or_else(|e| panic!("cannot read sandbox_renderer.rs: {e}"));
    for forbidden in ["use tokio::process", "use std::fs", "use std::env"] {
        assert!(
            !source.contains(forbidden),
            "sandbox_renderer.rs must not {forbidden}"
        );
    }
}

// ---------------------------------------------------------------------------
// PR-target `-e` fallback (DAR §6.1, D5)
// ---------------------------------------------------------------------------

/// The `-e` fallback list mirrors the resolved target: a PR-target
/// spec (detected via `CADUCEUS_WORK_TARGET=pr` in its environment)
/// renders only the PR-shaped fallbacks — never `CADUCEUS_ISSUE_*` /
/// `CADUCEUS_ISSUE_ID` / `CADUCEUS_BRANCH_NAME`.
#[test]
fn pr_target_e_fallback_renders_pr_vars_and_no_issue_vars() {
    let root = Path::new(ROOT);
    let cfg = Config::test_defaults(root);
    let worktree = root
        .join("workdirs")
        .join("owner")
        .join("repo")
        .join("run-pr-9");
    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: "run-pr-9".to_string(),
        target: "owner/repo#pr/9".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree: worktree.clone(),
        output_dir: cfg
            .state_dir
            .join("oci-runs")
            .join("run-pr-9")
            .join("output"),
        daemon_id: "test-daemon".to_string(),
        workdir_base: root.join("workdirs"),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: 4242,
        worktree_gid: 4242,
        engine_mode: EngineMode::Rootful,
        git_shadow_kind: GitShadowKind::File,
        git_shadow_host: cfg
            .state_dir
            .join("oci-runs")
            .join("run-pr-9")
            .join("git-shadow"),
    };
    let spec_input = caduceus::executor::ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::PullRequest(caduceus::review::ReviewTarget {
            repository: caduceus::review::RepositoryId {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
            },
            pull_request: 9,
            head_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            base_sha: "cafebabecafebabecafebabecafebabecafebabe".to_string(),
            base_ref: "main".to_string(),
            merge_base: "abcdef01abcdef01abcdef01abcdef01abcdef01".to_string(),
        }),
        worktree: worktree.clone(),
        run_id: "run-pr-9".to_string(),
        context_json: "{}".to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let resolved = caduceus::executor::sandbox_spec::resolve(cfg.sandbox(), &runtime, &spec_input)
        .expect("pr spec must resolve");
    // Render WITHOUT env files — the `-e` fallback surface (no
    // production caller, but the mode mirror must hold here too).
    let argv = render_with_env_files(&resolved, SandboxEngine::Docker, &[]);

    let has = |needle: &str| argv.iter().any(|arg| arg.contains(needle));
    for expected in [
        "CADUCEUS_WORK_TARGET=pr",
        "CADUCEUS_PR_NUMBER=9",
        "CADUCEUS_PR_REPO=owner/repo",
        "CADUCEUS_PR_BASE_SHA=cafebabecafebabecafebabecafebabecafebabe",
        "CADUCEUS_PR_HEAD_SHA=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "CADUCEUS_CONTEXT_JSON={}",
        "CADUCEUS_WORKTREE_PATH=/workspace",
        "CADUCEUS_RESULT_PATH=/output/worker-result.json",
    ] {
        assert!(has(expected), "PR fallback must carry {expected}: {argv:?}");
    }
    for forbidden in [
        "CADUCEUS_ISSUE_ID",
        "CADUCEUS_ISSUE_NUMBER",
        "CADUCEUS_ISSUE_REPO",
        "CADUCEUS_ISSUE_TITLE",
        "CADUCEUS_ISSUE_BODY",
        "CADUCEUS_ISSUE_LABELS_JSON",
        "CADUCEUS_BRANCH_NAME",
    ] {
        assert!(
            !has(forbidden),
            "PR fallback must not carry {forbidden}: {argv:?}"
        );
    }
}
