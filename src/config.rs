//! Configuration: typed loader for the YAML configuration file and the
//! env-variable overrides listed in `CONTRACTS.md` under "Configuration".
//!
//! `Config::load()` resolves `$CADUCEUS_CONFIG`, then `$HERMES_HOME/config.yaml`,
//! then `~/.config/caduceus/config.yaml`. `Config::test_defaults` provides a
//! deterministic root used in tests.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};

/// Caduceus configuration. Field semantics are pinned in `CONTRACTS.md`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub poll_interval_seconds: u64,
    pub state_dir: PathBuf,
    pub log_path: PathBuf,
    pub workdir_base: PathBuf,
    pub watched_repos: Vec<String>,
    pub worker_command: Vec<String>,
    pub worker_timeout_seconds: u64,
    pub http_timeout_seconds: u64,
    pub git_timeout_seconds: u64,
    pub transcript_max_bytes: u64,
    pub run_retention_days: u64,
    pub stale_run_hours: u64,
    pub max_retries_per_issue: u32,
    pub retry_backoff_seconds: u64,
    pub ticket_label_code: String,
    pub ticket_label_investigation: String,
    pub feedback_author_allowlist: Vec<String>,
    pub comment_ignore_patterns: Vec<String>,
    pub comment_forbidden_strings: Vec<String>,
    pub worker_env_allowlist: Vec<String>,
    pub github_token: Option<String>,
    pub api_base: String,
    pub dry_run: bool,
}

impl Config {
    /// Resolve configuration through the canonical chain.
    pub fn load() -> CaduceusResult<Self> {
        // Task 1.1 fills this in. Stub keeps the compiler happy and lets
        // Task 0.1's clippy gate run on the rest of the workspace.
        Err(CaduceusError::Config(
            "Config::load is implemented in Task 1.1".to_string(),
        ))
    }

    /// Deterministic root-anchored defaults for tests. Avoids any host-dependent
    /// `Config::defaults()` constructor that would make tests flake.
    pub fn test_defaults(root: &Path) -> Self {
        Self {
            poll_interval_seconds: 120,
            state_dir: root.join("state"),
            log_path: root.join("state").join("processor.log"),
            workdir_base: root.join("workdirs"),
            watched_repos: Vec::new(),
            worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
            worker_timeout_seconds: 3600,
            http_timeout_seconds: 60,
            git_timeout_seconds: 300,
            transcript_max_bytes: 10 * 1024 * 1024,
            run_retention_days: 30,
            stale_run_hours: 1,
            max_retries_per_issue: 3,
            retry_backoff_seconds: 300,
            ticket_label_code: "🤖 auto-fix".to_string(),
            ticket_label_investigation: "🤖 auto-fix-investigate".to_string(),
            feedback_author_allowlist: Vec::new(),
            comment_ignore_patterns: Vec::new(),
            comment_forbidden_strings: Vec::new(),
            worker_env_allowlist: Vec::new(),
            github_token: None,
            api_base: "https://api.github.com".to_string(),
            dry_run: false,
        }
    }
}
