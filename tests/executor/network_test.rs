//! Tests for the network policy enforcement module.
//!
//! Verifies that [`NetworkPolicy::build_network_args`] returns the
//! correct `--network` flag based on `config.sandbox().network`:
//! `none` → `--network none`, `unrestricted` → `--network host`.

use std::path::PathBuf;

use caduceus::executor::network::NetworkPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, SandboxNetwork};

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

// default_network_is_none — sandbox.network defaults to `none`

#[test]
fn default_network_is_none() {
    let cfg = test_cfg();
    let spec = test_spec("test-default-network");

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for default network");

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

// unrestricted_network_uses_host — sandbox.network = unrestricted →
// --network host

#[test]
fn unrestricted_network_uses_host() {
    let mut cfg = test_cfg();
    cfg.sandbox.as_mut().unwrap().network = SandboxNetwork::Unrestricted;

    let spec = test_spec("test-unrestricted-network");

    let args = NetworkPolicy::build_network_args(&spec, &cfg)
        .expect("network args must build for unrestricted network");

    let pos = args.iter().position(|a| a == "--network").unwrap();
    let value = args.get(pos + 1);
    assert_eq!(
        value,
        Some(&"host".to_string()),
        "expected --network=host, got: {args:?}"
    );
}
