//! Tests for the OCI config fields and their validation in `Config::from_raw`.
//!
//! Covers: default values, enum loading, digest validation, timeout > 0 rules.

use std::path::Path;

use caduceus::infra::config::{Config, LoadContext, OciPullPolicy, RawConfig};

fn ctx(root: &Path) -> LoadContext {
    LoadContext {
        plugin_root: Some(root.to_path_buf()),
        ..Default::default()
    }
}

fn valid_raw(root: &Path) -> RawConfig {
    RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(root.to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        oci_image_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        ..Default::default()
    }
}

// oci_cli_docker_default

#[test]
fn oci_cli_docker_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = valid_raw(tmp.path());
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("config must load");
    assert_eq!(
        cfg.oci_cli.to_string_lossy(),
        "docker",
        "oci_cli must default to docker"
    );
}

// oci_cli_podman_loads

#[test]
fn oci_cli_podman_loads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        oci_cli: Some(std::path::PathBuf::from("podman")),
        oci_image_digest: Some(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        ),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("podman config must load");
    assert_eq!(
        cfg.oci_cli.to_string_lossy(),
        "podman",
        "oci_cli must be podman"
    );
}

// oci_image_digest_required

#[test]
fn oci_image_digest_required() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        executor_mode: Some(caduceus::executor::ExecutorKind::Oci),
        oci_image_digest: None,
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx(tmp.path()))
        .expect_err("empty digest must be rejected in OCI mode");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oci_image_digest"),
        "error must mention oci_image_digest; got: {msg}"
    );
}

// oci_image_digest_must_be_sha256

#[test]
fn oci_image_digest_must_be_sha256() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        executor_mode: Some(caduceus::executor::ExecutorKind::Oci),
        oci_image_digest: Some("not-a-digest".to_string()),
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx(tmp.path())).expect_err("invalid digest must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oci_image_digest") && msg.contains("sha256"),
        "error must mention sha256 requirement; got: {msg}"
    );
}

// oci_pull_policy_default_never

#[test]
fn oci_pull_policy_default_never() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = valid_raw(tmp.path());
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("config must load");
    assert_eq!(
        cfg.oci_pull_policy,
        OciPullPolicy::Never,
        "oci_pull_policy must default to Never"
    );
}

// oci_stop_timeout_must_be_positive

#[test]
fn oci_stop_timeout_must_be_positive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        oci_image_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        oci_stop_timeout_seconds: Some(0),
        ..Default::default()
    };
    let err =
        Config::from_raw(raw, &ctx(tmp.path())).expect_err("stop timeout of 0 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oci_stop_timeout_seconds"),
        "error must mention oci_stop_timeout_seconds; got: {msg}"
    );
}

// oci_kill_timeout_must_be_positive

#[test]
fn oci_kill_timeout_must_be_positive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        oci_image_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        oci_kill_timeout_seconds: Some(0),
        ..Default::default()
    };
    let err =
        Config::from_raw(raw, &ctx(tmp.path())).expect_err("kill timeout of 0 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oci_kill_timeout_seconds"),
        "error must mention oci_kill_timeout_seconds; got: {msg}"
    );
}

// oci_reconcile_timeout_must_be_positive

#[test]
fn oci_reconcile_timeout_must_be_positive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        oci_image_digest: Some(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        ),
        oci_reconcile_timeout_seconds: Some(0),
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx(tmp.path()))
        .expect_err("reconcile timeout of 0 must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("oci_reconcile_timeout_seconds"),
        "error must mention oci_reconcile_timeout_seconds; got: {msg}"
    );
}
