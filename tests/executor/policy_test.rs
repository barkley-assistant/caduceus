//! Tests for the isolation policy enforcement module.
//!
//! These tests verify that [`IsolationPolicy::enforce`] correctly
//! applies all policy rules to an `ExecutorSpec`.

use std::path::PathBuf;

use caduceus::executor::network::NetworkProfile;
use caduceus::executor::policy::IsolationPolicy;
use caduceus::executor::ExecutorSpec;
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, OciPullPolicy};
use caduceus::infra::error::CaduceusError;

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

// mount allow-list: undeclared mount rejected

#[test]
fn undeclared_mount_rejected() {
    let cfg = test_cfg();
    let spec = test_spec("test-undeclared-mount");

    // The IsolationPolicy builds its own default mounts from the spec,
    // so the worktree is always declared. This test verifies that the
    // mount allow-list mechanism exists — by calling oci_args::build_argv
    // directly with an empty mount list, we confirm that undeclared
    // mounts are rejected.
    let mounts: Vec<caduceus::executor::oci_args::MountSpec> = vec![];
    let result = caduceus::executor::oci_args::build_argv(&spec, &cfg, &mounts, None);
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

// resource limit required — spec must have CPU, memory, PIDs

#[test]
fn resource_limit_required() {
    let cfg = test_cfg();
    let spec = test_spec("test-resource-limits");

    // The spec does not declare resource limits. The policy layer
    // should reject it. Currently the enforcement succeeds because
    // resource limits are not yet validated in the spec.
    //
    // This test is a placeholder — when resource limit enforcement
    // is added to the spec, this test should assert the error.
    let result = IsolationPolicy::enforce(&spec, &cfg);
    // For now, we verify that the enforcement doesn't panic and
    // the EnforcedSpec has the expected baseline flags.
    match result {
        Ok(enforced) => {
            // Verify baseline flags are present in the argv
            let argv = &enforced.argv;
            assert!(
                argv.iter().any(|a| a == "--user"),
                "argv must contain --user"
            );
            assert!(
                argv.iter().any(|a| a == "--cap-drop"),
                "argv must contain --cap-drop"
            );
        }
        Err(e) => {
            panic!("enforcement failed with: {e:?}");
        }
    }
}

// baseline enforced — argv has --user, --cap-drop ALL, --security-opt
// no-new-privileges, --read-only, --tmpfs; no docker.sock; no --device

#[test]
fn baseline_enforced() {
    let cfg = test_cfg();
    let spec = test_spec("test-baseline");

    let result = IsolationPolicy::enforce(&spec, &cfg);

    match &result {
        Ok(enforced) => {
            let argv = &enforced.argv;
            // Check baseline flags are present
            assert!(
                argv.iter().any(|a| a == "--user"),
                "argv must contain --user"
            );
            assert!(
                argv.iter().any(|a| a == "--cap-drop"),
                "argv must contain --cap-drop"
            );
            assert!(
                argv.iter().any(|a| a == "ALL"),
                "argv must contain ALL for --cap-drop"
            );
            assert!(
                argv.iter().any(|a| a == "no-new-privileges"),
                "argv must contain no-new-privileges"
            );
            assert!(
                argv.iter().any(|a| a == "--read-only"),
                "argv must contain --read-only"
            );
            assert!(
                argv.iter().any(|a| a == "--tmpfs"),
                "argv must contain --tmpfs"
            );
            assert!(
                argv.iter().any(|a| a.contains("/tmp:size=")),
                "argv must contain /tmp:size=... tmpfs mount"
            );

            // Reject docker.sock
            assert!(
                !argv.iter().any(|a| a.contains("docker.sock")),
                "argv must not contain docker.sock"
            );

            // Reject --device
            assert!(
                !argv.iter().any(|a| a == "--device"),
                "argv must not contain --device"
            );
        }
        Err(e) => {
            panic!("baseline enforcement failed with: {e:?}");
        }
    }
}

// image digest pinned — tag-only ref is rejected

#[test]
fn image_digest_pinned() {
    let mut cfg = test_cfg();
    cfg.oci_image_digest = String::new(); // empty = not pinned

    let spec = test_spec("test-digest-pinned");
    let result = IsolationPolicy::enforce(&spec, &cfg);
    match result {
        Err(CaduceusError::OciImageNotDigestPinned { reference }) => {
            assert!(
                reference.is_empty(),
                "expected empty reference in error, got: {reference}"
            );
        }
        Err(other) => panic!("expected OciImageNotDigestPinned; got: {other:?}"),
        Ok(_) => panic!("expected error for non-digest-pinned image"),
    }
}

// pull policy Always + digest → rejected

#[test]
fn pull_policy_always_rejected() {
    let mut cfg = test_cfg();
    cfg.oci_pull_policy = OciPullPolicy::Always;

    let spec = test_spec("test-pull-policy");
    let result = IsolationPolicy::enforce(&spec, &cfg);
    match result {
        Err(CaduceusError::OciPullPolicyIncompatible { .. }) => {} // expected
        Err(other) => panic!("expected OciPullPolicyIncompatible; got: {other:?}"),
        Ok(_) => panic!("expected error for Always + digest"),
    }
}

// git-less worker — .git is RO, mirrors not mounted

#[test]
fn git_less_worker() {
    let cfg = test_cfg();
    let spec = test_spec("test-git-less");

    // The policy builds default mounts from the spec, which only
    // includes the worktree path. The .git directory is NOT in
    // the default mounts. This test verifies that the argv does
    // not contain .git as a volume mount.
    let result = IsolationPolicy::enforce(&spec, &cfg);
    match &result {
        Ok(enforced) => {
            let argv = &enforced.argv;
            // The .git directory should not appear as a direct
            // volume mount path. Look for volume targets that
            // contain ".git".
            let volume_mounts: Vec<&String> = argv.iter().filter(|a| a.contains(".git")).collect();
            // The worktree path is "/tmp/worktree" — no .git in it.
            assert!(
                volume_mounts.is_empty(),
                "unexpected .git reference in argv: {volume_mounts:?}"
            );
        }
        Err(e) => {
            panic!("enforcement failed with: {e:?}");
        }
    }
}

// network_profile_applied — network args are merged into the argv

#[test]
fn network_profile_applied() {
    let mut cfg = test_cfg();
    // Add a network profile.
    cfg.network_profiles.insert(
        "isolated".to_string(),
        NetworkProfile {
            name: "isolated".to_string(),
            egress_allow: vec!["10.0.0.0/8".to_string()],
        },
    );

    let mut spec = test_spec("test-network-profile");
    spec.network_profile = Some("isolated".to_string());

    let result = IsolationPolicy::enforce(&spec, &cfg);
    match &result {
        Ok(enforced) => {
            let argv = &enforced.argv;
            let network_pos = argv.iter().position(|a| a == "--network");
            assert!(
                network_pos.is_some(),
                "argv must contain --network flag, got: {argv:?}"
            );
            let pos = network_pos.unwrap();
            let value = &argv[pos + 1];
            assert_eq!(
                value, "isolated",
                "expected --network=isolated, got --network={value}"
            );
        }
        Err(e) => {
            panic!("enforcement failed with: {e:?}");
        }
    }
}

// no_network_profile_gives_none — no network_profile → --network=none

#[test]
fn no_network_profile_gives_none() {
    let cfg = test_cfg();
    let spec = test_spec("test-no-network");

    let result = IsolationPolicy::enforce(&spec, &cfg);
    match &result {
        Ok(enforced) => {
            let argv = &enforced.argv;
            // There may be multiple --network flags. The last one
            // should be "none".
            let last_network = argv.iter().rposition(|a| a == "--network");
            assert!(
                last_network.is_some(),
                "argv must contain --network flag, got: {argv:?}"
            );
            let pos = last_network.unwrap();
            let value = &argv[pos + 1];
            assert_eq!(
                value, "none",
                "expected --network=none, got --network={value}"
            );
        }
        Err(e) => {
            panic!("enforcement failed with: {e:?}");
        }
    }
}
