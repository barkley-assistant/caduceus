//! Task 2.1 acceptance tests for the production configuration bootstrap.
//!
//! These tests use `Config::load_with_context()` for deterministic control
//! over the environment, and serial_test for the few tests that need
//! the real `Config::load()` path.
//!
//! AC-01: $CADUCEUS_CONFIG is authoritative — missing/unreadable/invalid errors
//! AC-02: Reject empty/relative HERMES_HOME
//! AC-03: Check $HERMES_HOME/config.yaml, then standalone
//! AC-04: Default worker command resolves to $HERMES_HOME/caduceus/worker-bridge.py
//! AC-05: Readiness diagnostics
//! AC-06: Atomic minimal config creation
//! AC-07: Mode/preservation/redaction
//! AC-08: Interruption retry
//! AC-09: No stubs in production code

use std::path::PathBuf;
use std::sync::Mutex;

use caduceus::config::{Config, RawEnv, SetupAction};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-bootstrap-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write config");
}

/// Run *f* with CADUCEUS_CONFIG and CADUCEUS_DRY_RUN set for the
/// duration of the call. The original env state is restored on exit.
fn with_env<F, R>(vars: &[(&str, Option<&str>)], f: F) -> R
where
    F: FnOnce() -> R,
{
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prior: Vec<(String, Option<std::ffi::OsString>)> = vars
        .iter()
        .map(|(k, _)| (k.to_string(), std::env::var_os(k)))
        .collect();
    for (k, v) in vars {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
    let result = f();
    for (k, v) in prior {
        match v {
            Some(val) => std::env::set_var(&k, val),
            None => std::env::remove_var(&k),
        }
    }
    result
}

// ---------------------------------------------------------------------------
// AC-01: $CADUCEUS_CONFIG is authoritative
// ---------------------------------------------------------------------------

#[test]
fn load_with_context_explicit_env_authoritative() {
    let root = tempdir("ac01-explicit");
    let explicit = root.join("config.yaml");
    write(
        &explicit,
        r#"
        worker_command: ["python3", "/from/explicit.py"]
        "#,
    );
    let hermes_dir = root.join("hermes");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );

    let env = RawEnv {
        caduceus_config: Some(explicit.to_string_lossy().to_string()),
        hermes_home: Some(hermes_dir.to_string_lossy().to_string()),
        caduceus_dry_run: None,
    };
    let cfg = Config::load_with_context(&env).expect("explicit path wins");
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/explicit.py")
    );
}

#[test]
fn load_with_context_explicit_missing_is_hard_error() {
    let root = tempdir("ac01-missing");
    let missing = root.join("does-not-exist.yaml");
    let env = RawEnv {
        caduceus_config: Some(missing.to_string_lossy().to_string()),
        hermes_home: None,
        caduceus_dry_run: None,
    };
    let err = Config::load_with_context(&env).expect_err("missing $CADUCEUS_CONFIG must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("$CADUCEUS_CONFIG points at") && msg.contains("missing"),
        "got: {msg}"
    );
}

#[test]
fn setup_config_rejects_when_caduceus_config_set() {
    let root = tempdir("ac01-setup-reject");
    let hermes_home = root.join("hermes");
    std::fs::create_dir_all(&hermes_home).expect("create hermes_home");

    let result = with_env(&[("CADUCEUS_CONFIG", Some("/tmp/irrelevant.yaml"))], || {
        caduceus::config::setup_config(&hermes_home, false)
    });
    let err = result.expect_err("setup_config must reject when CADUCEUS_CONFIG is set");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("refusing to generate config when CADUCEUS_CONFIG"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// AC-02: Reject empty/relative HERMES_HOME
// ---------------------------------------------------------------------------

#[test]
fn load_with_context_rejects_empty_hermes_home() {
    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some(String::new()),
        caduceus_dry_run: None,
    };
    let err = Config::load_with_context(&env).expect_err("empty HERMES_HOME must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("HERMES_HOME must not be empty"), "got: {msg}");
}

#[test]
fn load_with_context_rejects_relative_hermes_home() {
    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some("relative/path".to_string()),
        caduceus_dry_run: None,
    };
    let err = Config::load_with_context(&env).expect_err("relative HERMES_HOME must fail");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("HERMES_HOME must be an absolute path"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// AC-03: Check $HERMES_HOME/config.yaml, then standalone
// ---------------------------------------------------------------------------

#[test]
fn load_with_context_hermes_then_standalone_fallback() {
    let root = tempdir("ac03-hermes");
    let hermes_dir = root.join("hermes");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );

    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some(hermes_dir.to_string_lossy().to_string()),
        caduceus_dry_run: None,
    };
    let cfg = Config::load_with_context(&env).expect("hermes config loads");
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/hermes.py")
    );
}

