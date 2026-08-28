//! Isolation enforcement contract tests, rewritten against the typed
//! pipeline: `SandboxSpec` + `sandbox_renderer::render`.
//!
//! The legacy `IsolationPolicy::enforce` / `EnforcedSpec` /
//! `inject_baseline_flags` argv-mutation surface is deleted; the
//! enforcement contract it pinned (baseline flags present, every
//! engine flag before the image token, no socket/device escape) is now
//! a structural property of the closed `SandboxSpec` + renderer.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, SandboxEngine, SandboxSpec};
use caduceus::infra::config::Config;

mod support;

/// Resolve a spec from a config (paths under its workdir_base).
fn resolve_from(cfg: &Config) -> SandboxSpec {
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let runtime = support::runtime_facts(cfg, "run-001", &worktree);
    resolve(cfg.sandbox(), &runtime, &support::executor_spec(&runtime)).expect("must resolve")
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
        argv.iter().any(|a| a == "4242:4242"),
        "argv must contain the resolved worktree-owner identity"
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
        "4242:4242",
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

// network_mode_applied — the only network mode is `none` (host
// networking was removed, breaking: issue #245)

#[test]
fn network_mode_applied() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let network_pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[network_pos + 1], "none");
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

// git-shadow worker — `.git` is shadowed read-only, never writable

#[test]
fn git_less_worker() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let git_refs: Vec<&String> = argv.iter().filter(|a| a.contains(".git")).collect();
    // The only `.git` reference is the daemon-owned read-only shadow
    // at the fixed canonical path — the real gitdir is unreachable
    // and `/workspace/.git` is never writable.
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

// resources always rendered — cpus/memory/pids are total fields; the
// tmpfs pair covers the ephemeral surfaces

#[test]
fn resources_always_rendered() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--cpus"));
    assert!(argv.iter().any(|a| a == "--memory"));
    assert!(argv.iter().any(|a| a == "--pids-limit"));
    assert!(argv.iter().any(|a| a == "--tmpfs"));
    assert!(argv.iter().any(|a| a == "/dev/shm:size=64m"));
}
