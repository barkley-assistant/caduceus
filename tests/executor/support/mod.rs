//! Shared `RuntimeFacts` fixture for the executor test crates.
//!
//! `RuntimeFacts` gained the pre-flight fields (owner uid/gid, engine
//! mode, `.git` shadow kind and host path, state dir) that keep
//! `resolve` pure; this module gives every executor test one place
//! with sensible defaults so the per-case tests stay one-line
//! adjustable (design D8).
//!
//! Defaults: non-1000 worktree owner `4242:4242` (asserting dynamic
//! identity must never assume 1000), rootful engine mode, and the
//! `gitdir:` pointer-file `.git` shadow kind. The daemon-owned
//! `output_dir` / `git_shadow_host` paths mirror the pre-flight
//! derivation under `cfg.state_dir`. Tests needing non-default facts
//! mutate the returned struct's public fields directly.

use std::path::{Path, PathBuf};

use caduceus::executor::sandbox_spec::{EngineMode, GitShadowKind, RuntimeFacts};
use caduceus::github::issue::IssueKey;
use caduceus::infra::config::Config;

/// Daemon-owned `/output` host path for a run (mirrors the pre-flight
/// derivation in `engine_probe`).
pub fn oci_output_dir(cfg: &Config, run_id: &str) -> PathBuf {
    cfg.state_dir.join("oci-runs").join(run_id).join("output")
}

/// Daemon-owned `.git` shadow host path for a run (mirrors the
/// pre-flight derivation in `engine_probe`).
pub fn git_shadow_host(cfg: &Config, run_id: &str) -> PathBuf {
    cfg.state_dir
        .join("oci-runs")
        .join(run_id)
        .join("git-shadow")
}

/// Build `RuntimeFacts` with the documented defaults for `run_id`,
/// with the worktree at `worktree` (which must live under
/// `cfg.workdir_base` for the allow-list to accept it).
pub fn runtime_facts(cfg: &Config, run_id: &str, worktree: &Path) -> RuntimeFacts {
    RuntimeFacts {
        run_id: run_id.to_string(),
        issue: IssueKey::parse("owner/repo#1").expect("valid key"),
        worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
        worktree: worktree.to_path_buf(),
        output_dir: oci_output_dir(cfg, run_id),
        daemon_id: "test-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: 4242,
        worktree_gid: 4242,
        engine_mode: EngineMode::Rootful,
        git_shadow_kind: GitShadowKind::File,
        git_shadow_host: git_shadow_host(cfg, run_id),
    }
}
