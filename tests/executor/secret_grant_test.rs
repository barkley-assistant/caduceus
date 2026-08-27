//! Regression tests for the removed secret-grant placeholder.
//!
//! The prototype `secret_grants` config surface was deleted with the
//! `sandbox-config-section` change. The renderer is the sole argv
//! producer and receives secret env-file paths explicitly via
//! `render_with_env_files` (paths only, from
//! `EphemeralSecretFile`); the plain `render` path must never emit
//! `--env-file`. These tests pin that the resolution/render layer
//! stays out of secret handling unless the caller passes env files.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, RuntimeFacts, SandboxEngine, SandboxSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;

fn default_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn resolve_from(cfg: &Config) -> SandboxSpec {
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let output = cfg.workdir_base.join("owner").join("repo").join("result");
    let runtime = RuntimeFacts {
        run_id: "run-001".to_string(),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree,
        output_dir: output,
        daemon_id: "test-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
    };
    resolve(cfg.sandbox(), &runtime).expect("must resolve")
}

// render_does_not_emit_secret_env_file — the plain renderer emits no
// --env-file; secret env files only appear when the caller passes
// them to render_with_env_files

#[test]
fn render_does_not_emit_secret_env_file() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        !argv.iter().any(|a| a == "--env-file"),
        "argv must not contain --env-file, got: {argv:?}"
    );
}

// render_with_env_files_emits_env_file — the explicit seam works

#[test]
fn render_with_env_files_emits_env_file() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let env_files = vec![std::path::PathBuf::from("/tmp/secret-1.env")];
    let argv = caduceus::executor::sandbox_renderer::render_with_env_files(
        &spec,
        SandboxEngine::Docker,
        &env_files,
    );
    let pos = argv
        .iter()
        .position(|a| a == "--env-file")
        .expect("--env-file");
    assert_eq!(argv[pos + 1], "/tmp/secret-1.env");
}

// argv_never_contains_grant_names — nothing in the argv echoes
// secret-ish names from the config surface

#[test]
fn argv_never_contains_grant_names() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    for token in &argv {
        assert!(
            !token.contains("my-secret") && !token.contains("test-secret"),
            "secret-ish name must not appear in argv: {token:?}"
        );
    }
}