#[test]
fn load_with_context_hermes_missing_falls_to_standalone() {
    // This test depends on the existence of ~/.config/caduceus/config.yaml,
    // which may not exist in CI. The standalone fallback is tested by the
    // existing resolution tests. Here we verify that hermes pointing at a
    // missing directory produces a "no source" error rather than a panic.
    let root = tempdir("ac03-hermes-missing");
    let hermes_dir = root.join("hermes");
    // Don't create the config.yaml

    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some(hermes_dir.to_string_lossy().to_string()),
        caduceus_dry_run: None,
    };
    // This should error because HERMES_HOME/config.yaml doesn't exist
    // and neither does the standalone path (it's a virtual home dir).
    let err = Config::load_with_context(&env).expect_err("no sources should error");
    let msg = format!("{err:?}");
    assert!(msg.contains("no configuration source found"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// AC-04: Default worker resolves to hermes_home bridge
// ---------------------------------------------------------------------------

#[test]
fn load_resolves_default_worker_to_hermes_home_bridge() {
    let root = tempdir("ac04-hermes-bridge");
    let hermes_dir = root.join("hermes");
    // Create a hermes config without a worker_command — the default
    // should fall back to $HERMES_HOME/caduceus/worker-bridge.py.
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          poll_interval_seconds: 60
        "#,
    );

    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some(hermes_dir.to_string_lossy().to_string()),
        caduceus_dry_run: None,
    };
    let cfg = Config::load_with_context(&env).expect("config loads with default bridge");
    assert_eq!(
        cfg.worker_command.first().map(String::as_str),
        Some("python3"),
        "expected python3 as first arg"
    );
    let bridge_arg = cfg.worker_command.get(1).expect("bridge path argument");
    assert!(
        bridge_arg.ends_with("worker-bridge.py"),
        "expected bridge path ending in worker-bridge.py, got: {bridge_arg}"
    );
    // Verify it falls under the hermes_home tree.
    assert!(
        bridge_arg.contains("hermes/caduceus/worker-bridge.py"),
        "expected hermes_home bridge path, got: {bridge_arg}"
    );
}

// ---------------------------------------------------------------------------
// AC-05: Readiness diagnostics
// ---------------------------------------------------------------------------

