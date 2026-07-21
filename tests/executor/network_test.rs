//! Tests for the network policy enforcement module.
//!
//! Verifies that [`NetworkPolicy::build_network_args`] returns the
//! correct `--network` flag based on the spec's network profile.

use std::path::PathBuf;

use caduceus::executor::network::NetworkPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;

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

// ---------------------------------------------------------------------------
// no_profile_denies_network — no profile → --network=none
// ---------------------------------------------------------------------------

#[test]
fn no_profile_denies_network() {
    let cfg = test_cfg();
    let spec = test_spec("test-no-network", None);

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for no-profile spec");

    assert!(
        args.contains(&"--network".to_string()),
        "expected --network flag, got: {args:?}"
    );

    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"none".to_string()),
        "expected --network=none, got: {args:?}"
    );
}

// ---------------------------------------------------------------------------
// named_profile_network — named profile → --network=<profile_name>
// ---------------------------------------------------------------------------

#[test]
fn named_profile_network() {
    let cfg = test_cfg();
    let spec = test_spec("test-named-network", Some("my-bridge"));

    // The profile "my-bridge" is not in the config.network_profiles
    // map, so we expect OciNetworkNotInProfile error.
    let result = NetworkPolicy::build_network_args(&spec, &cfg);
    match result {
        Err(CaduceusError::OciNetworkNotInProfile { profile }) => {
            assert_eq!(profile, "my-bridge");
        }
        Err(other) => panic!("expected OciNetworkNotInProfile; got: {other:?}"),
        Ok(args) => {
            panic!("expected error for unknown profile, got args: {args:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// named_profile_network_with_config — profile in config → --network=<name>
// ---------------------------------------------------------------------------

#[test]
fn named_profile_network_with_config() {
    let mut cfg = test_cfg();

    // Add a network profile to config.
    cfg.network_profiles.insert(
        "my-bridge".to_string(),
        caduceus::executor::network::NetworkProfile {
            name: "my-bridge".to_string(),
            egress_allow: vec!["10.0.0.0/8".to_string()],
        },
    );

    let spec = test_spec("test-named-network-ok", Some("my-bridge"));

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for known profile");

    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"my-bridge".to_string()),
        "expected --network=my-bridge, got: {args:?}"
    );
}
