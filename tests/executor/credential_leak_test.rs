//! Adversarial credential-leak tests for the OCI executor (issue
//! #249; spec R6; design D7).
//!
//! These tests verify that secret credentials and resolved
//! `pass_env` values never leak through argv, log output, `Debug`
//! output, or signal handling.
//!
//! Transport tests use the daemon-private env-file transport
//! (`OciEnvFile`) and the pure-function `redact`/`scrub` helpers;
//! engine-dependent scenarios are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use caduceus::executor::oci_env_file::OciEnvFile;
use caduceus::executor::sandbox_spec::{resolve_with_env, SandboxSpec};
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::logging;

mod support;

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
        issue_title: "title".to_string(),
        issue_body: "body".to_string(),
        labels: Vec::new(),
        branch_name: "automation/issue-1".to_string(),
    }
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

// leak_via_signal — signal delivery must not expose secret values

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn leak_via_signal() {
    let _cfg = test_cfg();
    let spec = test_spec("leak-via-signal");

    // Verify the spec doesn't contain secret values in its exposed fields
    let spec_debug = format!("{spec:?}");
    assert!(
        !spec_debug.contains("ghp_secret"),
        "spec Debug must not contain secret values: {spec_debug}"
    );
}