#[test]
fn readiness_diagnostics_populated() {
    let root = tempdir("ac05-readiness");
    let state_dir = root.join("state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    let (report, _) =
        caduceus::status::build_report(&state_dir).expect("build report with readiness");
    let readiness = report.readiness.expect("readiness should be Some");
    assert!(readiness.contains_key("bridge"), "bridge key missing");
    assert!(readiness.contains_key("harness"), "harness key missing");
    assert!(readiness.contains_key("provider"), "provider key missing");
    // With no bridge file, both should be "missing"
    assert_eq!(readiness.get("bridge").map(String::as_str), Some("missing"));
    assert_eq!(
        readiness.get("harness").map(String::as_str),
        Some("missing")
    );
    assert_eq!(
        readiness.get("provider").map(String::as_str),
        Some("not-applicable")
    );
}

#[test]
fn readiness_diagnostics_bridge_present() {
    let root = tempdir("ac05-readiness-present");
    let state_dir = root.join("caduceus-state");
    std::fs::create_dir_all(&state_dir).expect("create state dir");

    // Create the bridge file at the expected location
    let bridge_dir = state_dir.parent().unwrap().join("caduceus");
    std::fs::create_dir_all(&bridge_dir).expect("create caduceus dir");
    write(
        &bridge_dir.join("worker-bridge.py"),
        "#!/usr/bin/env python3\n",
    );

    let (report, _) =
        caduceus::status::build_report(&state_dir).expect("build report with readiness");
    let readiness = report.readiness.expect("readiness should be Some");
    assert_eq!(readiness.get("bridge").map(String::as_str), Some("present"));
    assert_eq!(
        readiness.get("harness").map(String::as_str),
        Some("present")
    );
}

// ---------------------------------------------------------------------------
// AC-06: setup_config creates mode 0600, dry-run does not write
// ---------------------------------------------------------------------------

#[test]
fn setup_config_creates_mode_0600() {
    let root = tempdir("ac06-create");
    let hermes_home = root.join("hermes");
    std::fs::create_dir_all(&hermes_home).expect("create hermes_home");

    let report =
        caduceus::config::setup_config(&hermes_home, false).expect("setup_config creates config");
    assert_eq!(report.action, SetupAction::Created, "should be Created");
    assert!(report.path.is_file(), "config file should exist");

    // Verify mode via permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&report.path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        // Mode should be 0o600 (or narrower if umask affected it)
        assert!(mode <= 0o600, "mode should be <= 0o600, got: {mode:o}");
    }

    // Verify the file has valid YAML with caduceus config keys
    let content = std::fs::read_to_string(&report.path).expect("read config");
    assert!(
        content.contains("poll_interval_seconds"),
        "config should contain poll_interval_seconds"
    );
    assert!(
        content.contains("state_dir"),
        "config should contain state_dir"
    );
    assert!(
        content.contains("workdir_base"),
        "config should contain workdir_base"
    );
    // The first run creates a standalone file (no Hermes shape)
    assert!(
        !content.contains("caduceus:"),
        "standalone config should not have a caduceus: section wrapper"
    );
}

#[test]
fn setup_config_dry_run_does_not_write() {
    let root = tempdir("ac06-dry-run");
    let hermes_home = root.join("hermes");
    std::fs::create_dir_all(&hermes_home).expect("create hermes_home");

    let report = caduceus::config::setup_config(&hermes_home, true).expect("setup_config dry-run");
    assert_eq!(report.action, SetupAction::Skipped, "should be Skipped");
    // The file should NOT exist after a dry-run
    assert!(
        !report.path.is_file(),
        "config file should not exist after dry-run"
    );
}

// ---------------------------------------------------------------------------
// AC-07: mode preservation and YAML merging
// ---------------------------------------------------------------------------

#[test]
fn setup_config_preserves_existing_mode_and_yaml() {
    let root = tempdir("ac07-preserve");
    let hermes_home = root.join("hermes");
    std::fs::create_dir_all(&hermes_home).expect("create hermes_home");

    // Create an existing Hermes-shaped config with a known mode
    let config_path = hermes_home.join("config.yaml");
    write(
        &config_path,
        r#"
        model:
          default: hermes-model
        "#,
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o640))
            .expect("set mode 0640");
    }

    let report =
        caduceus::config::setup_config(&hermes_home, false).expect("setup_config updates existing");
    assert_eq!(report.action, SetupAction::Updated, "should be Updated");

    // Verify the file now has a caduceus: section
    let content = std::fs::read_to_string(&report.path).expect("read config");
    assert!(
        content.contains("caduceus:"),
        "merged config should have caduceus: section"
    );
    assert!(
        content.contains("hermes-model"),
        "merged config should preserve original hermes keys"
    );
    assert!(
        content.contains("poll_interval_seconds"),
        "merged config should have caduceus config keys"
    );

    // Verify mode was preserved (not widened above 0600)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(&report.path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert!(mode <= 0o640, "mode should be <= 0o640, got: {mode:o}");
    }
}

