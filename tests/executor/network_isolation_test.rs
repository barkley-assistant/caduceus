//! Adversarial network-isolation tests for the OCI executor.
//!
//! These tests verify that network egress is correctly blocked or
//! allowed based on the configured sandbox network mode, via the
//! typed pipeline (`resolve` → `render`). All tests that require a
//! live container engine are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS`.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine, SandboxSpec};
use caduceus::infra::config::Config;

mod support;

fn resolve_from(cfg: &Config) -> SandboxSpec {
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let runtime = support::runtime_facts(cfg, "run-001", &worktree);
    resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve")
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

// probe_allowed_egress — host networking is structurally
// unrepresentable (breaking, issue #245): a config that requests
// `network: unrestricted` fails at YAML parse time with a typed
// unknown-variant error.

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_allowed_egress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let image =
        "caduceus-worker@sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let body = format!(
        "worker_command: [\"python3\", \"/tmp/bridge.py\"]\n\
         state_dir: \"{}/state\"\n\
         reduced_containment_acknowledged: true\n\
         sandbox:\n\
         \x20 image: \"{image}\"\n\
         \x20 network: unrestricted\n",
        dir.path().display()
    );
    std::fs::write(&path, body).expect("write config");
    let err = Config::load_from(&path).expect_err("network: unrestricted must fail to parse");
    let msg = err.to_string();
    assert!(msg.contains("unknown variant"), "got: {msg}");
    assert!(msg.contains("unrestricted"), "got: {msg}");
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
