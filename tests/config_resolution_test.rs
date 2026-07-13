//! Task 1.3 acceptance tests for config resolution.
//!
//! These tests use the public `Config::load_with_paths` entry point
//! (env, hermes, standalone) so they can pin every precedence case
//! without mutating the host process environment beyond the
//! ``CADUCEUS_DRY_RUN`` variable (serialised under ``ENV_LOCK``).

use std::path::PathBuf;
use std::sync::Mutex;

use caduceus::config::Config;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-resolution-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write config");
}

/// Run *f* with ``CADUCEUS_DRY_RUN`` either set or cleared for the
/// duration of the call. The original env state is restored on exit.
fn with_dry_run<F, R>(value: Option<&str>, f: F) -> R
where
    F: FnOnce() -> R,
{
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let prior = std::env::var_os("CADUCEUS_DRY_RUN");
    match value {
        Some(v) => std::env::set_var("CADUCEUS_DRY_RUN", v),
        None => std::env::remove_var("CADUCEUS_DRY_RUN"),
    }
    let result = f();
    match prior {
        Some(v) => std::env::set_var("CADUCEUS_DRY_RUN", v),
        None => std::env::remove_var("CADUCEUS_DRY_RUN"),
    }
    result
}

fn assert_invalid_dry_run(value: &str) {
    let mut cfg = Config::test_defaults(&tempdir("invalid-dry-run"));
    let err = cfg
        .apply_dry_run_env(value)
        .expect_err("invalid dry-run value must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("CADUCEUS_DRY_RUN"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn explicit_env_file_wins_over_hermes_and_standalone() {
    let root = tempdir("explicit-wins");
    let explicit = root.join("explicit.yaml");
    write(
        &explicit,
        r#"
        worker_command: ["python3", "/from/explicit.py"]
        dry_run: false
        "#,
    );
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        model:
          default: hermes-only
        caduceus:
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/from/standalone.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(Some(&explicit), Some(&hermes_dir), Some(&standalone))
            .expect("explicit file wins")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/explicit.py")
    );
}

#[test]
#[serial_test::serial]
fn hermes_file_wins_when_explicit_env_not_set() {
    let root = tempdir("hermes-wins");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/from/standalone.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&hermes_dir), Some(&standalone))
            .expect("hermes wins over standalone")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/hermes.py")
    );
}

#[test]
#[serial_test::serial]
fn standalone_used_when_hermes_missing() {
    let root = tempdir("standalone-only");
    let hermes_dir = root.join("home"); // config.yaml never created
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/from/standalone.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&hermes_dir), Some(&standalone))
            .expect("standalone used when hermes is missing")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/standalone.py")
    );
}

// ---------------------------------------------------------------------------
// Explicit env errors
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn explicit_missing_env_path_is_a_hard_error() {
    let root = tempdir("explicit-missing");
    let missing = root.join("does-not-exist.yaml");
    let err = with_dry_run(None, || {
        Config::load_with_paths(Some(&missing), None, None)
            .expect_err("missing $CADUCEUS_CONFIG must fail")
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("$CADUCEUS_CONFIG points at") && msg.contains("missing"),
        "got: {msg}"
    );
}

#[test]
#[serial_test::serial]
fn explicit_env_malformed_yaml_is_a_hard_error() {
    let root = tempdir("explicit-malformed");
    let explicit = root.join("bad.yaml");
    write(
        &explicit,
        r#"
        worker_command: ["python3",
        missing_close_bracket: oops
        "#,
    );
    let err = with_dry_run(None, || {
        Config::load_with_paths(Some(&explicit), None, None)
            .expect_err("malformed explicit YAML must fail")
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("failed to parse") && msg.contains("bad.yaml"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Hermes file edge cases
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn hermes_file_without_caduceus_section_falls_through_to_standalone() {
    let root = tempdir("hermes-no-section");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        model:
          default: hermes-only
        "#,
    );
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/from/standalone.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&hermes_dir), Some(&standalone))
            .expect("hermes without section falls through to standalone")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/from/standalone.py")
    );
}

#[test]
#[serial_test::serial]
fn hermes_without_caduceus_section_and_no_standalone_reports_missing_section() {
    let root = tempdir("hermes-no-section-no-standalone");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        model:
          default: hermes-only
        "#,
    );
    let err = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&hermes_dir), None)
            .expect_err("missing caduceus section must surface")
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("missing 'caduceus:' section") || msg.contains("has no 'caduceus:' section"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// CADUCEUS_DRY_RUN truth table
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn dry_run_truth_table() {
    let root = tempdir("dry-run-table");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          worker_command: ["python3", "/from/hermes.py"]
          dry_run: false
        "#,
    );

    let cases: &[(&str, bool)] = &[
        ("1", true),
        ("true", true),
        ("TRUE", true),
        ("yes", true),
        ("Yes", true),
        ("0", false),
        ("false", false),
        ("FALSE", false),
        ("no", false),
        ("No", false),
    ];

    for (raw, expected) in cases {
        let cfg = with_dry_run(Some(raw), || {
            Config::load_with_paths(None, Some(&hermes_dir), None)
                .expect("dry-run truth table parses")
        });
        assert_eq!(
            cfg.dry_run, *expected,
            "dry-run for {raw:?} should be {expected}"
        );
    }
}

