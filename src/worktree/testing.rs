#![allow(dead_code, unused_imports)]
use super::*;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use nix::unistd::pipe;
use tokio::process::Command as TokioCommand;
use url::Url;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{scrub, CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Test-only Config helper. `Config::test_defaults` is documented as the
// canonical root-anchored builder, but `find_main_clone` and the runner
// only need a couple of fields; this keeps the inline tests focused on
// pure logic.
// ---------------------------------------------------------------------------

impl Config {
    #[doc(hidden)]
    pub(crate) fn minimal_workdir_for_runner_tests() -> Self {
        Self {
            poll_interval_seconds: 0,
            state_dir: PathBuf::from("/tmp"),
            log_path: PathBuf::from("/tmp/processor.log"),
            workdir_base: PathBuf::from("/tmp"),
            watched_repos: Vec::new(),
            worker_command: Vec::new(),
            worker_timeout_seconds: 0,
            http_timeout_seconds: 0,
            git_timeout_seconds: 0,
            transcript_max_bytes: 0,
            run_retention_days: 0,
            stale_run_hours: 0,
            max_retries_per_issue: 0,
            retry_backoff_seconds: 0,
            ticket_label_code: String::new(),
            ticket_label_investigation: String::new(),
            feedback_author_allowlist: Vec::new(),
            comment_ignore_patterns: Vec::new(),
            comment_forbidden_strings: Vec::new(),
            worker_env_allowlist: Vec::new(),
            github_token: None,
            api_base: DEFAULT_API_BASE.to_string(),
            dry_run: false,
            worker_parallelism: 1,
            discovery_max_pages: 20,
            compiled_ignore_patterns: Vec::new(),
            scheduler_lease_ttl_seconds: 60,
            scheduler_transaction_budget_ms: 100,
            drain_timeout_seconds: 30,
            backpressure_budget_ms: 5000,
            circuit_failure_threshold: 3,
            circuit_backoff_seconds: vec![30, 120, 600],
            circuit_open_interval_seconds: 1800,
            circuit_max_degraded_seconds: 86400,
            repo_storage_root: PathBuf::from("/tmp/repos"),
            executor_mode: crate::executor::ExecutorKind::TrustedHost,
            reduced_containment_acknowledged: true,
            oci_cli: PathBuf::from("docker"),
            oci_image_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
            oci_pull_policy: crate::infra::config::OciPullPolicy::Never,
            oci_stop_timeout_seconds: crate::infra::config::DEFAULT_OCI_STOP_TIMEOUT_SECONDS,
            oci_kill_timeout_seconds: crate::infra::config::DEFAULT_OCI_KILL_TIMEOUT_SECONDS,
            oci_reconcile_timeout_seconds:
                crate::infra::config::DEFAULT_OCI_RECONCILE_TIMEOUT_SECONDS,
            network_profiles: std::collections::HashMap::new(),
            secret_grants: Vec::new(),
            upgrade_choice: None,
        }
    }
}

const DEFAULT_API_BASE: &str = "https://api.github.com";

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn parse_origin_handles_https_form() {
        let (owner, repo) = parse_origin("https://github.com/octocat/Hello-World.git").unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "Hello-World");
    }

    #[test]
    fn parse_origin_handles_ssh_form() {
        let (owner, repo) = parse_origin("git@github.com:octocat/Hello-World.git").unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "Hello-World");
    }

    #[test]
    fn validate_origin_host_accepts_matching_github_com() {
        validate_origin_host(
            "https://github.com/octocat/Hello-World.git",
            DEFAULT_API_BASE,
        )
        .unwrap();
    }

    #[test]
    fn validate_origin_host_rejects_mismatched_enterprise_host() {
        let err = validate_origin_host(
            "https://github.com/octocat/Hello-World.git",
            "https://ghe.example.com",
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains("origin host"));
    }

    #[test]
    fn validate_origin_host_rejects_ssh_alias() {
        let err = validate_origin_host(
            "git@github.com-attacker:octocat/Hello-World.git",
            DEFAULT_API_BASE,
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains("alias"), "got: {text}");
    }

    #[test]
    fn cap_text_truncates_with_marker() {
        let huge = "x".repeat(GIT_OUTPUT_BYTE_CAP + 100);
        let capped = cap_text(huge.as_bytes());
        assert!(capped.contains("truncated"));
        assert!(capped.len() <= GIT_OUTPUT_BYTE_CAP + 64);
    }

    #[test]
    fn redact_and_cap_strips_token_shaped_substrings() {
        let raw = b"some output\nGITHUB_TOKEN=ghp_should_not_leak\nrest";
        let redacted = redact_and_cap(raw);
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains("ghp_should_not_leak"));
    }

    #[test]
    fn clone_path_is_workdir_base_plus_owner_plus_repo() {
        let cfg = Config {
            workdir_base: PathBuf::from("/srv/workdirs"),
            ..Config::minimal_workdir_for_runner_tests()
        };
        let key = IssueKey {
            owner: "octocat".to_string(),
            repo: "Hello-World".to_string(),
            number: 1,
        };
        assert_eq!(
            clone_path(&cfg, &key),
            PathBuf::from("/srv/workdirs/octocat/Hello-World")
        );
    }
}
