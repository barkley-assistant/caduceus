//! Network-isolation tests for the OCI executor, driven through the
//! typed pipeline (`resolve` → `render`).
//!
//! The network policy is a closed two-variant enum: `none`
//! (loopback-only, the default — every outbound connection fails at
//! the engine level) or `unrestricted` (the engine's default isolated
//! bridge — NAT'd outbound egress with no host namespace joining).
//! Neither mode can ever render `--network host`: host networking is
//! structurally unrepresentable (no variant maps to it), pinned here
//! and in the renderer goldens.
//!
//! These tests are pure — no live container engine is required.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, NetworkMode, SandboxEngine, SandboxSpec};
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

/// `Config::test_defaults` with `sandbox.network` set to
/// `unrestricted` (the engine's default isolated bridge — NAT'd
/// egress, never host networking).
fn unrestricted_cfg() -> Config {
    use caduceus::infra::config::SandboxNetwork;

    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox
        .as_mut()
        .expect("test_defaults has a sandbox")
        .network = SandboxNetwork::Unrestricted;
    cfg
}

/// The token rendered after `--network` for the given engine.
fn network_value(spec: &SandboxSpec, engine: SandboxEngine) -> String {
    let argv = render(spec, engine);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    argv[pos + 1].clone()
}

// probe_blocked_egress — the default network mode is `none` → all
// egress blocked

#[test]
fn probe_blocked_egress() {
    // With the default sandbox network, the spec resolves to
    // NetworkMode::None and the rendered argv carries --network none
    // on both engines. Any outbound connection from the container
    // must fail at the engine level.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    assert_eq!(spec.network(), NetworkMode::None);
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        assert_eq!(
            network_value(&spec, engine),
            "none",
            "expected --network=none for blocked egress ({engine:?})"
        );
    }
}

// probe_allowed_egress — `sandbox.network = unrestricted` resolves to
// `NetworkMode::Unrestricted` and renders the engine's default
// isolated bridge (`bridge`) — NAT'd outbound egress, NOT host
// networking, and NOT `none`.

#[test]
fn probe_allowed_egress() {
    let cfg = unrestricted_cfg();
    let spec = resolve_from(&cfg);
    assert_eq!(spec.network(), NetworkMode::Unrestricted);
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        assert_eq!(
            network_value(&spec, engine),
            "bridge",
            "expected --network=bridge (engine default isolated bridge) for \
             unrestricted ({engine:?})"
        );
        assert_ne!(
            network_value(&spec, engine),
            "none",
            "unrestricted must not render --network=none ({engine:?})"
        );
        assert_ne!(
            network_value(&spec, engine),
            "host",
            "unrestricted must never render --network=host ({engine:?})"
        );
    }
}

// probe_dns_exfiltration — DNS queries are blocked with the default
// `none` mode

#[test]
fn probe_dns_exfiltration() {
    // --network none blocks DNS resolution entirely because the
    // container has no network stack.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
        assert_eq!(
            network_value(&spec, engine),
            "none",
            "expected --network=none for DNS exfiltration prevention ({engine:?})"
        );
    }
}
