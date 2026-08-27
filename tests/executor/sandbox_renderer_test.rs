//! Golden argv snapshot tests for the pure deterministic renderer.
//!
//! The goldens are literal expected `Vec<String>` values. Because
//! `SandboxSpec` is closed (the only constructor is `resolve`), the
//! fixtures resolve a fixed `Config` + `RuntimeFacts` first; `resolve`
//! does no I/O, so the fixed host paths never need to exist and the
//! goldens are byte-for-byte deterministic.

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_renderer::{render, render_with_env_files};
use caduceus::executor::sandbox_spec::{RuntimeFacts, SandboxEngine, SandboxSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, SandboxConfig, SandboxNetwork};

/// Fixed root for the golden fixtures. Never touched on disk.
const ROOT: &str = "/tmp/caduceus-renderer-goldens";

/// Resolve a fixture spec from `Config::test_defaults` with an
/// optional sandbox mutation. Returns the spec plus the host paths
/// the goldens interpolate into mount args.
fn fixture(
    run_id: &str,
    mutate: impl FnOnce(&mut SandboxConfig),
) -> (SandboxSpec, PathBuf, PathBuf, String) {
    let root = Path::new(ROOT);
    let mut cfg = Config::test_defaults(root);
    mutate(cfg.sandbox.as_mut().expect("test_defaults has a sandbox"));
    let worktree = root
        .join("workdirs")
        .join("owner")
        .join("repo")
        .join(run_id);
    let output = root
        .join("workdirs")
        .join("owner")
        .join("repo")
        .join("result");
    let runtime = RuntimeFacts {
        run_id: run_id.to_string(),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree: worktree.clone(),
        output_dir: output.clone(),
        daemon_id: "state".to_string(),
        workdir_base: root.join("workdirs"),
    };
    let spec = caduceus::executor::sandbox_spec::resolve(cfg.sandbox(), &runtime)
        .expect("fixture must resolve");
    (spec, worktree, output, cfg.sandbox().image.clone())
}

/// The default fixture with no sandbox mutation.
fn default_fixture() -> (SandboxSpec, PathBuf, PathBuf, String) {
    fixture("run-001", |_| {})
}

// ---------------------------------------------------------------------------
// Golden snapshots — one per engine, byte-for-byte.
// ---------------------------------------------------------------------------

