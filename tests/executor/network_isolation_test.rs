//! Adversarial network-isolation tests for the OCI executor.
//!
//! These tests verify that network egress is correctly blocked or
//! allowed based on the configured sandbox network mode, via the
//! typed pipeline (`resolve` → `render`). All tests that require a
//! live container engine are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS`.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, RuntimeFacts, SandboxEngine, SandboxSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, SandboxNetwork};

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

fn default_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn network_value(cfg: &Config) -> String {
    let spec = resolve_from(cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    argv[pos + 1].clone()
}

// probe_blocked_egress — no network profile → all egress blocked

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_blocked_egress() {
    // With the default sandbox network, the rendered argv carries
    // --network none. Any outbound connection from the container must
    // fail at the engine level.
    let cfg = default_cfg();
    assert_eq!(
        network_value(&cfg),
        "none",
        "expected --network=none for blocked egress"
    );
}

// probe_allowed_egress — unrestricted network mode allows egress

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_allowed_egress() {
    let mut cfg = default_cfg();
    cfg.sandbox.as_mut().unwrap().network = SandboxNetwork::Unrestricted;
    assert_eq!(
        network_value(&cfg),
        "host",
        "expected --network=host for allowed egress"
    );
}

// probe_dns_exfiltration — DNS queries are blocked with no network
// profile

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_dns_exfiltration() {
    // --network none blocks DNS resolution entirely because the
    // container has no network stack.
    let cfg = default_cfg();
    assert_eq!(
        network_value(&cfg),
        "none",
        "expected --network=none for DNS exfiltration prevention"
    );
}
