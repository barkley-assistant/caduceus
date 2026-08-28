//! Adversarial tamper tests for the OCI executor isolation boundary.
//!
//! These tests verify that the isolation boundary prevents tampering
//! with the container's filesystem, secrets, and git metadata. They
//! use the typed pipeline (`resolve` → `render`) plus the `redact()`
//! primitive from `infra::logging`.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine, SandboxSpec};
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;
use caduceus::infra::logging;

mod support;

fn default_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn resolve_from(cfg: &Config, run_id: &str) -> SandboxSpec {
    let worktree = cfg.workdir_base.join("owner").join("repo").join(run_id);
    let runtime = support::runtime_facts(cfg, run_id, &worktree);
    resolve(cfg.sandbox(), &runtime).expect("must resolve")
}

// tamper_modified_files — an undeclared host path is rejected

#[test]
fn tamper_modified_files() {
    // An adversary who tries to inject an undeclared mount into the
    // container must be rejected. The resolution step owns the
    // host-path allow-list: a worktree outside `workdir_base`
    // triggers OciUndeclaredMount.
    let cfg = default_cfg();
    let runtime = support::runtime_facts(
        &cfg,
        "tamper-modified-files",
        std::path::Path::new("/tmp/worktree"),
    );
    let result = resolve(cfg.sandbox(), &runtime);
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

// tamper_secret_in_result — redact() scrubs ghp_ tokens from output

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

// tamper_commit_metadata — argv's only .git reference is the
// read-only daemon-owned shadow

#[test]
fn tamper_commit_metadata() {
    // The .git pointer is shadowed by a daemon-owned read-only mount;
    // the real gitdir is unreachable and .git is never writable.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg, "tamper-commit-metadata");
    let argv = render(&spec, SandboxEngine::Docker);
    let git_refs: Vec<&String> = argv.iter().filter(|a| a.contains(".git")).collect();
    assert_eq!(
        git_refs.len(),
        1,
        "exactly the shadow mount may reference .git, got: {git_refs:?}"
    );
    assert!(
        git_refs[0].ends_with(":/workspace/.git:ro"),
        "the .git reference must be the read-only shadow, got: {:?}",
        git_refs[0]
    );
}