#[test]
fn docker_golden_argv() {
    let (spec, worktree, output, image) = default_fixture();
    let expected = vec![
        "docker".to_string(),
        "create".to_string(),
        "--user".to_string(),
        "1000:1000".to_string(),
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
        "--pids-limit".to_string(),
        "256".to_string(),
        "--shm-size".to_string(),
        "64m".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=state".to_string(),
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
fn podman_golden_argv() {
    let (spec, worktree, output, image) = default_fixture();
    let expected = vec![
        "podman".to_string(),
        "create".to_string(),
        "--user".to_string(),
        "1000:1000".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--read-only".to_string(),
        "--userns".to_string(),
        "keep-id:uid=1000,gid=1000".to_string(),
        "--network".to_string(),
        "none".to_string(),
        "--cpus".to_string(),
        "2".to_string(),
        "--memory".to_string(),
        "2048m".to_string(),
        "--pids-limit".to_string(),
        "256".to_string(),
        "--shm-size".to_string(),
        "64m".to_string(),
        "--name".to_string(),
        "run-001".to_string(),
        "-v".to_string(),
        format!("{}:/workspace:rw", worktree.display()),
        "-v".to_string(),
        format!("{}:/output:rw", output.display()),
        "--tmpfs".to_string(),
        "/tmp:size=256m".to_string(),
        "-e".to_string(),
        "CADUCEUS_RUN_ID=run-001".to_string(),
        "-e".to_string(),
        "CADUCEUS_ISSUE_ID=owner/repo#1".to_string(),
        "-l".to_string(),
        "caduceus.daemon_id=state".to_string(),
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

/// The Podman output differs from Docker ONLY by the binary name and
/// the inserted `--userns keep-id:uid=1000,gid=1000` pair.
#[test]
fn podman_delta_is_only_userns_and_binary() {
    let (spec, _, _, _) = default_fixture();
    let docker = render(&spec, SandboxEngine::Docker);
    let podman = render(&spec, SandboxEngine::Podman);

    assert_eq!(
        docker.len() + 2,
        podman.len(),
        "podman adds exactly 2 tokens"
    );
    let mut podman_expected = docker.clone();
    podman_expected[0] = "podman".to_string();
    // Insert the userns pair immediately after --read-only and before
    // --network (the documented delta position).
    podman_expected.splice(
        9..9,
        [
            "--userns".to_string(),
            "keep-id:uid=1000,gid=1000".to_string(),
        ],
    );
    assert_eq!(podman, podman_expected);
}

/// Determinism contract: two invocations on the same inputs are
/// byte-identical.
#[test]
fn render_is_deterministic() {
    let (spec, _, _, _) = default_fixture();
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
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--user"));
    assert!(argv.iter().any(|a| a == "1000:1000"));
}

#[test]
fn renders_workspace_mount() {
    let (spec, worktree, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "-v"));
    assert!(argv
        .iter()
        .any(|a| a == &format!("{}:/workspace:rw", worktree.display())));
    // Exactly one workspace mount and one output mount — the
    // double-RW bug is gone.
    let mounts: Vec<&String> = argv
        .iter()
        .filter(|a| a.contains("/workspace:") || a.contains("/output:"))
        .collect();
    assert_eq!(mounts.len(), 2, "exactly two mounts, got: {mounts:?}");
}

#[test]
fn renders_output_mount() {
    let (spec, _, output, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv
        .iter()
        .any(|a| a == &format!("{}:/output:rw", output.display())));
}

#[test]
fn renders_tmpfs() {
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--tmpfs"));
    assert!(argv.iter().any(|a| a == "/tmp:size=256m"));
}

#[test]
fn renders_environment() {
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "-e"));
    assert!(argv.iter().any(|a| a == "CADUCEUS_RUN_ID=run-001"));
    assert!(argv.iter().any(|a| a == "CADUCEUS_ISSUE_ID=owner/repo#1"));
}

#[test]
fn renders_env_files_in_slice_order() {
    let (spec, _, _, _) = default_fixture();
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

#[test]
fn renders_labels_in_fixed_order() {
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let labels: Vec<&String> = argv.iter().filter(|a| a.starts_with("caduceus.")).collect();
    assert_eq!(
        labels,
        vec![
            &"caduceus.daemon_id=state".to_string(),
            &"caduceus.run_id=run-001".to_string(),
            &"caduceus.issue_id=owner/repo#1".to_string(),
        ]
    );
}

#[test]
fn renders_resources() {
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--cpus"));
    assert!(argv.iter().any(|a| a == "2"));
    assert!(argv.iter().any(|a| a == "--memory"));
    assert!(argv.iter().any(|a| a == "2048m"));
    assert!(argv.iter().any(|a| a == "--pids-limit"));
    assert!(argv.iter().any(|a| a == "256"));
    assert!(argv.iter().any(|a| a == "--shm-size"));
    assert!(argv.iter().any(|a| a == "64m"));
}

#[test]
fn renders_network_none_by_default() {
    let (spec, _, _, _) = default_fixture();
    let argv = render(&spec, SandboxEngine::Docker);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[pos + 1], "none");
}

#[test]
fn renders_network_host_for_unrestricted() {
    let (spec, _, _, _) = fixture("run-001", |sb| {
        sb.network = SandboxNetwork::Unrestricted;
    });
    let argv = render(&spec, SandboxEngine::Docker);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[pos + 1], "host");
}

#[test]
fn renders_entrypoint_image_and_worker_args() {
    let (spec, _, _, image) = default_fixture();
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
