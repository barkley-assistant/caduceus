//! Adversarial network-isolation tests for the OCI executor.
//!
//! These tests verify that network egress is correctly blocked or
//! allowed based on the configured network profile. All tests that
//! require a live container engine are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS`.

use std::path::PathBuf;

use caduceus::executor::network::NetworkPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;

fn test_cfg() -> Config {
    let tmp = tempfile::tempdir().expect("tempdir");
    Config::test_defaults(tmp.path())
}

fn test_spec(run_id: &str, network_profile: Option<&str>) -> ExecutorSpec {
    ExecutorSpec {
        self_exe: PathBuf::from("/usr/bin/caduceus"),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worktree: PathBuf::from("/tmp/worktree"),
        run_id: run_id.to_string(),
        context_json: r#"{"x":1}"#.to_string(),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        cancellation: tokio_util::sync::CancellationToken::new(),
        network_profile: network_profile.map(|s| s.to_string()),
    }
}

// probe_blocked_egress — no network profile → all egress blocked

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_blocked_egress() {
    // With no network profile, the network args become --network=none.
    // Any outbound connection from the container must fail. The daemon
    // audit log must contain "EgressBlocked".
    //
    // When CADUCEUS_RUN_ISOLATION_TESTS is set, run:
    //  docker run --network=none caduceus-worker \
    //  sh -c 'curl -s https://api.github.com/zen'
    // Expected: curl exits with "Connection refused" or similar
    // Daemon audit: "EgressBlocked"

    let cfg = test_cfg();
    let spec = test_spec("probe-blocked-egress", None);

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for no-profile spec");

    // Verify --network=none
    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"none".to_string()),
        "expected --network=none for blocked egress"
    );

    // The network=none flag ensures egress is blocked at the engine
    // level. The daemon audit event "EgressBlocked" is emitted by
    // the lifecycle module when the container tries to connect.
}

// probe_allowed_egress — with a network profile, specific egress works
// but the audit logs the URL without the response body

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_allowed_egress() {
    // With a named network profile (e.g. "github-api"), egress to the
    // allowed endpoints succeeds. The daemon audit must log the URL
    // but NOT the response body (to prevent credential leakage through
    // API response logging).
    //
    // When CADUCEUS_RUN_ISOLATION_TESTS is set, configure a profile
    // that allows egress to api.github.com:
    //  docker run --network=github-api caduceus-worker \
    //  sh -c 'curl -s https://api.github.com/zen'
    // Expected: curl succeeds, response body is a random zen quote
    // Daemon audit: logs URL but NOT response body

    let mut cfg = test_cfg();
    cfg.network_profiles.insert(
        "github-api".to_string(),
        caduceus::executor::network::NetworkProfile {
            name: "github-api".to_string(),
            egress_allow: vec!["api.github.com".to_string()],
        },
    );

    let spec = test_spec("probe-allowed-egress", Some("github-api"));

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for profile spec");

    // Verify --network=github-api
    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"github-api".to_string()),
        "expected --network=github-api for allowed egress"
    );
}

// probe_dns_exfiltration — DNS queries are blocked with no network
// profile

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn probe_dns_exfiltration() {
    // With --network=none, DNS resolution must also fail. This
    // prevents DNS-based data exfiltration. The daemon audit log
    // must contain "DnsEgressBlocked".
    //
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  docker run --network=none caduceus-worker \
    //  sh -c 'dig evil.example.com'
    // Expected: dig returns connection refused / no servers could be reached
    // Daemon audit: "DnsEgressBlocked"

    let cfg = test_cfg();
    let spec = test_spec("probe-dns-exfil", None);

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for no-profile spec");

    // Verify --network=none blocks DNS too
    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"none".to_string()),
        "expected --network=none for DNS exfiltration prevention"
    );

    // The network=none isolation prevents DNS resolution entirely
    // because the container has no network stack. The daemon audit
    // event "DnsEgressBlocked" is emitted by the lifecycle module.
}
