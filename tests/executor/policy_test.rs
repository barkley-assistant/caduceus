//! Isolation enforcement contract tests, rewritten against the typed
//! pipeline: `SandboxSpec` + `sandbox_renderer::render`.
//!
//! The legacy `IsolationPolicy::enforce` / `EnforcedSpec` /
//! `inject_baseline_flags` argv-mutation surface is deleted; the
//! enforcement contract it pinned (baseline flags present, every
//! engine flag before the image token, no socket/device escape) is now
//! a structural property of the closed `SandboxSpec` + renderer.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, RuntimeFacts, SandboxEngine, SandboxSpec};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::{Config, SandboxNetwork};

/// Resolve a spec from a config (paths under its workdir_base).
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

// baseline_enforced — argv has --user, --cap-drop ALL, --security-opt
// no-new-privileges, --read-only, --tmpfs; no docker.sock; no --device

#[test]
fn baseline_enforced() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);

    assert!(
        argv.iter().any(|a| a == "--user"),
        "argv must contain --user"
    );
    assert!(
        argv.iter().any(|a| a == "1000:1000"),
        "argv must contain the fixed 1000:1000 identity"
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
    // The closed type has no escape hatch for sockets or devices.
    assert!(
        !argv.iter().any(|a| a.contains("docker.sock")),
        "argv must not contain docker.sock"
    );
    assert!(
        !argv.iter().any(|a| a == "--device"),
        "argv must not contain --device"
    );
}

// EXEC-002 positional regression: every engine flag must appear before
// the image token. The image position is structural (right after
// --entrypoint), so this can never regress — pinned here anyway.

#[test]
fn isolation_flags_precede_image() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);

    let image_idx = argv
        .iter()
        .position(|a| a.contains("@sha256:"))
        .expect("image token must be present");
    let engine_flags = [
        "--user",
        "1000:1000",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--read-only",
        "--tmpfs",
        "--network",
        "none",
        "--cpus",
        "--memory",
        "--pids-limit",
        "--shm-size",
        "--name",
        "-v",
        "-e",
        "-l",
        "--entrypoint",
    ];
    for flag in &engine_flags {
        if let Some(pos) = argv.iter().position(|a| a == *flag) {
            assert!(
                pos < image_idx,
                "engine flag {flag} at index {pos} must be before image at index {image_idx}: {argv:?}"
            );
        }
    }
    let tmpfs_target_idx = argv
        .iter()
        .position(|a| a.starts_with("/tmp:size="))
        .expect("tmpfs target must be present");
    assert!(
        tmpfs_target_idx < image_idx,
        "tmpfs target at index {tmpfs_target_idx} must be before image at index {image_idx}: {argv:?}"
    );
}

// network_mode_applied — unrestricted network → --network host

#[test]
fn network_mode_applied() {
    let mut cfg = default_cfg();
    cfg.sandbox.as_mut().unwrap().network = SandboxNetwork::Unrestricted;
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let network_pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[network_pos + 1], "host");
}

// no_network_mode_gives_none — default network mode → --network=none

#[test]
fn no_network_mode_gives_none() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let network_pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[network_pos + 1], "none");
}

// git-less worker — .git is not mounted

#[test]
fn git_less_worker() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let git_refs: Vec<&String> = argv.iter().filter(|a| a.contains(".git")).collect();
    assert!(
        git_refs.is_empty(),
        "unexpected .git reference in argv: {git_refs:?}"
    );
}

// resources always rendered — cpus/memory/pids/shm are total fields

#[test]
fn resources_always_rendered() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--cpus"));
    assert!(argv.iter().any(|a| a == "--memory"));
    assert!(argv.iter().any(|a| a == "--pids-limit"));
    assert!(argv.iter().any(|a| a == "--shm-size"));
}
