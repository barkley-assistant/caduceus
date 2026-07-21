//! Tests for the executor-mode Config schema and the
//! `reduced_containment_acknowledged` opt-in at `Config::load`.
//!
//! The opt-in test (SCN-03.1) asserts that the failure happens at
//! `Config::from_raw` BEFORE any subprocess is spawned. The Oci
//! runtime rejection test (SCN-03.3 setup) asserts that `executor_mode:
//! oci` parses in config but is rejected at `OciExecutor::run`.

use std::path::Path;

use caduceus::executor::ExecutorKind;
use caduceus::infra::config::{Config, LoadContext, RawConfig, RawEnv};

fn ctx(root: &Path) -> LoadContext {
    LoadContext {
        plugin_root: Some(root.to_path_buf()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// SCN-03.1: trusted_host_requires_opt_in
// ---------------------------------------------------------------------------

/// `Config::from_raw` with TrustedHost + unacknowledged returns
/// `Err(Config(...))` containing the opt-in message. The test never
/// calls `executor.run` — no subprocess is spawned.
#[test]
fn trusted_host_requires_opt_in() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        // executor_mode: defaults to TrustedHost
        // reduced_containment_acknowledged: defaults to None → false
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx(tmp.path())).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("reduced_containment_acknowledged"),
        "error must mention reduced_containment_acknowledged; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// SCN-03.2: trusted_host_with_opt_in_loads
// ---------------------------------------------------------------------------

/// `Config::from_raw` with TrustedHost + acknowledged returns `Ok`.
#[test]
fn trusted_host_with_opt_in_loads() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        executor_mode: Some(ExecutorKind::TrustedHost),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("must load");
    assert_eq!(cfg.executor_mode, ExecutorKind::TrustedHost);
    assert!(cfg.reduced_containment_acknowledged);
}

// ---------------------------------------------------------------------------
// SCN-03.6: oci_loads_cleanly
// ---------------------------------------------------------------------------

/// `Config::from_raw` with `executor_mode: oci` returns `Ok`. The Oci
/// stub is rejected at `OciExecutor::run`, not at config load.
#[test]
fn oci_loads_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        executor_mode: Some(ExecutorKind::Oci),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("Oci must load");
    assert_eq!(cfg.executor_mode, ExecutorKind::Oci);
}

// ---------------------------------------------------------------------------
// SCN-03.4: reduced_containment_ack_default_false
// ---------------------------------------------------------------------------

/// A `RawConfig` with no `reduced_containment_acknowledged` field
/// defaults to `false` in `Config` and therefore fails the opt-in
/// check.
#[test]
fn reduced_containment_ack_default_false() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        // No executor_mode and no reduced_containment_acknowledged
        ..Default::default()
    };
    let err = Config::from_raw(raw, &ctx(tmp.path())).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("reduced_containment_acknowledged"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// SCN-02.2: config_default_is_trusted_host
// ---------------------------------------------------------------------------

/// `Config::executor_mode` defaults to `TrustedHost` when the YAML
/// key is absent. The opt-in field is set so the config loads.
#[test]
fn config_default_is_trusted_host() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let raw = RawConfig {
        worker_command: Some(vec!["python3".to_string(), "bridge.py".to_string()]),
        state_dir: Some(tmp.path().to_path_buf()),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx(tmp.path())).expect("must load");
    assert_eq!(cfg.executor_mode, ExecutorKind::TrustedHost);
}

// ---------------------------------------------------------------------------
// SCN-02.3: test_defaults_loads_with_opt_in
// ---------------------------------------------------------------------------

/// `Config::test_defaults(root)` returns a valid `Config` with
/// `executor_mode == TrustedHost` and `reduced_containment_acknowledged
/// == true`. This is the canonical test fixture; existing tests rely
/// on it loading cleanly.
#[test]
fn test_defaults_loads_with_opt_in() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = Config::test_defaults(tmp.path());
    assert_eq!(cfg.executor_mode, ExecutorKind::TrustedHost);
    assert!(cfg.reduced_containment_acknowledged);
    // Round-trip through from_raw: a RawConfig derived from test_defaults
    // should load.
    let raw = RawConfig {
        worker_command: Some(cfg.worker_command.clone()),
        state_dir: Some(cfg.state_dir.clone()),
        reduced_containment_acknowledged: Some(true),
        executor_mode: Some(cfg.executor_mode),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx(tmp.path())).expect("test_defaults must round-trip");
}

// ---------------------------------------------------------------------------
// SCN-02.4: executor_kind_yaml_round_trip
// ---------------------------------------------------------------------------

/// `ExecutorKind` serializes to snake_case and deserializes from
/// snake_case. This is the YAML contract operators write.
#[test]
fn executor_kind_yaml_round_trip() {
    // TrustedHost <-> "trusted_host"
    let yaml = serde_yaml::to_string(&ExecutorKind::TrustedHost).expect("serialize");
    assert!(
        yaml.contains("trusted_host"),
        "TrustedHost must serialize to trusted_host; got: {yaml}"
    );
    let parsed: ExecutorKind = serde_yaml::from_str("trusted_host").expect("parse trusted_host");
    assert_eq!(parsed, ExecutorKind::TrustedHost);

    // Oci <-> "oci"
    let yaml = serde_yaml::to_string(&ExecutorKind::Oci).expect("serialize");
    assert!(
        yaml.contains("oci"),
        "Oci must serialize to oci; got: {yaml}"
    );
    let parsed: ExecutorKind = serde_yaml::from_str("oci").expect("parse oci");
    assert_eq!(parsed, ExecutorKind::Oci);
}

// ---------------------------------------------------------------------------
// load_with_context surfaces the same error
// ---------------------------------------------------------------------------

/// `Config::load_with_context` (the production entry point) surfaces
/// the same `reduced_containment_acknowledged` error when the
/// underlying YAML lacks the field.
#[test]
fn load_with_context_rejects_unacknowledged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config_path = tmp.path().join("config.yaml");
    std::fs::write(
        &config_path,
        r#"
worker_command: ["python3", "bridge.py"]
"#,
    )
    .expect("write yaml");
    let env = RawEnv {
        caduceus_config: Some(config_path.to_string_lossy().to_string()),
        hermes_home: None,
        caduceus_dry_run: None,
    };
    let err = Config::load_with_context(&env).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("reduced_containment_acknowledged"),
        "load_with_context error must surface the opt-in requirement; got: {msg}"
    );
}