#[test]
#[serial_test::serial]
fn dry_run_yaml_value_overridden_by_env_when_truthy() {
    let root = tempdir("dry-run-override-truthy");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          dry_run: false
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );
    let cfg = with_dry_run(Some("yes"), || {
        Config::load_with_paths(None, Some(&hermes_dir), None).expect("dry-run env override parses")
    });
    assert!(cfg.dry_run);
}

#[test]
#[serial_test::serial]
fn dry_run_yaml_truthy_overridden_by_env_when_falsy() {
    let root = tempdir("dry-run-override-falsy");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          dry_run: true
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );
    let cfg = with_dry_run(Some("no"), || {
        Config::load_with_paths(None, Some(&hermes_dir), None).expect("dry-run env override parses")
    });
    assert!(!cfg.dry_run);
}

#[test]
#[serial_test::serial]
fn dry_run_yaml_value_kept_when_env_unset() {
    let root = tempdir("dry-run-yaml");
    let hermes_dir = root.join("home");
    write(
        &hermes_dir.join("config.yaml"),
        r#"
        caduceus:
          dry_run: true
          worker_command: ["python3", "/from/hermes.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&hermes_dir), None).expect("dry-run env unset parses")
    });
    assert!(cfg.dry_run);
}

#[test]
#[serial_test::serial]
fn dry_run_invalid_value_is_rejected() {
    let _guard = ENV_LOCK.lock().expect("env lock poisoned");
    for value in ["maybe", "2", "true1", "", "   "] {
        assert_invalid_dry_run(value);
    }
}

// ---------------------------------------------------------------------------
// Path semantics
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn non_unicode_yaml_bytes_are_rejected_with_clear_error() {
    // The contract mandates Unicode YAML parsing for env-derived
    // paths. Write a file containing a UTF-8 invalid byte so the
    // YAML parser surfaces a clear error to the operator.
    let root = tempdir("non-unicode-env");
    let explicit = root.join("with-binary.yaml");
    std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
    std::fs::write(&explicit, b"worker_command: [\"python3\"]\n\xffbinary: 1\n")
        .expect("write binary yaml");
    let err = with_dry_run(None, || {
        Config::load_with_paths(Some(&explicit), None, None)
            .expect_err("non-Unicode YAML must be rejected")
    });
    let msg = format!("{err:?}");
    // A YAML parse error or a UTF-8 decode failure is acceptable;
    // both surface a config-shaped error to the operator.
    assert!(msg.contains("Config"), "got: {msg}");
}

#[test]
#[serial_test::serial]
fn paths_with_spaces_round_trip_cleanly() {
    let root = tempdir("path-with-spaces");
    let spaced = root.join("dir with spaces");
    std::fs::create_dir_all(&spaced).unwrap();
    write(
        &spaced.join("config.yaml"),
        r#"
        caduceus:
          worker_command: ["python3", "/with/space.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&spaced), None).expect("paths with spaces parse cleanly")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/with/space.py")
    );
}

#[test]
#[serial_test::serial]
fn relative_hermes_home_is_rejected() {
    // Build a real relative path (don't actually create the dir) —
    // the loader should reject before it tries to read.
    let relative = std::path::PathBuf::from("relative/home");
    let err = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&relative), None)
            .expect_err("relative HERMES_HOME must be rejected")
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("HERMES_HOME must be an absolute path"),
        "got: {msg}"
    );
}

#[test]
#[serial_test::serial]
fn empty_hermes_home_is_rejected() {
    let empty = std::path::PathBuf::new();
    let err = with_dry_run(None, || {
        Config::load_with_paths(None, Some(&empty), None)
            .expect_err("empty HERMES_HOME must be rejected")
    });
    let msg = format!("{err:?}");
    assert!(msg.contains("HERMES_HOME must not be empty"), "got: {msg}");
}

#[test]
#[serial_test::serial]
fn standalone_only_works_without_hermes() {
    let root = tempdir("standalone-no-hermes");
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/standalone.py"]
        "#,
    );
    let cfg = with_dry_run(None, || {
        Config::load_with_paths(None, None, Some(&standalone))
            .expect("standalone works without hermes slot")
    });
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/standalone.py")
    );
}

#[test]
#[serial_test::serial]
fn no_sources_at_all_is_an_error() {
    let err = with_dry_run(None, || {
        Config::load_with_paths(None, None, None).expect_err("no sources must error")
    });
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no configuration source") || msg.contains("no configuration source provided"),
        "got: {msg}"
    );
}

#[test]
#[serial_test::serial]
fn load_from_supports_standalone_shape() {
    let root = tempdir("load-from");
    let standalone = root.join("standalone.yaml");
    write(
        &standalone,
        r#"
        worker_command: ["python3", "/direct/path.py"]
        "#,
    );
    let cfg = Config::load_from(&standalone).expect("load_from parses standalone");
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/direct/path.py")
    );
}

#[test]
#[serial_test::serial]
fn load_from_supports_hermes_shape() {
    let root = tempdir("load-from-hermes");
    let cfg_file = root.join("config.yaml");
    write(
        &cfg_file,
        r#"
        model:
          default: test
        caduceus:
          worker_command: ["python3", "/hermes/path.py"]
        "#,
    );
    let cfg = Config::load_from(&cfg_file).expect("load_from parses hermes shape");
    assert_eq!(
        cfg.worker_command.get(1).map(String::as_str),
        Some("/hermes/path.py")
    );
}