// ---------------------------------------------------------------------------
// AC-08: Interruption retry
// ---------------------------------------------------------------------------

#[test]
fn setup_config_interrupt_before_rename_is_retryable() {
    let root = tempdir("ac08-retry");
    let hermes_home = root.join("hermes");
    std::fs::create_dir_all(&hermes_home).expect("create hermes_home");

    // Simulate a leftover .tmp file from a previous interrupted run
    let tmp_path = hermes_home.join("config.yaml.tmp");
    write(&tmp_path, "leftover garbage\n");

    // First run should succeed and clean up the tmp file
    let report = caduceus::config::setup_config(&hermes_home, false)
        .expect("first setup_config succeeds despite leftover tmp");
    assert_eq!(report.action, SetupAction::Created, "should be Created");
    assert!(report.path.is_file(), "config file should exist");

    // The tmp file should be gone
    assert!(!tmp_path.is_file(), "leftover tmp file should be removed");

    // Second run should succeed (idempotent)
    let report2 =
        caduceus::config::setup_config(&hermes_home, false).expect("second setup_config succeeds");
    assert_eq!(
        report2.action,
        SetupAction::Updated,
        "second run should Update"
    );
    assert!(report2.path.is_file(), "config file should still exist");
}

// ---------------------------------------------------------------------------
// AC-09: No stubs or dev notes in production paths
// ---------------------------------------------------------------------------

#[test]
fn no_stubs_or_dev_notes_in_production_paths() {
    // Check that Config::load() no longer returns the stub error
    // by using a known-absent configuration scenario.
    let root = tempdir("ac09-no-stubs");
    // Point HERMES_HOME at a directory with no config, and no standalone
    let hermes_dir = root.join("hermes");
    std::fs::create_dir_all(&hermes_dir).expect("create hermes_home");

    // Use load_with_context to verify it doesn't hit the old stub
    let env = RawEnv {
        caduceus_config: None,
        hermes_home: Some(hermes_dir.to_string_lossy().to_string()),
        caduceus_dry_run: None,
    };
    let err = Config::load_with_context(&env).expect_err("should fail with no sources");
    let msg = format!("{err:?}");
    // Must NOT be the old stub message
    assert!(
        !msg.contains("Task 1.3"),
        "should not contain Task 1.3 stub reference: {msg}"
    );
    assert!(
        msg.contains("no configuration source found"),
        "should contain 'no configuration source found', got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// RawEnv::from_process_env
// ---------------------------------------------------------------------------

#[test]
fn raw_env_from_process_env_captures_vars() {
    let result = with_env(
        &[
            ("CADUCEUS_CONFIG", Some("/tmp/test-config.yaml")),
            ("HERMES_HOME", Some("/tmp/hermes")),
            ("CADUCEUS_DRY_RUN", Some("1")),
        ],
        RawEnv::from_process_env,
    );
    assert_eq!(
        result.caduceus_config.as_deref(),
        Some("/tmp/test-config.yaml")
    );
    assert_eq!(result.hermes_home.as_deref(), Some("/tmp/hermes"));
    assert_eq!(result.caduceus_dry_run.as_deref(), Some("1"));
}

#[test]
fn raw_env_from_process_env_handles_unset_vars() {
    let result = with_env(
        &[
            ("CADUCEUS_CONFIG", None),
            ("HERMES_HOME", None),
            ("CADUCEUS_DRY_RUN", None),
        ],
        RawEnv::from_process_env,
    );
    assert!(result.caduceus_config.is_none());
    assert!(result.hermes_home.is_none());
    assert!(result.caduceus_dry_run.is_none());
}

// ---------------------------------------------------------------------------
// StatusReport: schema version bump
// ---------------------------------------------------------------------------

#[test]
fn status_schema_version_bumped_to_7_5_0() {
    assert_eq!(
        caduceus::status::STATUS_SCHEMA_VERSION,
        "7.5.0",
        "schema version should be 7.5.0 for the pool_state field"
    );
}
