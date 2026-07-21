//! Adversarial tamper tests for the OCI executor isolation boundary.
//!
//! These tests verify that the isolation boundary prevents tampering
//! with the container's filesystem, secrets, and git metadata. They
//! use INFRA-LEVEL primitives (oci_args::build_argv, redact(), argv
//! inspection) since src/runtime/edit_validator.rs does not exist yet.

use std::path::PathBuf;

use caduceus::executor::oci_args::build_argv;
use caduceus::executor::policy::IsolationPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;
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

// ---------------------------------------------------------------------------
// tamper_modified_files — undeclared mount is rejected
// ---------------------------------------------------------------------------

#[test]
fn tamper_modified_files() {
    // An adversary who tries to inject an undeclared mount into the
    // container must be rejected. The build_argv function enforces
    // the mount allow-list: every -v flag must map to a declared
    // MountSpec. An empty mount list with a spec that references
    // /tmp/worktree triggers OciUndeclaredMount.

    let cfg = test_cfg();
    let spec = test_spec("tamper-modified-files");

    // Pass an empty mount list — the worktree is in the spec but
    // the caller didn't declare it in the mount allow-list.
    let mounts: Vec<caduceus::executor::oci_args::MountSpec> = vec![];

    let result = build_argv(&spec, &cfg, &mounts, None);

    match result {
        Err(CaduceusError::OciUndeclaredMount { path }) => {
            assert!(
                path.contains("worktree"),
                "expected worktree path in error, got: {path}"
            );
        }
        Err(other) => panic!("expected OciUndeclaredMount; got: {other:?}"),
        Ok(_) => panic!("expected error for undeclared mount"),
    }
}

// ---------------------------------------------------------------------------
// tamper_secret_in_result — redact() scrubs ghp_ tokens from output
// ---------------------------------------------------------------------------

#[test]
fn tamper_secret_in_result() {
    // An adversary who tries to exfiltrate a GitHub PAT through the
    // result output must be blocked by the redaction layer. The
    // redact() function from infra::logging scrubs credential-shaped
    // values (GITHUB_TOKEN=ghp_..., GH_TOKEN=ghp_...) from any
    // string before it reaches the log stream.

    let cases = vec![
        // ghp_ token in GITHUB_TOKEN assignment
        (
            "GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            true, // should be redacted
        ),
        // ghp_ token in GH_TOKEN assignment
        ("GH_TOKEN=ghp_1234567890abcdefghijklmnop", true),
        // ghp_ token in CADUCEUS_GITHUB_TOKEN assignment
        ("CADUCEUS_GITHUB_TOKEN=ghp_xyz789abc", true),
        // Non-credential string with ghp_ (not after a credential name)
        (
            "some_output=ghp_not_a_credential",
            false, // not redacted (no credential name prefix)
        ),
        // Plain string without any credential
        ("hello world", false),
        // Empty string
        ("", false),
    ];

    for (input, should_redact) in cases {
        let result = logging::redact(input);
        if should_redact {
            assert!(
                result.contains("<redacted>"),
                "expected redaction for input {input:?}, got: {result:?}"
            );
            assert!(
                !result.contains("ghp_"),
                "ghp_ token must be redacted in output for input {input:?}, got: {result:?}"
            );
        } else {
            // For non-credential strings, the output should be unchanged
            // (unless the string happens to match a credential pattern)
            if !input.contains("GITHUB_TOKEN=")
                && !input.contains("GH_TOKEN=")
                && !input.contains("CADUCEUS_GITHUB_TOKEN=")
            {
                assert_eq!(
                    result, input,
                    "non-credential input should pass through unchanged"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tamper_commit_metadata — argv does not contain .git volume mount
// ---------------------------------------------------------------------------

#[test]
fn tamper_commit_metadata() {
    // The git-less worker contract ensures that the .git directory is
    // never mounted as a writable volume. An adversary who tries to
    // tamper with git metadata (commit hashes, refs, config) must be
    // blocked because .git is not accessible from inside the container.
    //
    // This test verifies that the argv produced by IsolationPolicy::enforce
    // does not contain any .git volume mount.

    let cfg = test_cfg();
    let spec = test_spec("tamper-commit-metadata");

    let result = IsolationPolicy::enforce(&spec, &cfg);

    match &result {
        Ok(enforced) => {
            let argv = &enforced.argv;
            // The .git directory should not appear as a direct
            // volume mount path. Look for volume targets that
            // contain ".git".
            let git_refs: Vec<&String> = argv.iter().filter(|a| a.contains(".git")).collect();
            // The worktree path is "/tmp/worktree" — no .git in it.
            assert!(
                git_refs.is_empty(),
                "unexpected .git reference in argv: {git_refs:?}"
            );
        }
        Err(e) => {
            panic!("enforcement failed with: {e:?}");
        }
    }
}
