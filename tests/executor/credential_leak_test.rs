//! Adversarial credential-leak tests for the OCI executor (issue
//! #249; spec R6; design D7).
//!
//! These tests verify that secret credentials and resolved
//! `pass_env` values never leak through argv, log output, `Debug`
//! output, or the environment-file transport. The env-file
//! transport and the pure-function `redact`/`scrub` helpers are
//! exercised directly; the live engine env contract lives in the
//! gated suite (`tests/integration/oci_env_live_test.rs`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use caduceus::executor::oci_env_file::OciEnvFile;
use caduceus::executor::sandbox_spec::{resolve_with_env, SandboxSpec};
use caduceus::infra::config::Config;
use caduceus::infra::logging;

mod support;

fn test_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

// leak_via_argv — resolved env values must never appear in argv; the
// env file is the only transport and only its PATH is in argv.

#[test]
fn leak_via_argv() {
    let cfg = test_cfg();
    let mut cfg = cfg;
    cfg.sandbox.as_mut().expect("sandbox").pass_env = vec!["GITHUB_TOKEN".to_string()];
    // Resolution refuses the denied name — the value can never even
    // be assembled, let alone reach argv.
    let runtime = support::runtime_facts(
        &cfg,
        "leak-argv",
        &cfg.workdir_base
            .join("owner")
            .join("repo")
            .join("leak-argv"),
    );
    let parent: BTreeMap<std::ffi::OsString, std::ffi::OsString> =
        [("GITHUB_TOKEN", "ghp_abc123_secret")]
            .iter()
            .map(|(k, v)| (std::ffi::OsString::from(*k), std::ffi::OsString::from(*v)))
            .collect();
    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
        &parent,
    )
    .expect_err("denied pass_env name must be refused");
    let rendered = format!("{err:?}");
    assert!(
        !rendered.contains("ghp_abc123_secret"),
        "refusal Debug must never contain the value: {rendered}"
    );
}

// leak_via_transport_debug — the env-file handle exposes the path
// only; resolved values never reach Debug or Display output.

#[test]
fn leak_via_transport_debug() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let env: BTreeMap<String, String> = [
        ("MY_TOOL_TOKEN", "ghp_abc123_secret"),
        ("OTHER", "ghp_def456_secret"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let file = OciEnvFile::create(&tmp.path().join("oci-runs").join("leak"), &env)
        .expect("create env file");

    let debug_output = format!("{file:?}");
    assert!(
        !debug_output.contains("ghp_abc123_secret"),
        "handle Debug must not contain secret values: {debug_output}"
    );
    assert!(
        !debug_output.contains("ghp_def456_secret"),
        "handle Debug must not contain second secret: {debug_output}"
    );
    // The path itself carries no value bytes.
    let path_str = file.path().to_string_lossy();
    assert!(!path_str.contains("ghp_abc123_secret"));

    // The file is removed after the guard is dropped.
    let path: PathBuf = file.path().to_path_buf();
    drop(file);
    assert!(!path.exists(), "env file must be removed after drop");
}

// leak_via_spec_debug — a resolved spec's Debug output must redact
// the environment: names may appear, values must not.

#[test]
fn leak_via_spec_debug() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox.as_mut().expect("sandbox").pass_env = vec!["MY_TOOL_TOKEN".to_string()];
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join("leak-spec");
    let runtime = support::runtime_facts(&cfg, "leak-spec", &worktree);
    let parent: BTreeMap<std::ffi::OsString, std::ffi::OsString> =
        [("MY_TOOL_TOKEN", "ghp_spec_debug_secret")]
            .iter()
            .map(|(k, v)| (std::ffi::OsString::from(*k), std::ffi::OsString::from(*v)))
            .collect();
    let spec: SandboxSpec = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
        &parent,
    )
    .expect("must resolve");

    let debug_output = format!("{spec:?}");
    assert!(
        !debug_output.contains("ghp_spec_debug_secret"),
        "spec Debug must never contain resolved values: {debug_output}"
    );
    // Names may appear; values must not.
    assert!(
        debug_output.contains("MY_TOOL_TOKEN"),
        "spec Debug may carry env names: {debug_output}"
    );
}

// leak_via_log — redact() must scrub credential values from log output
// while keeping the variable name visible, and must redact env-file
// paths so the at-rest values are never pointed at from logs.

#[test]
fn leak_via_log() {
    let cases = vec![
        ("GITHUB_TOKEN=ghp_supersecret123", "GITHUB_TOKEN=<redacted>"),
        ("GH_TOKEN=ghp_another_secret", "GH_TOKEN=<redacted>"),
        (
            "export CADUCEUS_GITHUB_TOKEN=ghp_xyz789",
            "export CADUCEUS_GITHUB_TOKEN=<redacted>",
        ),
        // Env-file paths are never loggable content (issue #249).
        (
            "using env file /state/oci-runs/r1/caduceus_env_01ABC.env",
            "using env file /state/oci-runs/r1/<redacted>",
        ),
        // Non-credential strings pass through
        ("hello world", "hello world"),
        ("PATH=/usr/bin", "PATH=/usr/bin"),
        // Quoted values
        (r#"GITHUB_TOKEN="ghp_quoted""#, "GITHUB_TOKEN=<redacted>"),
        // Multiple secrets in one string
        (
            "GITHUB_TOKEN=abc GH_TOKEN=def",
            "GITHUB_TOKEN=<redacted> GH_TOKEN=<redacted>",
        ),
        // Empty input
        ("", ""),
    ];

    for (input, expected) in &cases {
        let result = logging::redact(input);
        assert_eq!(
            &result, expected,
            "redact({input:?}) expected {expected:?}, got {result:?}"
        );
    }
}

// missing_pass_env_aborts_pre_create — an approved-but-absent
// pass_env name must fail resolution with a typed error BEFORE any
// `docker create` (I9 frozen semantics: never warn-and-skip).
//
// The daemon snapshot is authoritative: a config that requests
// `pass_env = ["CADUCEUS_LIVE_PASS_ENV_CANARY"]` but whose daemon
// environment does not contain that name must abort pre-create.

#[test]
fn missing_pass_env_aborts_pre_create() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox
        .as_mut()
        .expect("sandbox")
        .pass_env
        .push("CADUCEUS_LIVE_PASS_ENV_CANARY".to_string());
    let worktree = cfg
        .workdir_base
        .join("owner")
        .join("repo")
        .join("missing-pass-env");
    let runtime = support::runtime_facts(&cfg, "missing-pass-env", &worktree);
    // The daemon snapshot does NOT carry the requested name.
    let parent: BTreeMap<std::ffi::OsString, std::ffi::OsString> = BTreeMap::new();

    let err = resolve_with_env(
        cfg.sandbox(),
        &runtime,
        &support::executor_spec(&runtime),
        &parent,
    )
    .expect_err("missing pass_env name must abort pre-create");

    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("CADUCEUS_LIVE_PASS_ENV_CANARY"),
        "the refusal must name the missing variable: {rendered}"
    );
    // The refusal is a typed error, not a silent success.
    assert!(matches!(err, caduceus::CaduceusError::Config(_)));
}
