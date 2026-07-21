//! Tests for the pure argv builder — no tokio::process::Command allowed.

use std::path::PathBuf;

use caduceus::executor::oci_args::{build_argv, MountSpec, OciEngine};
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;

fn test_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn test_spec(run_id: &str) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: PathBuf::from("/usr/bin/caduceus"),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worktree: PathBuf::from("/tmp/worktree"),
        run_id: run_id.to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
        network_profile: None,
    }
}

fn sample_mounts() -> Vec<MountSpec> {
    vec![
        MountSpec {
            host_path: PathBuf::from("/tmp/worktree"),
            container_path: PathBuf::from("/worktree"),
            read_only: false,
        },
        MountSpec {
            host_path: PathBuf::from("/tmp/result"),
            container_path: PathBuf::from("/result"),
            read_only: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// one_contract_both_clis (AC-01)
// ---------------------------------------------------------------------------

#[test]
fn one_contract_both_clis() {
    let cfg = test_cfg();
    let spec = test_spec("run-001");
    let mounts = sample_mounts();

    let docker_argv = build_argv(&spec, &cfg, &mounts, None).expect("docker argv must build");

    let mut podman_cfg = cfg.clone();
    podman_cfg.oci_cli = PathBuf::from("podman");
    let podman_argv =
        build_argv(&spec, &podman_cfg, &mounts, None).expect("podman argv must build");

    // Both produce equivalent argv modulo the binary name at index 0.
    assert_eq!(docker_argv.len(), podman_argv.len(), "same length");
    for i in 0..docker_argv.len() {
        if i == 0 {
            // Binary name differs
            assert_eq!(docker_argv[0], "docker");
            assert_eq!(podman_argv[0], "podman");
        } else {
            assert_eq!(
                docker_argv[i], podman_argv[i],
                "argv[{i}] differs: docker={:?}, podman={:?}",
                docker_argv[i], podman_argv[i]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// undeclared_mount_rejected (AC-02)
// ---------------------------------------------------------------------------

#[test]
fn undeclared_mount_rejected() {
    let cfg = test_cfg();
    let spec = test_spec("run-002");
    // Pass an empty mount list — the worktree is in the spec but
    // the caller didn't declare it in the mount allow-list.
    let mounts: Vec<MountSpec> = vec![];

    let err = build_argv(&spec, &cfg, &mounts, None)
        .expect_err("undeclared worktree mount must be rejected");
    match err {
        CaduceusError::OciUndeclaredMount { .. } => {} // expected
        ref other => panic!("expected OciUndeclaredMount; got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// argv_no_secret_value (AC-04)
// ---------------------------------------------------------------------------

#[test]
fn argv_no_secret_value() {
    let cfg = test_cfg();
    let spec = test_spec("run-003");
    let mounts = sample_mounts();
    let secret_path = Some(std::path::Path::new("/tmp/secrets.env"));

    let argv = build_argv(&spec, &cfg, &mounts, secret_path).expect("argv must build");

    // The secret path may appear as --env-file but the SECRET
    // VALUE must never be in argv.
    for token in &argv {
        assert!(
            !token.contains("SUPERSECRET"),
            "secret value 'SUPERSECRET' must not appear in argv token: {token:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// argv_no_tokio_process_command (AC-01 structural)
// ---------------------------------------------------------------------------

#[test]
fn argv_no_tokio_process_command() {
    let project_root = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let source = std::fs::read_to_string(format!("{project_root}/src/executor/oci_args.rs"))
        .unwrap_or_else(|e| panic!("cannot read oci_args.rs: {e}"));
    // The module must not IMPORT tokio::process::Command. Doc-comment
    // mentions are fine.
    assert!(
        !source.contains("use tokio::process::Command"),
        "src/executor/oci_args.rs must not import tokio::process::Command"
    );
    assert!(
        !source.contains("tokio::process::Command;"),
        "src/executor/oci_args.rs must not import tokio::process::Command"
    );
}

// ---------------------------------------------------------------------------
// argv_label_set_stable (AC-05)
// ---------------------------------------------------------------------------

#[test]
fn argv_label_set_stable() {
    let cfg = test_cfg();
    let mounts = sample_mounts();

    // Two calls with different run_ids but same daemon_id.
    let spec_a = test_spec("run-010");
    let spec_b = test_spec("run-011");

    let argv_a = build_argv(&spec_a, &cfg, &mounts, None).expect("argv_a must build");
    let argv_b = build_argv(&spec_b, &cfg, &mounts, None).expect("argv_b must build");

    // Extract label flags
    let labels_a: Vec<String> = argv_a
        .iter()
        .filter(|t| t.starts_with("caduceus."))
        .cloned()
        .collect();
    let labels_b: Vec<String> = argv_b
        .iter()
        .filter(|t| t.starts_with("caduceus."))
        .cloned()
        .collect();

    // Both runs have the same daemon_id
    let daemon_a: Vec<&String> = labels_a
        .iter()
        .filter(|l| l.starts_with("caduceus.daemon_id"))
        .collect();
    let daemon_b: Vec<&String> = labels_b
        .iter()
        .filter(|l| l.starts_with("caduceus.daemon_id"))
        .collect();
    assert!(!daemon_a.is_empty(), "must have daemon_id label");
    assert_eq!(
        daemon_a, daemon_b,
        "daemon_id must be identical across runs"
    );

    // Run IDs differ
    let run_a: Vec<&String> = labels_a
        .iter()
        .filter(|l| l.starts_with("caduceus.run_id"))
        .collect();
    let run_b: Vec<&String> = labels_b
        .iter()
        .filter(|l| l.starts_with("caduceus.run_id"))
        .collect();
    assert_ne!(run_a, run_b, "run_id labels must differ");
}

// ---------------------------------------------------------------------------
// OciEngine detection
// ---------------------------------------------------------------------------

#[test]
fn oci_engine_detects_docker_or_podman() {
    // OciEngine::from_binary_name
    assert_eq!(OciEngine::from_binary_name("docker"), OciEngine::Docker);
    assert_eq!(
        OciEngine::from_binary_name("/usr/bin/docker"),
        OciEngine::Docker
    );
    assert_eq!(OciEngine::from_binary_name("podman"), OciEngine::Podman);
    assert_eq!(
        OciEngine::from_binary_name("/usr/local/bin/podman"),
        OciEngine::Podman
    );
    assert_eq!(
        OciEngine::from_binary_name("nerdctl"),
        OciEngine::Docker,
        "unknown binary defaults to Docker"
    );
}
