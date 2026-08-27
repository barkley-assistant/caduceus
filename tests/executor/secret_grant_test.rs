//! Regression tests for the removed secret-grant placeholder.
//!
//! The prototype `secret_grants` config surface and
//! `resolve_secret_grants` policy wiring were deleted with the
//! `sandbox-config-section` change (no secrets backend replaces them).
//! [`IsolationPolicy::enforce`] no longer creates ephemeral secret
//! files from config grants and no longer emits `--env-file` — secret
//! transport remains the caller's explicit seam
//! (`oci_lifecycle::run` / `EphemeralSecretFile`). These tests pin
//! that the policy layer stays out of secret handling.

use std::path::PathBuf;

use caduceus::executor::policy::IsolationPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;

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

// enforce_does_not_emit_secret_env_file — the policy layer no longer
// wires config grants into ephemeral files

#[test]
fn enforce_does_not_emit_secret_env_file() {
    let cfg = test_cfg();
    let spec = test_spec("test-no-secret-file");

    let enforced = IsolationPolicy::enforce(&spec, &cfg)
        .expect("enforcement must succeed without any secret wiring");
    assert!(
        !enforced.argv.iter().any(|a| a == "--env-file"),
        "argv must not contain --env-file, got: {:?}",
        enforced.argv
    );
}

// enforce_argv_never_contains_grant_names — nothing in the argv echoes
// secret-ish names from the config surface

#[test]
fn enforce_argv_never_contains_grant_names() {
    let cfg = test_cfg();
    let spec = test_spec("test-no-grant-names");

    let enforced = IsolationPolicy::enforce(&spec, &cfg)
        .expect("enforcement must succeed without any secret wiring");
    for token in &enforced.argv {
        assert!(
            !token.contains("my-secret") && !token.contains("test-secret"),
            "secret-ish name must not appear in argv: {token:?}"
        );
    }
}
