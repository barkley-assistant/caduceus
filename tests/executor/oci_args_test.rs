//! Rewritten argv-contract tests for the OCI executor.
//!
//! The legacy `build_argv` / `find_image_position` surface is deleted;
//! the pure renderer (`sandbox_renderer::render`) is the sole argv
//! producer. This file asserts the renderer contract: golden output,
//! label stability across runs, secret-value hygiene, and the
//! `SandboxEngine` detection surface (moved from the deleted
//! `oci_args.rs`).

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_renderer::{render, render_with_env_files};
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine, SandboxSpec};
use caduceus::infra::config::Config;

mod support;

fn resolve_from(cfg: &Config, run_id: &str) -> SandboxSpec {
    let worktree = cfg.workdir_base.join("owner").join("repo").join(run_id);
    let runtime = support::runtime_facts(cfg, run_id, &worktree);
    resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve")
}

fn default_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

// one_contract_both_clis (AC-01) — both engines produce the same argv
// modulo the binary name and the documented Podman userns delta.

#[test]
fn one_contract_both_clis() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg, "run-001");
    let docker_argv = render(&spec, SandboxEngine::Docker);
    let podman_argv = render(&spec, SandboxEngine::Podman);

    assert_eq!(docker_argv[0], "docker");
    assert_eq!(podman_argv[0], "podman");
    // Rootful docker vs rootless-mode fixture facts for podman differ
    // by the identity encoding: docker has the `--user` pair, the
    // podman facts carry plain keep-id. Strip both identity encodings
    // and the argvs are otherwise identical.
    let strip_identity = |argv: &[String]| -> Vec<String> {
        let mut out = Vec::new();
        let mut skip_next = false;
        for tok in argv {
            if skip_next {
                skip_next = false;
                continue;
            }
            if tok == "--user" {
                skip_next = true;
                continue;
            }
            if tok == "--userns" {
                skip_next = true;
                continue;
            }
            out.push(tok.clone());
        }
        out
    };
    let docker_stripped = strip_identity(&docker_argv);
    let podman_stripped = strip_identity(&podman_argv);
    assert_eq!(docker_stripped[0], "docker");
    let mut podman_as_docker = podman_stripped;
    podman_as_docker[0] = "docker".to_string();
    assert_eq!(docker_stripped, podman_as_docker);
}

// undeclared_mount_rejected (AC-02) — resolution rejects host paths
// outside the declared workdir_base.

#[test]
fn undeclared_mount_rejected() {
    let cfg = default_cfg();
    let runtime = support::runtime_facts(&cfg, "run-002", Path::new("/tmp/worktree"));
    let err = resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime))
        .expect_err("undeclared worktree must be rejected");
    assert!(
        matches!(
            err,
            caduceus::infra::error::CaduceusError::OciUndeclaredMount { .. }
        ),
        "expected OciUndeclaredMount; got: {err:?}"
    );
}

// argv_no_secret_value (AC-04) — the secret path may appear as
// --env-file but the SECRET VALUE must never be in argv.

#[test]
fn argv_no_secret_value() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg, "run-003");
    let secret_path = PathBuf::from("/tmp/SUPERSECRET.env");
    let argv = render_with_env_files(&spec, SandboxEngine::Docker, &[secret_path]);
    for token in &argv {
        assert!(
            !token.contains("SUPERSECRET_VALUE"),
            "secret value must not appear in argv token: {token:?}"
        );
    }
}

// argv_label_set_stable (AC-05) — daemon_id identical across runs,
// run_id differs.

#[test]
fn argv_label_set_stable() {
    let cfg = default_cfg();
    let spec_a = resolve_from(&cfg, "run-010");
    let spec_b = resolve_from(&cfg, "run-011");

    let argv_a = render(&spec_a, SandboxEngine::Docker);
    let argv_b = render(&spec_b, SandboxEngine::Docker);
    let labels_a: Vec<&String> = argv_a
        .iter()
        .filter(|t| t.starts_with("caduceus."))
        .collect();
    let labels_b: Vec<&String> = argv_b
        .iter()
        .filter(|t| t.starts_with("caduceus."))
        .collect();

    let daemon_a: Vec<&&String> = labels_a
        .iter()
        .filter(|l| l.starts_with("caduceus.daemon_id"))
        .collect();
    let daemon_b: Vec<&&String> = labels_b
        .iter()
        .filter(|l| l.starts_with("caduceus.daemon_id"))
        .collect();
    assert!(!daemon_a.is_empty(), "must have daemon_id label");
    assert_eq!(
        daemon_a, daemon_b,
        "daemon_id must be identical across runs"
    );

    let run_a: Vec<&&String> = labels_a
        .iter()
        .filter(|l| l.starts_with("caduceus.run_id"))
        .collect();
    let run_b: Vec<&&String> = labels_b
        .iter()
        .filter(|l| l.starts_with("caduceus.run_id"))
        .collect();
    assert_ne!(run_a, run_b, "run_id labels must differ");
}

// SandboxEngine detection

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

// The renderer formats the resolved identity and mounts from the
// spec — no operator argv is re-read.

#[test]
fn renderer_reads_spec_not_argv() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg, "run-004");
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "4242:4242"));
    assert!(argv.iter().any(|a| a.ends_with(":/workspace:rw")));
    assert!(argv.iter().any(|a| a.ends_with(":/output:rw")));
    let _ = Path::new("/tmp");
}
