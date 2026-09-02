//! Adversarial resource-limit tests for the OCI executor.
//!
//! The typed `ResourceLimits` fields must reach the rendered argv as
//! engine flags (`--cpus`, `--memory`, `--memory-swap`,
//! `--pids-limit` and the dual `--tmpfs` list). This is a pure
//! typed-pipeline assertion; the live cgroup-enforcement half of the
//! resource boundary lives in the gated live suite
//! (`oci_isolation_live_test.rs`: `memory_hog_oom_live`,
//! `fork_bomb_eagain_live`, `cpu_burn_throttled_live`,
//! `tmpfs_bounded_live`, `dev_shm_bounded_live`).

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
    // Swap is pinned equal to the memory limit (no swap rescue).
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
