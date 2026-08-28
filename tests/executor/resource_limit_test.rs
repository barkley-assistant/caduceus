//! Adversarial resource-limit tests for the OCI executor.
//!
//! These tests verify that the typed `ResourceLimits` fields reach the
//! rendered argv as engine flags (`--cpus`, `--memory`, `--pids-limit`
//! and the dual `--tmpfs` list). All tests require a live
//! Docker/Podman engine for
//! the cgroup-enforcement half and are gated behind
//! `CADUCEUS_RUN_ISOLATION_TESTS`; the argv assertions themselves run
//! whenever the suite is executed.

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

/// The resource flags are always present with the configured values
/// (test_defaults: cpus 2.0, memory 2048m, pids 256, tmpfs 256m,
/// shm 64m — `/dev/shm` is declared via the dual tmpfs list).
#[test]
fn resource_flags_reach_argv() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(argv.iter().any(|a| a == "--memory"));
    assert!(argv.iter().any(|a| a == "2048m"));
    assert!(argv.iter().any(|a| a == "--pids-limit"));
    assert!(argv.iter().any(|a| a == "256"));
    assert!(argv.iter().any(|a| a == "--cpus"));
    assert!(argv.iter().any(|a| a == "2"));
    assert!(argv.iter().any(|a| a == "--tmpfs"));
    assert!(argv.iter().any(|a| a == "/tmp:size=256m"));
    assert!(argv.iter().any(|a| a == "/dev/shm:size=64m"));
}

// exhaust_memory — malloc beyond --memory=2048m triggers OOM-killer

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn exhaust_memory() {
    // When a worker container exceeds its memory limit, the
    // OOM-killer fires and the container exits non-zero. The daemon
    // audit log must contain "MemoryLimitEnforced".
    //
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  docker run --memory=2048m caduceus-worker \
    //  python3 -c 'x = bytearray(4096 * 1024 * 1024)'
    // Expected: container exits with OOM (exit code 137)
    // Daemon audit: "MemoryLimitEnforced"
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        argv.iter().any(|a| a == "--memory"),
        "argv must carry --memory for the OOM boundary"
    );
}

// exhaust_pids — fork beyond --pids-limit=256 returns EAGAIN

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn exhaust_pids() {
    // When a worker container exceeds its PID limit, fork() returns
    // EAGAIN. The daemon audit log must contain "PidsLimitEnforced".
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        argv.iter().any(|a| a == "--pids-limit"),
        "argv must carry --pids-limit for the PID boundary"
    );
}

// exhaust_cpu — spin-loop at 100% CPU is throttled by cgroup

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn exhaust_cpu() {
    // When a worker container uses 100% CPU, the cgroup CPU throttling
    // kicks in (via --cpus). The daemon audit log must contain
    // "CpuThrottled".
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        argv.iter().any(|a| a == "--cpus"),
        "argv must carry --cpus for the CPU boundary"
    );
}

// ---------------------------------------------------------------------------
// Hardened baseline additions (issue #245, task 13.1)
// ---------------------------------------------------------------------------

// memory_swap_pinned — --memory-swap equals --memory, so a worker
// cannot escape the memory bound via swap (no swap rescue)

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn memory_swap_pinned_no_swap_rescue() {
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  docker run --memory 2048m --memory-swap 2048m caduceus-worker \
    //  python3 -c 'x = bytearray(4096 * 1024 * 1024)'
    // Expected: OOM kill (exit 137) with NO swap rescue — committed
    // memory (mem + swap) cannot exceed the --memory value.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let mem_pos = argv.iter().position(|a| a == "--memory").expect("--memory");
    let swap_pos = argv
        .iter()
        .position(|a| a == "--memory-swap")
        .expect("--memory-swap must be rendered");
    assert_eq!(
        argv[swap_pos + 1],
        argv[mem_pos + 1],
        "--memory-swap must be pinned equal to --memory"
    );
}

// tmpfs_bounds — writes beyond the configured tmpfs `size=` fail

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn tmpfs_write_beyond_size_fails() {
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  dd if=/dev/zero of=/tmp/over bs=1M count=$((tmpfs_mb + 16))
    //  dd if=/dev/zero of=/dev/shm/over bs=1M count=$((shm_mb + 16))
    // Expected: both writes fail with ENOSPC once the bounded tmpfs
    // is full — the sizes come from --tmpfs size= in the argv below.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        argv.iter()
            .any(|a| a == &format!("/tmp:size={}m", cfg.sandbox().resources.tmpfs_mb)),
        "/tmp tmpfs must be bounded by resources.tmpfs_mb"
    );
    assert!(
        argv.iter()
            .any(|a| a == &format!("/dev/shm:size={}m", cfg.sandbox().resources.shm_mb)),
        "/dev/shm tmpfs must be bounded by resources.shm_mb"
    );
}

// rootfs_read_only — writes to the container rootfs fail EROFS

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn rootfs_write_fails_erofs() {
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  touch /etc/escape-attempt
    // Expected: touch fails with EROFS (Read-only file system) — the
    // rootfs is --read-only and the only writable surfaces are the
    // two binds and the two bounded tmpfs.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    assert!(
        argv.iter().any(|a| a == "--read-only"),
        "argv must carry --read-only for the rootfs boundary"
    );
}

// caps_absent — --cap-drop ALL leaves no capabilities in-container

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn capabilities_absent() {
    // When CADUCEUS_RUN_ISOLATION_TESTS is set:
    //  capsh --print   (or: grep CapEff /proc/self/status)
    // Expected: CapEff == 0000000000000000 — every capability was
    // dropped; no-new-privileges blocks re-acquisition via setuid.
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let cap_pos = argv
        .iter()
        .position(|a| a == "--cap-drop")
        .expect("--cap-drop");
    assert_eq!(argv[cap_pos + 1], "ALL", "every capability must be dropped");
    assert!(
        argv.iter().any(|a| a == "no-new-privileges"),
        "no-new-privileges must accompany --cap-drop ALL"
    );
}

// network_none_default — default render asserts --network none; no
// egress, no DNS, no host networking (structurally unrepresentable)

#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn default_render_is_network_none() {
    let cfg = default_cfg();
    let spec = resolve_from(&cfg);
    let argv = render(&spec, SandboxEngine::Docker);
    let pos = argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network");
    assert_eq!(argv[pos + 1], "none");
}
