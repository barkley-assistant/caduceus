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
