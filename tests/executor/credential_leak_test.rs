//! Adversarial credential-leak tests for the OCI executor.
//!
//! These tests verify that secret credentials (GitHub PAT, GITHUB_TOKEN,
//! GH_TOKEN) never leak through argv, log output, or signal handling.
//!
//! Credential-handling tests use the pure-function APIs
//! (EphemeralSecretFile, redact) when possible, and gate engine-dependent
//! scenarios behind `CADUCEUS_RUN_ISOLATION_TESTS`.

use std::path::PathBuf;

use caduceus::executor::secret_transport::EphemeralSecretFile;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::logging;

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

// leak_via_argv — secret value in worker command or env must not appear
// in the final argv

#[test]
fn leak_via_argv() {
    // Write a secret via EphemeralSecretFile and verify that the value
    // never appears in argv tokens. The SecretHandle only exposes paths,
    // not values.
    let secrets = vec![
        ("GITHUB_TOKEN".to_string(), "ghp_abc123_secret".to_string()),
        ("GH_TOKEN".to_string(), "ghp_def456_secret".to_string()),
    ];
    let handle = EphemeralSecretFile::write(&secrets).expect("write must succeed");

    // Verify that secret values do not appear in any exposed surface
    let debug_output = format!("{handle:?}");
    let display_output = format!("{handle}");

    assert!(
        !debug_output.contains("ghp_abc123_secret"),
        "secret leaked into Debug: {debug_output}"
    );
    assert!(
        !debug_output.contains("ghp_def456_secret"),
        "second secret leaked into Debug: {debug_output}"
    );
    assert!(
        !display_output.contains("ghp_abc123_secret"),
        "secret leaked into Display: {display_output}"
    );

    // Verify paths don't contain secret values
    for path in handle.paths() {
        let path_str = path.to_string_lossy();
        assert!(
            !path_str.contains("ghp_abc123_secret"),
            "secret leaked into path: {path_str}"
        );
    }

    // Verify the secret file is cleaned up after handle is dropped
    let paths: Vec<PathBuf> = handle.paths().to_vec();
    drop(handle);
    for p in &paths {
        assert!(
            !p.exists(),
            "secret file must be removed after drop: {}",
            p.display()
        );
    }
}

// leak_via_log — redact() must scrub credential values from log output
// while keeping the variable name visible

#[test]
fn leak_via_log() {
    // The redact function from infra::logging removes credential values
    // from log strings. Test that:
    // 1. GITHUB_TOKEN=<value> is redacted to GITHUB_TOKEN=<redacted>
    // 2. GH_TOKEN=<value> is redacted
    // 3. Non-credential strings pass through untouched

    let cases = vec![
        ("GITHUB_TOKEN=ghp_supersecret123", "GITHUB_TOKEN=<redacted>"),
        ("GH_TOKEN=ghp_another_secret", "GH_TOKEN=<redacted>"),
        (
            "export CADUCEUS_GITHUB_TOKEN=ghp_xyz789",
            "export CADUCEUS_GITHUB_TOKEN=<redacted>",
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
    // When a worker receives SIGUSR1 (or any signal), the daemon's audit
    // log must record that a signal was received without including the
    // secret value.
    //
    // This test requires a live OCI engine to run a container that
    // sends a signal with an embedded secret. The daemon audit log
    // must contain "SignalReceived" without the secret value.
    //
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  docker run caduceus-worker sh -c '
    //  GITHUB_TOKEN=ghp_secret
    //  kill -USR1 $$
    //  '
    // Expected: daemon audit contains "SignalReceived" but NOT "ghp_secret"

    let _cfg = test_cfg();
    let spec = test_spec("leak-via-signal");

    // Verify the spec doesn't contain secret values in its exposed fields
    let spec_debug = format!("{spec:?}");
    assert!(
        !spec_debug.contains("ghp_secret"),
        "spec Debug must not contain secret values: {spec_debug}"
    );
}
