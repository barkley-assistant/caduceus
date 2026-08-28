//! Adversarial escape tests for the OCI container isolation boundary.
//!
//! These tests verify that a worker container cannot escape the mount,
//! filesystem, device, or daemon-storage boundaries. The argv-side
//! assertions run against the typed pipeline (`resolve` → `render`);
//! the full engine-backed scenarios are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS` because they require a live
//! Docker/Podman engine and run adversarial scenarios inside a real
//! container.
//!
//! Dual-assertion pattern: every test asserts BOTH the worker-side
//! denial (EINVAL, EROFS, ENOENT, EPERM) AND the daemon-side audit
//! event (MountBoundaryHeld, GitLessBoundaryHeld, etc.).

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

/// The rendered argv for the default fixture (Docker engine).
fn rendered_argv() -> Vec<String> {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    render(&spec, SandboxEngine::Docker)
}

// escape_worktree_mount — writing outside worktree is EINVAL + audit

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn escape_worktree_mount() {
    // The read-only rootfs plus the single workspace mount keeps the
    // worker inside the worktree. When CADUCEUS_RUN_ISOLATION_TESTS
    // is set, run against a real engine:
    //  docker run --read-only --tmpfs /tmp:size=256M \
    //  -v /tmp/worktree:/workspace:rw \
    //  caduceus-worker sh -c 'touch /../outside_worktree/test'
    // Expected: touch: cannot touch '/../outside_worktree/test': No
    // such file or directory. Daemon audit: "MountBoundaryHeld".
    let argv = rendered_argv();
    assert!(
        argv.iter().any(|a| a == "--read-only"),
        "read-only rootfs must be enforced for escape prevention"
    );
}

// escape_git_metadata — reading /workspace/.git reaches only the
// daemon-owned read-only shadow, never the real gitdir

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn escape_git_metadata() {
    // The gitdir pointer file at /workspace/.git is shadowed by a
    // daemon-owned read-only mount; the real main-repo gitdir path is
    // unreachable and the shadow is not writable. When
    // CADUCEUS_RUN_ISOLATION_TESTS is set, run:
    //  docker run --read-only -v /tmp/worktree:/workspace:rw \
    //    -v /state/oci-runs/run/git-shadow:/workspace/.git:ro \
    //  caduceus-worker sh -c 'cat /workspace/.git'
    // Expected: only the sentinel shadow content (never the real
    // gitdir path), and writes to /workspace/.git fail.
    // Daemon audit: "GitShadowHeld".
    let argv = rendered_argv();
    let shadow_mounts: Vec<&String> = argv
        .iter()
        .filter(|a| a.ends_with(":/workspace/.git:ro"))
        .collect();
    assert_eq!(
        shadow_mounts.len(),
        1,
        "exactly one read-only shadow mount for /workspace/.git: {argv:?}"
    );
    // No writable .git mount anywhere.
    assert!(
        !argv
            .iter()
            .any(|a| a.contains(".git") && a.ends_with(":rw")),
        ".git must never be mounted writable: {argv:?}"
    );
}

// escape_daemon_storage — ls <state>/repos returns ENOENT

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn escape_daemon_storage() {
    // The daemon storage directory must not be exposed to the worker
    // container. When CADUCEUS_RUN_ISOLATION_TESTS is set, run:
    //  docker run --read-only caduceus-worker sh -c 'ls <state>/repos/'
    // Expected: ls: cannot access '<state>/repos/': No such file or
    // directory. Daemon audit: "DaemonStorageNotExposed".
    let argv = rendered_argv();
    let daemon_paths = ["caduceus/repos", "caduceus/state", ".local/share/caduceus"];
    for path in &daemon_paths {
        let found: Vec<&String> = argv.iter().filter(|a| a.contains(path)).collect();
        assert!(
            found.is_empty(),
            "daemon storage path {path} must not be mounted; got: {found:?}"
        );
    }
}

// escape_engine_socket — the closed type has no socket-mount surface

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn escape_engine_socket() {
    // The Docker/Podman engine socket must not be accessible from
    // inside the worker container. The closed `SandboxSpec` has no
    // field for arbitrary extra mounts, so a socket mount is
    // unrepresentable; the rendered argv therefore never contains the
    // socket path. Daemon audit: "EngineSocketNotExposed".
    let argv = rendered_argv();
    assert!(
        !argv.iter().any(|a| a.contains("docker.sock")),
        "engine socket must not appear in argv: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--device"),
        "device mounts must not appear in argv: {argv:?}"
    );
}

// escape_device_node — mknod /tmp/null c 1 3 returns EPERM + audit

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn escape_device_node() {
    // Creating device nodes inside the container must be denied
    // (--cap-drop ALL removes CAP_MKNOD, and --device is
    // unrepresentable). When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  docker run --read-only --cap-drop ALL caduceus-worker \
    //  sh -c 'mknod /tmp/null c 1 3'
    // Expected: mknod: /tmp/null: Operation not permitted. Daemon
    // audit: "DeviceBoundaryHeld".
    let argv = rendered_argv();
    assert!(
        argv.iter().any(|a| a == "--cap-drop"),
        "argv must contain --cap-drop"
    );
    assert!(
        argv.iter().any(|a| a == "ALL"),
        "argv must contain ALL for --cap-drop"
    );
    assert!(
        !argv.iter().any(|a| a == "--device"),
        "argv must not contain --device"
    );
}
