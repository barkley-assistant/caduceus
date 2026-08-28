//! Configuration: typed loader for the YAML configuration file and the
//! env-variable overrides.
//!
//! The public [`Config`] is the daemon's canonical view. It is built
//! from a private [`RawConfig`] deserialisation layer that keeps
//! `worker_command` optional — the daemon resolves the user-owned
//! bridge default once it knows where it is loaded from. All
//! validation (regex compilation, allowlist syntax, repo slug
//! validation, durations, label uniqueness, GitHub-credential denial)
//! happens in [`Config::from_raw`] so callers see one consolidated
//! `CaduceusError::Config` instead of scattered parse errors.
//!
//! Tests must use [`Config::test_defaults`] rooted at a temp dir; the
//! daemon never relies on a host-dependent `Config::defaults()`
//! constructor.
//!
//! Public field list, semantics, and defaults are pinned here —
//! every field documented must be present.
//!
//! `git_author_name` and `git_author_email` are optional `String` keys in the
//! `caduceus:` block. An absent or empty value falls through to host git
//! config and then the last-resort daemon identity.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::executor::SandboxEngine;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// GitHub credential variable names that must never appear in the
/// worker environment allowlist, even if the operator explicitly adds
/// them. Source: the worker-result contract in `src/worker/worker_contract.rs`.
pub const DENIED_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"];

/// Worker command tokens that are always rejected as interpolation.
const FORBIDDEN_INTERPOLATION_TOKENS: &[&str] = &["$HOME", "${HOME}", "~", "$USER"];

/// The exact token that *is* allowed as worker-command interpolation.
pub const PLUGIN_ROOT_TOKEN: &str = "${plugin_root}";

/// Default values the daemon falls back to when an operator omits a
/// field.
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 120;
pub const DEFAULT_WORKER_TIMEOUT_SECONDS: u64 = 3600;
pub const DEFAULT_HTTP_TIMEOUT_SECONDS: u64 = 60;
pub const DEFAULT_GIT_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_TRANSCRIPT_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_RUN_RETENTION_DAYS: u64 = 30;
pub const DEFAULT_STALE_RUN_HOURS: u64 = 1;
pub const DEFAULT_WORKTREE_GC_OLDER_THAN_DAYS: u64 = 1;
pub const DEFAULT_WORKTREE_GC_DISABLED: bool = false;
pub const DEFAULT_ARCHIVE_ON_RETRY: bool = false;
pub const DEFAULT_ATTIC_RETENTION_DAYS: u64 = 30;
pub const DEFAULT_MAX_RETRIES_PER_ISSUE: u32 = 3;
pub const DEFAULT_RETRY_BACKOFF_SECONDS: u64 = 300;
pub const DEFAULT_TICKET_LABEL_CODE: &str = "🤖 auto-fix";
pub const DEFAULT_TICKET_LABEL_INVESTIGATION: &str = "🤖 auto-fix-investigate";
pub const DEFAULT_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_WORKER_PARALLELISM: u32 = 1;
/// Multiplier applied to `worker_parallelism` to derive the default
/// `max_issues_per_tick` cap. Bounded but generous — a tick with
/// `worker_parallelism: N` processes up to `N * 4` issues, leaving
/// the rest for the next tick (issue #108).
pub const DEFAULT_MAX_ISSUES_PER_TICK_MULTIPLIER: u32 = 4;
pub const DEFAULT_SCHEDULER_LEASE_TTL_SECONDS: u64 = 60;
pub const DEFAULT_WORKER_LEASE_TTL_SECONDS: u64 = 600;
pub const DEFAULT_SCHEDULER_TRANSACTION_BUDGET_MS: u64 = 100;
pub const DEFAULT_DRAIN_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_BACKPRESSURE_BUDGET_MS: u64 = 5000;
pub const DEFAULT_CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
pub const DEFAULT_CIRCUIT_BACKOFF_SECONDS: &[u64] = &[30, 120, 600];
pub const DEFAULT_CIRCUIT_OPEN_INTERVAL_SECONDS: u64 = 1800;
pub const DEFAULT_CIRCUIT_MAX_DEGRADED_SECONDS: u64 = 86400;
pub const DEFAULT_DISCOVERY_MAX_PAGES: u32 = 20;
pub const DEFAULT_REPO_STORAGE_ROOT: &str = "repos";
pub const DEFAULT_STATE_BACKEND: &str = "json";
pub const DEFAULT_EXECUTOR_MODE: crate::executor::ExecutorKind =
    crate::executor::ExecutorKind::TrustedHost;

/// OCI image pull policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OciPullPolicy {
    Never,
    #[default]
    IfMissing,
    Always,
}

pub const DEFAULT_SANDBOX_STOP_TIMEOUT_SECONDS: u64 = 10;
pub const DEFAULT_SANDBOX_KILL_TIMEOUT_SECONDS: u64 = 5;
pub const DEFAULT_SANDBOX_RECONCILE_TIMEOUT_SECONDS: u64 = 60;
pub const DEFAULT_SANDBOX_RESERVED_HOST_DISK_MB: u64 = 2048;

/// Resolved sandbox section. `image` is required — no default.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub engine: SandboxEngine,
    /// Full immutable image reference `name@sha256:<64 hex>`.
    pub image: String,
    pub pull_policy: OciPullPolicy,
    pub resources: SandboxResources,
    pub network: SandboxNetwork,
    pub pass_env: Vec<String>,
    pub stop_timeout_seconds: u64,
    pub kill_timeout_seconds: u64,
    pub reconcile_timeout_seconds: u64,
    pub reserved_host_disk_mb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxResources {
    pub cpus: f64,
    pub memory_mb: u64,
    pub pids: u64,
    pub tmpfs_mb: u64,
    pub shm_mb: u64,
}

/// New enum (replaces `network_profiles`).
///
/// Host networking is structurally unrepresentable: the former
/// `Unrestricted` variant (`--network host`) was removed (breaking,
/// issue #245). The only value is `None` (`--network none`); a YAML
/// `network: unrestricted` fails at serde parse time as an unknown
/// variant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetwork {
    #[default]
    None, // `--network none`
}

/// Raw layer — mirrors the schema with all-`Option` fields.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSandboxConfig {
    pub engine: Option<SandboxEngine>,
    pub image: Option<String>,
    pub pull_policy: Option<OciPullPolicy>,
    pub resources: Option<RawSandboxResources>,
    pub network: Option<SandboxNetwork>,
    pub pass_env: Option<Vec<String>>,
    pub stop_timeout_seconds: Option<u64>,
    pub kill_timeout_seconds: Option<u64>,
    pub reconcile_timeout_seconds: Option<u64>,
    pub reserved_host_disk_mb: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSandboxResources {
    pub cpus: Option<f64>,
    pub memory_mb: Option<u64>,
    pub pids: Option<u64>,
    pub tmpfs_mb: Option<u64>,
    pub shm_mb: Option<u64>,
}

/// Caduceus configuration. Field semantics are pinned here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub poll_interval_seconds: u64,
    pub state_dir: PathBuf,
    pub state_backend: String,
    pub log_path: PathBuf,
    pub workdir_base: PathBuf,
    pub watched_repos: Vec<String>,
    pub worker_command: Vec<String>,
    /// Optional operator instruction injected into the worker prompt.
    /// Empty string means no section is rendered.
    pub worker_instruction: String,
    pub worker_timeout_seconds: u64,
    pub http_timeout_seconds: u64,
    pub git_timeout_seconds: u64,
    pub transcript_max_bytes: u64,
    pub run_retention_days: u64,
    pub stale_run_hours: u64,
    /// Days of inactivity before a worktree is eligible for
    /// automatic GC. Default 1; set to a higher value to keep
    /// stale worktrees around longer.
    pub worktree_gc_older_than_days: u64,
    /// Disables the daemon's automatic worktree GC step entirely.
    /// Default `false`.
    pub worktree_gc_disabled: bool,
    /// Whether to archive a failed run's working tree to the attic
    /// before removing it on retry. Default `false`.
    pub archive_on_retry: bool,
    /// Maximum age of archived working trees in the daemon's attic
    /// before they are eligible for automatic pruning. Default 30.
    pub attic_retention_days: u64,
    pub max_retries_per_issue: u32,
    pub retry_backoff_seconds: u64,
    pub ticket_label_code: String,
    pub ticket_label_investigation: String,
    /// Whether to remove the trigger label from an issue once the
    /// run reaches a terminal-success state. Default `true`; set to
    /// `false` to keep the label for manual visibility.
    pub remove_label_on_completion: bool,
    pub feedback_author_allowlist: Vec<String>,
    pub comment_ignore_patterns: Vec<String>,
    pub comment_forbidden_strings: Vec<String>,
    pub worker_env_allowlist: Vec<String>,
    pub github_token: Option<String>,
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub api_base: String,
    pub dry_run: bool,
    /// Maximum number of concurrent worker processes. Default 1.
    pub worker_parallelism: u32,
    /// Maximum number of queue entries a single tick will claim
    /// before returning (issue #108). `0` means unbounded — the
    /// pre-#108 drain-the-queue behavior. Default
    /// `worker_parallelism * 4`, so unbounded is never accidental.
    /// The JoinSet drain still runs to completion on tick exit;
    /// this cap only stops claiming new entries and keeps
    /// `contract/SCHED-001` (bounded single-host concurrency)
    /// bounded across a long cron tick.
    pub max_issues_per_tick: u32,
    /// Maximum number of pages to follow during paginated API
    /// discovery. Default 20.
    pub discovery_max_pages: u32,
    /// Compiled regexes for `comment_ignore_patterns`. Populated by
    /// [`Config::from_raw`]; not part of the YAML schema.
    #[serde(skip)]
    pub compiled_ignore_patterns: Vec<Regex>,
    /// TTL in seconds for scheduler leases. Default 60.
    pub scheduler_lease_ttl_seconds: u64,
    /// TTL in seconds for the per-repo worker lease enforced by
    /// `Pool::admit` across the host. Bounds the worst-case leak
    /// when a worker panics between acquire and the RAII Drop of
    /// the `LeaseGuard`. Default 600. The contract label
    /// `contract/SCHED-001` ("bounded single-host concurrency")
    /// relies on this lease to keep overlapping cron ticks from
    /// exceeding `worker_parallelism`.
    pub worker_lease_ttl_seconds: u64,
    /// Maximum time in milliseconds for a scheduler transaction.
    /// Default 100.
    pub scheduler_transaction_budget_ms: u64,
    /// Timeout in seconds for graceful worker drain on shutdown.
    /// Default 30.
    pub drain_timeout_seconds: u64,
    /// Maximum time in milliseconds to wait for a semaphore permit
    /// before returning PoolSaturated. Default 5000.
    pub backpressure_budget_ms: u64,
    /// Number of consecutive infrastructure failures before the circuit
    /// opens. Default 3.
    pub circuit_failure_threshold: u32,
    /// Exponential backoff stages in seconds for circuit breaker retry.
    /// Default [30, 120, 600].
    pub circuit_backoff_seconds: Vec<u64>,
    /// Seconds after which an open circuit transitions to half-open for
    /// a probe. Default 1800 (30 min).
    pub circuit_open_interval_seconds: u64,
    /// Maximum seconds a circuit can remain open before the work is
    /// escalated to NeedsAttention. Default 86400 (24h).
    pub circuit_max_degraded_seconds: u64,
    /// Root directory for daemon-owned repository storage (bare mirrors
    /// and disposable worktrees). Defaults to `<state_dir>/repos`.
    /// Must not be a symlink and must use mode 0700.
    pub repo_storage_root: PathBuf,
    /// Which executor mode the daemon uses to dispatch workers.
    /// Default [`crate::executor::ExecutorKind::TrustedHost`].
    /// `Oci` parses in config and is rejected at runtime when no OCI
    /// executor is available.
    pub executor_mode: crate::executor::ExecutorKind,
    /// Operator acknowledgement of reduced containment. Required `true`
    /// when `executor_mode == TrustedHost` — the daemon refuses to
    /// dispatch workers on the trusted host without explicit opt-in.
    /// Defaults to `false`; `Config::from_raw` rejects TrustedHost with
    /// `ReducedContainmentNotAcknowledged` before any subprocess spawns.
    pub reduced_containment_acknowledged: bool,

    /// The authoritative OCI sandbox section. `None` for TrustedHost
    /// configs that omit `sandbox:`; required (and always `Some`) when
    /// `executor_mode == oci` — `Config::from_raw` rejects OCI configs
    /// without a valid `sandbox.image`.
    pub sandbox: Option<SandboxConfig>,
}

/// Loose deserialisation layer used to read the YAML before the source
/// path is known. All fields are optional here so the daemon can fill
/// in defaults and resolve the worker command after the load context is
/// available. Conversion to [`Config`] runs every validation.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub poll_interval_seconds: Option<u64>,
    pub state_dir: Option<PathBuf>,
    pub state_backend: Option<String>,
    pub log_path: Option<PathBuf>,
    pub workdir_base: Option<PathBuf>,
    pub watched_repos: Option<Vec<String>>,
    /// Optional in the raw layer so a missing field can be filled with
    /// the user-owned bridge default once the load context is known.
    pub worker_command: Option<Vec<String>>,
    /// Optional operator instruction injected into the worker prompt.
    pub worker_instruction: Option<String>,
    pub worker_timeout_seconds: Option<u64>,
    pub http_timeout_seconds: Option<u64>,
    pub git_timeout_seconds: Option<u64>,
    pub transcript_max_bytes: Option<u64>,
    pub run_retention_days: Option<u64>,
    pub stale_run_hours: Option<u64>,
    pub worktree_gc_older_than_days: Option<u64>,
    pub worktree_gc_disabled: Option<bool>,
    pub archive_on_retry: Option<bool>,
    pub attic_retention_days: Option<u64>,
    pub max_retries_per_issue: Option<u32>,
    pub retry_backoff_seconds: Option<u64>,
    pub ticket_label_code: Option<String>,
    pub ticket_label_investigation: Option<String>,
    pub remove_label_on_completion: Option<bool>,
    pub feedback_author_allowlist: Option<Vec<String>>,
    pub comment_ignore_patterns: Option<Vec<String>>,
    pub comment_forbidden_strings: Option<Vec<String>>,
    pub worker_env_allowlist: Option<Vec<String>>,
    pub github_token: Option<String>,
    pub git_author_name: Option<String>,
    pub git_author_email: Option<String>,
    pub api_base: Option<String>,
    pub dry_run: Option<bool>,
    pub worker_parallelism: Option<u32>,
    pub max_issues_per_tick: Option<u32>,
    pub discovery_max_pages: Option<u32>,
    pub scheduler_lease_ttl_seconds: Option<u64>,
    pub worker_lease_ttl_seconds: Option<u64>,
    pub scheduler_transaction_budget_ms: Option<u64>,
    pub drain_timeout_seconds: Option<u64>,
    pub backpressure_budget_ms: Option<u64>,
    pub circuit_failure_threshold: Option<u32>,
    pub circuit_backoff_seconds: Option<Vec<u64>>,
    pub circuit_open_interval_seconds: Option<u64>,
    pub circuit_max_degraded_seconds: Option<u64>,
    pub repo_storage_root: Option<PathBuf>,
    pub executor_mode: Option<crate::executor::ExecutorKind>,
    pub reduced_containment_acknowledged: Option<bool>,

    /// Optional in the raw layer — absent means `Config.sandbox` is
    /// `None` (valid for TrustedHost) unless `executor_mode == oci`.
    pub sandbox: Option<RawSandboxConfig>,
}

/// Load context — used to resolve paths and the default worker command
/// when the raw layer leaves them blank. The full env-aware loader
/// uses this struct as the seam between parsing and resolution.
#[derive(Clone, Debug, Default)]
pub struct LoadContext {
    pub hermes_home: Option<PathBuf>,
    pub plugin_root: Option<PathBuf>,
    pub env: RawEnv,
}

/// Snapshot of the env variables the config resolver reads. Captured
/// so tests can drive resolution deterministically without mutating
/// the process environment.
#[derive(Clone, Debug, Default)]
pub struct RawEnv {
    pub caduceus_config: Option<String>,
    pub hermes_home: Option<String>,
    pub caduceus_dry_run: Option<String>,
}

impl RawEnv {
    /// Capture the configuration-related environment variables from the
    /// OS process. This is the production entry point; tests use the
    /// struct literal or the `RawEnv::default` constructor.
    pub fn from_process_env() -> Self {
        Self {
            caduceus_config: std::env::var_os("CADUCEUS_CONFIG")
                .map(|v| v.to_string_lossy().to_string()),
            hermes_home: std::env::var_os("HERMES_HOME").map(|v| v.to_string_lossy().to_string()),
            caduceus_dry_run: std::env::var_os("CADUCEUS_DRY_RUN")
                .map(|v| v.to_string_lossy().to_string()),
        }
    }
}

/// What action [`setup_config`] performed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupAction {
    /// A new configuration file was created.
    Created,
    /// An existing file was updated (a `caduceus:` section was added to a
    /// Hermes-shaped file, or an existing standalone was left unchanged).
    Updated,
    /// No action taken (dry-run or config already present).
    Skipped,
}

/// Report from [`setup_config`].
#[derive(Clone, Debug)]
pub struct SetupReport {
    /// Path to the configuration file.
    pub path: PathBuf,
    /// What was done.
    pub action: SetupAction,
    /// The mode the file has (0o600 for new files, existing mode otherwise).
    pub mode: u32,
}

/// Drop guard that removes a temporary file on panic or early return.
struct TmpGuard(PathBuf);

impl Drop for TmpGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Generate minimal non-secret configuration atomically.
///
/// Writes to `$HERMES_HOME/config.yaml`. Refuses when `$CADUCEUS_CONFIG` is
/// set because that env var targets a specific authoritative path.
///
/// Uses a mode-`0600` temporary file in the same directory, atomic
/// [`std::fs::rename`], and a [`TmpGuard`] that cleans the temp file on
/// every error path. When the target already exists, the original file
/// mode and owner are preserved (never widened).
///
/// The generated config OMITS `worker_command` — the load chain resolves
/// the default from the `hermes_home` / `plugin_root`.
pub fn setup_config(hermes_home: &Path, dry_run: bool) -> CaduceusResult<SetupReport> {
    if std::env::var_os("CADUCEUS_CONFIG").is_some() {
        return Err(CaduceusError::Config(
            "refusing to generate config when CADUCEUS_CONFIG is set".to_string(),
        ));
    }

    let config_path = hermes_home.join("config.yaml");

    if dry_run {
        let action = if config_path.is_file() {
            "update"
        } else {
            "create"
        };
        println!(
            "caduceus setup: dry-run, would {action} {}",
            config_path.display(),
        );
        return Ok(SetupReport {
            path: config_path,
            action: SetupAction::Skipped,
            mode: 0o600,
        });
    }

    let state_dir = hermes_home.join("caduceus-state");
    let workdir_base = hermes_home.join("projects");

    let yaml_body = format!(
        r#"# Caduceus configuration — generated by `caduceus setup`
#
# worker_command is resolved at load time from the daemon install
# location (/usr/bin/env python3 <hermes-home>/caduceus/worker-bridge.py).
# Only non-secret fields are stored here; secrets use environment variables
# (CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, gh auth) and the worker env
# allowlist.
---
poll_interval_seconds: 120
state_dir: "{}"
log_path: "{}/processor.log"
workdir_base: "{}"
executor_mode: trusted_host
reduced_containment_acknowledged: true
"#,
        state_dir.display(),
        state_dir.display(),
        workdir_base.display(),
    );

    // Determine existing file state.
    let existing_mode = std::fs::metadata(&config_path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777);
    let preserve_hermes_shape = config_path.is_file();

    // Write temp file in the same directory.
    let tmp_path = config_path.with_file_name("config.yaml.tmp");
    let mut _guard = TmpGuard(tmp_path.clone());

    // Remove any leftover tmp from a previous interrupted run.
    let _ = std::fs::remove_file(&tmp_path);

    // Build final content.
    let final_content: String = if preserve_hermes_shape {
        // Read existing file and merge: if it's a Hermes-shaped file,
        // add/replace the caduceus: section.
        let existing_text = std::fs::read_to_string(&config_path)
            .map_err(|e| CaduceusError::Config(format!("failed to read existing config: {e}")))?;
        let existing_value: serde_yaml::Value = serde_yaml::from_str(&existing_text)
            .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

        let mut merged = match existing_value {
            serde_yaml::Value::Mapping(map) => map,
            _ => serde_yaml::Mapping::new(),
        };

        // Parse the generated YAML body
        let generated_value: serde_yaml::Value = serde_yaml::from_str(&yaml_body)
            .map_err(|e| CaduceusError::Config(format!("failed to parse generated config: {e}")))?;

        if let serde_yaml::Value::Mapping(gen_map) = generated_value {
            merged.insert(
                serde_yaml::Value::String("caduceus".to_string()),
                serde_yaml::Value::Mapping(gen_map),
            );
        }

        let merged_value = serde_yaml::Value::Mapping(merged);
        serde_yaml::to_string(&merged_value)
            .map_err(|e| CaduceusError::Config(format!("failed to serialize merged config: {e}")))?
    } else {
        yaml_body
    };

    // Write atomically.
    use std::io::Write;
    let mut f = std::fs::File::create(&tmp_path)
        .map_err(|e| CaduceusError::Config(format!("failed to create temp config: {e}")))?;
    f.write_all(final_content.as_bytes())
        .map_err(|e| CaduceusError::Config(format!("failed to write temp config: {e}")))?;
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| CaduceusError::Config(format!("failed to set temp config mode: {e}")))?;
    drop(f);

    // Rename.
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| CaduceusError::Config(format!("failed to rename config: {e}")))?;

    // Release the guard since rename succeeded.
    let _ = std::mem::take(&mut _guard.0);

    // Restore original mode (never widen).
    let final_mode = if let Some(orig) = existing_mode {
        let narrowed = std::cmp::min(orig, 0o600);
        if narrowed < orig {
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(narrowed)).ok();
        }
        orig
    } else {
        0o600u32
    };

    let action = if preserve_hermes_shape {
        SetupAction::Updated
    } else {
        SetupAction::Created
    };

    Ok(SetupReport {
        path: config_path,
        action,
        mode: final_mode,
    })
}

impl Config {
    /// Construct a validated [`Config`] from the supplied raw layer.
    ///
    /// Validates every field, compiles regexes, rejects duplicate
    /// labels and credential names in the allowlist, and resolves the
    /// default worker command when the raw layer did not provide one.
    /// The supplied context determines where defaults live.
    pub fn from_raw(raw: RawConfig, ctx: &LoadContext) -> CaduceusResult<Self> {
        let mut errors: Vec<String> = Vec::new();

        let poll_interval_seconds = raw
            .poll_interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS);
        if poll_interval_seconds == 0 {
            errors.push("poll_interval_seconds must be > 0".to_string());
        }

        let state_dir = match raw.state_dir {
            Some(p) => expand_leading_tilde(p),
            None => default_state_dir(ctx),
        };
        validate_secure_path(&state_dir, "state_dir", &mut errors);

        let state_backend = raw
            .state_backend
            .unwrap_or_else(|| DEFAULT_STATE_BACKEND.to_string());
        if state_backend != "json" && state_backend != "sqlite" {
            errors.push(format!(
                "state_backend must be 'json' or 'sqlite', got: {state_backend}"
            ));
        }

        let repo_storage_root = match raw.repo_storage_root {
            Some(p) => expand_leading_tilde(p),
            None => state_dir.join(DEFAULT_REPO_STORAGE_ROOT),
        };
        validate_repo_storage_root(&repo_storage_root, &mut errors);

        let log_path = match raw.log_path {
            Some(p) => expand_leading_tilde(p),
            None => state_dir.join("processor.log"),
        };

        let workdir_base = match raw.workdir_base {
            Some(p) => expand_leading_tilde(p),
            None => default_workdir_base(ctx),
        };

        let watched_repos = raw.watched_repos.unwrap_or_default();
        validate_watched_repos(&watched_repos, &mut errors);

        let worker_command = match raw.worker_command {
            Some(cmd) if !cmd.is_empty() => expand_worker_command(cmd, ctx)?,
            _ => default_worker_command(ctx).ok_or_else(|| {
                CaduceusError::Config(
                    "worker_command is required for standalone installs (no <plugin>/bin/caduceus layout)"
                        .to_string(),
                )
            })?,
        };
        // After resolution, validate worker-command syntax again
        // (expansion might have introduced issues; mostly defensive).
        validate_worker_command(&worker_command, &mut errors);

        let worker_timeout_seconds = raw
            .worker_timeout_seconds
            .unwrap_or(DEFAULT_WORKER_TIMEOUT_SECONDS);
        if worker_timeout_seconds == 0 {
            errors.push("worker_timeout_seconds must be > 0".to_string());
        }
        let http_timeout_seconds = raw
            .http_timeout_seconds
            .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECONDS);
        if http_timeout_seconds == 0 {
            errors.push("http_timeout_seconds must be > 0".to_string());
        }
        let git_timeout_seconds = raw
            .git_timeout_seconds
            .unwrap_or(DEFAULT_GIT_TIMEOUT_SECONDS);
        if git_timeout_seconds == 0 {
            errors.push("git_timeout_seconds must be > 0".to_string());
        }

        let transcript_max_bytes = raw
            .transcript_max_bytes
            .unwrap_or(DEFAULT_TRANSCRIPT_MAX_BYTES);
        if transcript_max_bytes == 0 {
            errors.push("transcript_max_bytes must be > 0".to_string());
        }

        let run_retention_days = raw.run_retention_days.unwrap_or(DEFAULT_RUN_RETENTION_DAYS);
        if run_retention_days == 0 {
            errors.push("run_retention_days must be > 0".to_string());
        }
        let stale_run_hours = raw.stale_run_hours.unwrap_or(DEFAULT_STALE_RUN_HOURS);
        if stale_run_hours == 0 {
            errors.push("stale_run_hours must be > 0".to_string());
        }
        let worktree_gc_older_than_days = raw
            .worktree_gc_older_than_days
            .unwrap_or(DEFAULT_WORKTREE_GC_OLDER_THAN_DAYS);
        if worktree_gc_older_than_days == 0 {
            errors.push("worktree_gc_older_than_days must be > 0".to_string());
        }
        let worktree_gc_disabled = raw
            .worktree_gc_disabled
            .unwrap_or(DEFAULT_WORKTREE_GC_DISABLED);
        let archive_on_retry = raw.archive_on_retry.unwrap_or(DEFAULT_ARCHIVE_ON_RETRY);
        let attic_retention_days = raw
            .attic_retention_days
            .unwrap_or(DEFAULT_ATTIC_RETENTION_DAYS);
        if attic_retention_days == 0 {
            errors.push("attic_retention_days must be > 0".to_string());
        }

        let max_retries_per_issue = raw
            .max_retries_per_issue
            .unwrap_or(DEFAULT_MAX_RETRIES_PER_ISSUE);
        if max_retries_per_issue == 0 {
            errors.push("max_retries_per_issue must be > 0".to_string());
        }
        let retry_backoff_seconds = raw
            .retry_backoff_seconds
            .unwrap_or(DEFAULT_RETRY_BACKOFF_SECONDS);
        if retry_backoff_seconds == 0 {
            errors.push("retry_backoff_seconds must be > 0".to_string());
        }

        let ticket_label_code = raw
            .ticket_label_code
            .unwrap_or_else(|| DEFAULT_TICKET_LABEL_CODE.to_string());
        if ticket_label_code.trim().is_empty() {
            errors.push("ticket_label_code must not be empty".to_string());
        }
        let ticket_label_investigation = raw
            .ticket_label_investigation
            .unwrap_or_else(|| DEFAULT_TICKET_LABEL_INVESTIGATION.to_string());
        if ticket_label_investigation.trim().is_empty() {
            errors.push("ticket_label_investigation must not be empty".to_string());
        }
        if ticket_label_code == ticket_label_investigation {
            errors.push(format!(
            "ticket_label_code and ticket_label_investigation must differ (got {ticket_label_code:?})"
        ));
        }
        let remove_label_on_completion = raw.remove_label_on_completion.unwrap_or(true);

        let feedback_author_allowlist = raw.feedback_author_allowlist.unwrap_or_default();

        let comment_ignore_patterns = raw.comment_ignore_patterns.unwrap_or_default();
        let comment_forbidden_strings = raw.comment_forbidden_strings.unwrap_or_default();
        let worker_env_allowlist = raw.worker_env_allowlist.unwrap_or_default();

        validate_comment_forbidden_strings(&comment_forbidden_strings, &mut errors);
        let compiled_ignore_patterns =
            compile_ignore_patterns(&comment_ignore_patterns, &mut errors)?;
        validate_worker_env_allowlist(&worker_env_allowlist, &mut errors);

        let api_base = raw.api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        if let Err(err) = validate_api_base(&api_base) {
            errors.push(err);
        }

        let discovery_max_pages = raw
            .discovery_max_pages
            .unwrap_or(DEFAULT_DISCOVERY_MAX_PAGES);
        if discovery_max_pages == 0 {
            errors.push("discovery_max_pages must be > 0".to_string());
        }

        // Executor mode and reduced-containment opt-in. TrustedHost
        // requires explicit acknowledgement; Oci is allowed in config
        // and rejected at runtime when no OCI executor is available.
        // The validation runs BEFORE any subprocess is spawned.
        let executor_mode = raw.executor_mode.unwrap_or(DEFAULT_EXECUTOR_MODE);
        let reduced_containment_acknowledged =
            raw.reduced_containment_acknowledged.unwrap_or(false);
        if matches!(executor_mode, crate::executor::ExecutorKind::TrustedHost)
            && !reduced_containment_acknowledged
        {
            errors.push(
                "trusted-host execution requires reduced_containment_acknowledged: true"
                    .to_string(),
            );
        }

        // Sandbox section. Documented invariant:
        //   - executor_mode == Oci  → `sandbox:` section REQUIRED,
        //     `sandbox.image` REQUIRED.
        //   - executor_mode == TrustedHost → `sandbox:` OPTIONAL; absent
        //     ⇒ Config.sandbox is None and nothing downstream reads it.
        //     Present ⇒ validated identically.
        let is_oci_mode = matches!(executor_mode, crate::executor::ExecutorKind::Oci);
        let sandbox = match raw.sandbox {
            None => {
                if is_oci_mode {
                    errors.push(
                        "executor_mode 'oci' requires a `sandbox:` section with a valid \
                         `sandbox.image` (name@sha256:<64 hex>)"
                            .to_string(),
                    );
                }
                None
            }
            Some(raw_sb) => Some(resolve_sandbox(raw_sb, &mut errors)),
        };

        // dry_run is resolved by the env overlay. The raw
        // layer may carry a YAML-supplied hint here for tests; we
        // delegate the merge.
        let dry_run = raw.dry_run.unwrap_or(false);

        // Scheduler dispatch bounds. `worker_parallelism` caps
        // in-flight workers; `max_issues_per_tick` defaults to a
        // bounded multiple so a single cron tick never drains an
        // unbounded queue (issue #108). Explicit `0` opts back into
        // the unbounded drain-the-queue behavior — never the
        // default.
        let worker_parallelism = raw.worker_parallelism.unwrap_or(DEFAULT_WORKER_PARALLELISM);
        let max_issues_per_tick = raw
            .max_issues_per_tick
            .unwrap_or(worker_parallelism.saturating_mul(DEFAULT_MAX_ISSUES_PER_TICK_MULTIPLIER));

        if !errors.is_empty() {
            return Err(CaduceusError::Config(errors.join("; ")));
        }

        Ok(Config {
            poll_interval_seconds,
            state_dir,
            state_backend,
            log_path,
            workdir_base,
            watched_repos,
            worker_command,
            worker_instruction: raw.worker_instruction.unwrap_or_default(),
            worker_timeout_seconds,
            http_timeout_seconds,
            git_timeout_seconds,
            transcript_max_bytes,
            run_retention_days,
            stale_run_hours,
            worktree_gc_older_than_days,
            worktree_gc_disabled,
            archive_on_retry,
            attic_retention_days,
            max_retries_per_issue,
            retry_backoff_seconds,
            ticket_label_code,
            ticket_label_investigation,
            remove_label_on_completion,
            feedback_author_allowlist,
            comment_ignore_patterns,
            comment_forbidden_strings,
            worker_env_allowlist,
            github_token: raw.github_token.and_then(|s| {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }),
            git_author_name: raw.git_author_name.and_then(|s| {
                if s.trim().is_empty() {
                    None
                } else {
                    Some(s.trim().to_string())
                }
            }),
            git_author_email: raw.git_author_email.and_then(|s| {
                if s.trim().is_empty() {
                    None
                } else {
                    Some(s.trim().to_string())
                }
            }),
            api_base,
            dry_run,
            worker_parallelism,
            max_issues_per_tick,
            discovery_max_pages,
            compiled_ignore_patterns,
            scheduler_lease_ttl_seconds: raw
                .scheduler_lease_ttl_seconds
                .unwrap_or(DEFAULT_SCHEDULER_LEASE_TTL_SECONDS),
            worker_lease_ttl_seconds: raw
                .worker_lease_ttl_seconds
                .unwrap_or(DEFAULT_WORKER_LEASE_TTL_SECONDS),
            scheduler_transaction_budget_ms: raw
                .scheduler_transaction_budget_ms
                .unwrap_or(DEFAULT_SCHEDULER_TRANSACTION_BUDGET_MS),
            drain_timeout_seconds: raw
                .drain_timeout_seconds
                .unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECONDS),
            backpressure_budget_ms: raw
                .backpressure_budget_ms
                .unwrap_or(DEFAULT_BACKPRESSURE_BUDGET_MS),

            // Circuit breaker config
            circuit_failure_threshold: {
                let v = raw
                    .circuit_failure_threshold
                    .unwrap_or(DEFAULT_CIRCUIT_FAILURE_THRESHOLD);
                if v == 0 {
                    errors.push("circuit_failure_threshold must be > 0".to_string());
                }
                v
            },
            circuit_backoff_seconds: {
                let v = raw
                    .circuit_backoff_seconds
                    .unwrap_or_else(|| DEFAULT_CIRCUIT_BACKOFF_SECONDS.to_vec());
                if v.is_empty() {
                    errors.push("circuit_backoff_seconds must not be empty".to_string());
                }
                v
            },
            circuit_open_interval_seconds: {
                let v = raw
                    .circuit_open_interval_seconds
                    .unwrap_or(DEFAULT_CIRCUIT_OPEN_INTERVAL_SECONDS);
                if v == 0 {
                    errors.push("circuit_open_interval_seconds must be > 0".to_string());
                }
                v
            },
            circuit_max_degraded_seconds: {
                let v = raw
                    .circuit_max_degraded_seconds
                    .unwrap_or(DEFAULT_CIRCUIT_MAX_DEGRADED_SECONDS);
                if v == 0 {
                    errors.push("circuit_max_degraded_seconds must be > 0".to_string());
                }
                v
            },
            repo_storage_root,
            executor_mode,
            reduced_containment_acknowledged,
            sandbox,
        })
    }

    /// Panics only for hand-built Configs that bypass from_raw in OCI
    /// paths — unreachable for loaded configs (from_raw enforces
    /// presence in OCI mode) and for test_defaults.
    pub fn sandbox(&self) -> &SandboxConfig {
        self.sandbox
            .as_ref()
            .expect("sandbox section is required for the OCI executor")
    }

    /// Deterministic root-anchored defaults for tests. Avoids any
    /// host-dependent `Config::defaults()` constructor that would make
    /// tests flake.
    pub fn test_defaults(root: &Path) -> Self {
        let state_dir = root.join("state");
        let log_path = state_dir.join("processor.log");
        let workdir_base = root.join("workdirs");
        Self {
            poll_interval_seconds: DEFAULT_POLL_INTERVAL_SECONDS,
            state_dir,
            state_backend: "json".to_string(),
            log_path,
            workdir_base,
            watched_repos: Vec::new(),
            worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
            worker_instruction: String::new(),
            worker_timeout_seconds: DEFAULT_WORKER_TIMEOUT_SECONDS,
            http_timeout_seconds: DEFAULT_HTTP_TIMEOUT_SECONDS,
            git_timeout_seconds: DEFAULT_GIT_TIMEOUT_SECONDS,
            transcript_max_bytes: DEFAULT_TRANSCRIPT_MAX_BYTES,
            run_retention_days: DEFAULT_RUN_RETENTION_DAYS,
            stale_run_hours: DEFAULT_STALE_RUN_HOURS,
            worktree_gc_older_than_days: DEFAULT_WORKTREE_GC_OLDER_THAN_DAYS,
            worktree_gc_disabled: DEFAULT_WORKTREE_GC_DISABLED,
            archive_on_retry: DEFAULT_ARCHIVE_ON_RETRY,
            attic_retention_days: DEFAULT_ATTIC_RETENTION_DAYS,
            max_retries_per_issue: DEFAULT_MAX_RETRIES_PER_ISSUE,
            retry_backoff_seconds: DEFAULT_RETRY_BACKOFF_SECONDS,
            ticket_label_code: DEFAULT_TICKET_LABEL_CODE.to_string(),
            ticket_label_investigation: DEFAULT_TICKET_LABEL_INVESTIGATION.to_string(),
            remove_label_on_completion: true,
            feedback_author_allowlist: Vec::new(),
            comment_ignore_patterns: Vec::new(),
            comment_forbidden_strings: Vec::new(),
            worker_env_allowlist: Vec::new(),
            github_token: None,
            git_author_name: None,
            git_author_email: None,
            api_base: DEFAULT_API_BASE.to_string(),
            dry_run: false,
            worker_parallelism: DEFAULT_WORKER_PARALLELISM,
            max_issues_per_tick: DEFAULT_WORKER_PARALLELISM
                .saturating_mul(DEFAULT_MAX_ISSUES_PER_TICK_MULTIPLIER),
            discovery_max_pages: DEFAULT_DISCOVERY_MAX_PAGES,
            compiled_ignore_patterns: Vec::new(),
            scheduler_lease_ttl_seconds: DEFAULT_SCHEDULER_LEASE_TTL_SECONDS,
            worker_lease_ttl_seconds: DEFAULT_WORKER_LEASE_TTL_SECONDS,
            scheduler_transaction_budget_ms: DEFAULT_SCHEDULER_TRANSACTION_BUDGET_MS,
            drain_timeout_seconds: DEFAULT_DRAIN_TIMEOUT_SECONDS,
            backpressure_budget_ms: DEFAULT_BACKPRESSURE_BUDGET_MS,
            circuit_failure_threshold: DEFAULT_CIRCUIT_FAILURE_THRESHOLD,
            circuit_backoff_seconds: DEFAULT_CIRCUIT_BACKOFF_SECONDS.to_vec(),
            circuit_open_interval_seconds: DEFAULT_CIRCUIT_OPEN_INTERVAL_SECONDS,
            circuit_max_degraded_seconds: DEFAULT_CIRCUIT_MAX_DEGRADED_SECONDS,
            repo_storage_root: root.join("repos"),
            executor_mode: DEFAULT_EXECUTOR_MODE,
            // `test_defaults` opts in to trusted-host execution so the
            // existing test suite loads without each test setting the
            // flag explicitly. Tests that exercise the opt-in error
            // path construct a `RawConfig` with the field set to
            // `Some(false)`.
            reduced_containment_acknowledged: true,

            // One valid default sandbox. `test_defaults` bypasses
            // from_raw, so the regex never sees the placeholder image;
            // YAML-loading tests supply real-shaped digests.
            sandbox: Some(SandboxConfig {
                engine: SandboxEngine::Docker,
                // Format-valid placeholder; `test_defaults` bypasses
                // from_raw, so the regex never sees it. YAML-loading
                // tests supply real-shaped digests.
                image: format!("caduceus-worker@sha256:{}", "0".repeat(64)),
                pull_policy: OciPullPolicy::IfMissing,
                resources: SandboxResources {
                    cpus: 2.0,
                    memory_mb: 2048,
                    pids: 256,
                    tmpfs_mb: 256,
                    shm_mb: 64,
                },
                network: SandboxNetwork::None,
                pass_env: Vec::new(),
                stop_timeout_seconds: DEFAULT_SANDBOX_STOP_TIMEOUT_SECONDS,
                kill_timeout_seconds: DEFAULT_SANDBOX_KILL_TIMEOUT_SECONDS,
                reconcile_timeout_seconds: DEFAULT_SANDBOX_RECONCILE_TIMEOUT_SECONDS,
                reserved_host_disk_mb: DEFAULT_SANDBOX_RESERVED_HOST_DISK_MB,
            }),
        }
    }

    /// Resolve configuration through the canonical chain.
    /// Captures the process environment and delegates to
    /// [`Config::load_with_context`] for the actual resolution.
    pub fn load() -> CaduceusResult<Self> {
        let env = RawEnv::from_process_env();
        Self::load_with_context(&env)
    }

    /// Load configuration from the OS environment via the canonical
    /// resolution chain. Accepts a pre-captured [`RawEnv`] so tests can
    /// drive the loader deterministically without mutating process state.
    pub fn load_with_context(env: &RawEnv) -> CaduceusResult<Self> {
        // 1. Resolve $CADUCEUS_CONFIG
        let env_path: Option<PathBuf> = env.caduceus_config.as_deref().map(PathBuf::from);

        // 2. Resolve and validate $HERMES_HOME
        let hermes_path: Option<PathBuf> = match env.hermes_home.as_deref() {
            Some("") => {
                return Err(CaduceusError::Config(
                    "HERMES_HOME must not be empty".to_string(),
                ));
            }
            Some(raw) => {
                let p = PathBuf::from(raw);
                if p.is_relative() {
                    return Err(CaduceusError::Config(
                        "HERMES_HOME must be an absolute path".to_string(),
                    ));
                }
                Some(p)
            }
            None => None,
        };

        // 3. Build standalone path ~/.config/caduceus/config.yaml via shellexpand
        let standalone_path: Option<PathBuf> = {
            let expanded = shellexpand::full("~/.config/caduceus/config.yaml")
                .map_err(|e| CaduceusError::Config(format!("cannot expand config path: {e}")))?;
            let p = PathBuf::from(expanded.as_ref());
            Some(p)
        };

        // 4. Resolve sources with the existing infrastructure
        let sources = resolve_sources(
            env_path.as_deref(),
            hermes_path.as_deref(),
            standalone_path.as_deref(),
        )?;
        let raw = load_raw_from_candidates(&sources)?;

        // 5. Discover plugin root
        let plugin_root = hermes_path.as_deref().and_then(discover_plugin_root);

        // 6. Build Config from Raw with the LoadContext
        let mut config = Config::from_raw(
            raw,
            &LoadContext {
                hermes_home: hermes_path,
                plugin_root,
                env: env.clone(),
            },
        )?;

        // 7. Resolve the GitHub token using the environment chain when
        //    the YAML did not supply one.
        resolve_if_missing(&mut config, &crate::infra::config::token::OsEnv)?;

        // 8. Apply CADUCEUS_DRY_RUN
        if let Some(ref value) = env.caduceus_dry_run {
            apply_dry_run_env(&mut config, value)?;
        }

        Ok(config)
    }

    /// Load configuration from a single, explicit file path.
    ///
    /// The file may be either a standalone Caduceus config (whose
    /// top-level keys map to [`RawConfig`] directly) or a Hermes
    /// configuration document (in which case the ``caduceus:``
    /// section is extracted). The parser detects which shape the
    /// file has by looking for a top-level ``caduceus:`` mapping.
    ///
    /// This entry point is mostly useful for tests and for the
    /// `caduceus migrate-state` flow that needs to read a known file.
    /// The cron tick uses [`Config::load`]. Token resolution is
    /// intentionally NOT invoked here — `load_from` is a test seam
    /// and the `gh auth token` runner must not shell out from CI.
    /// Tests that need to assert strict-error behaviour use the
    /// runner-aware variant [`Config::load_from_with_env`].
    pub fn load_from(path: &Path) -> CaduceusResult<Self> {
        let raw = load_raw_from(path)?;
        Config::from_raw(
            raw,
            &LoadContext {
                hermes_home: None,
                plugin_root: None,
                env: RawEnv::default(),
            },
        )
    }

    /// Test-only entry point. Loads a standalone config from *path*
    /// and gives the test control over the environment and the
    /// ``gh auth token`` runner so token resolution is deterministic.
    pub fn load_from_with_env(
        path: &Path,
        env: &dyn TokenEnv,
        runner: &dyn GhRunner,
    ) -> CaduceusResult<Self> {
        let raw = load_raw_from(path)?;
        let mut config = Config::from_raw(
            raw,
            &LoadContext {
                hermes_home: None,
                plugin_root: None,
                env: RawEnv::default(),
            },
        )?;
        resolve_if_missing_with_runner(&mut config, env, runner)?;
        Ok(config)
    }

    /// Test-only entry point. The three ``Option<Path>`` slots pin the
    /// configuration source at each level of the documented chain
    /// independently so unit tests can drive every precedence case.
    ///
    /// * `env` — value of `$CADUCEUS_CONFIG`, when set.
    /// * `hermes` — value of `$HERMES_HOME` (resolved or relative —
    ///   relative paths are rejected).
    /// * `standalone` — path to the standalone config file (default
    ///   `~/.config/caduceus/config.yaml`); `None` skips that level.
    ///
    /// This entry point reads ``CADUCEUS_DRY_RUN`` from the process env
    /// directly for backwards compatibility with existing tests. New
    /// tests should use [`Config::load_with_context`] for full
    /// deterministic control.
    pub fn load_with_paths(
        env: Option<&Path>,
        hermes: Option<&Path>,
        standalone: Option<&Path>,
    ) -> CaduceusResult<Self> {
        let sources = resolve_sources(env, hermes, standalone)?;
        let raw = load_raw_from_candidates(&sources)?;
        let mut config = Config::from_raw(
            raw,
            &LoadContext {
                hermes_home: hermes.map(|p| p.to_path_buf()),
                plugin_root: None,
                env: RawEnv::default(),
            },
        )?;
        // Token resolution is intentionally NOT invoked here. This is
        // a test seam and the `gh auth token` runner must not shell
        // out from CI runners. Production cron tick uses [`Config::load`]
        // which routes through [`Config::load_with_context`] where the
        // chain runs.
        // ``CADUCEUS_DRY_RUN`` is read from the process env via the
        // same path the daemon uses at runtime. Tests that need to
        // pin the dry-run behaviour set the env var themselves and
        // call ``Config::apply_dry_run`` directly.
        if let Some(value) = std::env::var_os("CADUCEUS_DRY_RUN") {
            apply_dry_run_env(&mut config, &value.to_string_lossy())?;
        }
        Ok(config)
    }

    /// Override ``dry_run`` with a value that was read from the
    /// ``CADUCEUS_DRY_RUN`` environment variable. Returns an error
    /// for any value other than ``1``/``true``/``yes`` (true) or
    /// ``0``/``false``/``no`` (false).
    pub fn apply_dry_run_env(&mut self, value: &str) -> CaduceusResult<()> {
        apply_dry_run_env(self, value)
    }

    /// Resolve the GitHub authentication token for this configuration.
    ///
    /// Hierarchy:
    ///
    /// 1. Explicit `github_token` field, when non-empty.
    /// 2. `$CADUCEUS_GITHUB_TOKEN` environment variable, when non-empty.
    /// 3. `$GITHUB_TOKEN` environment variable, when non-empty.
    /// 4. `gh auth token` subprocess output, when non-empty.
    ///
    /// Empty / whitespace-only values are skipped at every level.
    /// Errors at any level preserve the secret (only the failure
    /// reason and a hint are surfaced).
    pub fn resolve_github_token(&self, env: &dyn TokenEnv) -> CaduceusResult<ResolvedToken> {
        resolve_token_chain(self, env, &RealGhRunner)
    }
}

/// Resolve a raw `sandbox:` section into a validated [`SandboxConfig`].
///
/// Every default is applied here (the raw layer keeps every field
/// `Option`, consistent with the rest of the config). Each rejection
/// pushes an error naming its field into the shared `errors` vec; the
/// caller aggregates them into `CaduceusError::Config`.
fn resolve_sandbox(raw: RawSandboxConfig, errors: &mut Vec<String>) -> SandboxConfig {
    // image — required, no default; validated by the full-reference
    // regex `^[^@\s/]+@sha256:[a-f0-9]{64}$`. Failures are decomposed
    // so the operator can locate and fix the exact problem.
    let image = match raw.image {
        Some(s) if !s.is_empty() => {
            let image_regex = Regex::new(r"^[^@\s/]+@sha256:[a-f0-9]{64}$")
                .expect("sandbox image regex is valid");
            if !image_regex.is_match(&s) {
                match s.split_once('@') {
                    None => {
                        // No digest part at all (tag-only ref, bare name).
                        errors.push(format!(
                            "sandbox.image must be a full reference of the form \
                             name@sha256:<64 hex>, got: {s:?}"
                        ));
                    }
                    Some((_, digest)) if !digest.starts_with("sha256:") => {
                        errors
                            .push("sandbox.image digest must use the sha256 algorithm".to_string());
                    }
                    Some((_, digest)) => {
                        if digest.len() != "sha256:".len() + 64 {
                            errors.push(
                                "sandbox.image digest must be exactly 64 hex chars".to_string(),
                            );
                        } else {
                            // Right shape but still not matching
                            // (empty name, whitespace, slash in name,
                            // non-hex characters...).
                            errors.push(format!(
                                "sandbox.image must be a full reference of the form \
                                 name@sha256:<64 hex>, got: {s:?}"
                            ));
                        }
                    }
                }
            }
            s
        }
        Some(_) => {
            // Explicitly set to empty string.
            errors.push("sandbox.image must not be empty".to_string());
            String::new()
        }
        None => {
            errors.push(
                "sandbox.image is required and must be a full reference of the form \
                 name@sha256:<64 hex>"
                    .to_string(),
            );
            String::new()
        }
    };

    // engine — serde rejects unknown variants at YAML-parse time;
    // the default is applied here.
    let engine = raw.engine.unwrap_or_default();
    let pull_policy = raw.pull_policy.unwrap_or(OciPullPolicy::IfMissing);
    let network = raw.network.unwrap_or_default();
    let resources = resolve_sandbox_resources(raw.resources.unwrap_or_default(), errors);
    let pass_env = raw.pass_env.unwrap_or_default();

    let stop_timeout_seconds = raw
        .stop_timeout_seconds
        .unwrap_or(DEFAULT_SANDBOX_STOP_TIMEOUT_SECONDS);
    if stop_timeout_seconds == 0 {
        errors.push("sandbox.stop_timeout_seconds must be > 0".to_string());
    }
    let kill_timeout_seconds = raw
        .kill_timeout_seconds
        .unwrap_or(DEFAULT_SANDBOX_KILL_TIMEOUT_SECONDS);
    if kill_timeout_seconds == 0 {
        errors.push("sandbox.kill_timeout_seconds must be > 0".to_string());
    }
    let reconcile_timeout_seconds = raw
        .reconcile_timeout_seconds
        .unwrap_or(DEFAULT_SANDBOX_RECONCILE_TIMEOUT_SECONDS);
    if reconcile_timeout_seconds == 0 {
        errors.push("sandbox.reconcile_timeout_seconds must be > 0".to_string());
    }
    // `0` is a valid value and DISABLES the disk-pressure watchdog
    // (no sampling, no enforcement); any positive value is the
    // free-space floor in MB (issue #245).
    let reserved_host_disk_mb = raw
        .reserved_host_disk_mb
        .unwrap_or(DEFAULT_SANDBOX_RESERVED_HOST_DISK_MB);

    SandboxConfig {
        engine,
        image,
        pull_policy,
        resources,
        network,
        pass_env,
        stop_timeout_seconds,
        kill_timeout_seconds,
        reconcile_timeout_seconds,
        reserved_host_disk_mb,
    }
}

/// Resolve the raw `resources:` block. `cpus` rejects non-finite
/// values (YAML `.nan` parses to f64 NaN and would slip a `<`
/// comparison) before the floor check.
fn resolve_sandbox_resources(
    raw: RawSandboxResources,
    errors: &mut Vec<String>,
) -> SandboxResources {
    let cpus = raw.cpus.unwrap_or(2.0);
    if !cpus.is_finite() || cpus < 0.25 {
        errors.push(format!(
            "sandbox.resources.cpus must be >= 0.25, got {cpus}"
        ));
    }
    let memory_mb = raw.memory_mb.unwrap_or(2048);
    if memory_mb < 64 {
        errors.push(format!(
            "sandbox.resources.memory_mb must be >= 64, got {memory_mb}"
        ));
    }
    let pids = raw.pids.unwrap_or(256);
    if pids < 16 {
        errors.push(format!("sandbox.resources.pids must be >= 16, got {pids}"));
    }
    // tmpfs_mb / shm_mb floors: zero would let the engine apply its
    // DEFAULT tmpfs size (`--tmpfs /tmp:size=0m` — Docker treats a
    // zero size as unbounded), which would silently weaken the
    // bounded-tmpfs baseline (issue #245). Negatives are impossible
    // (serde type error at parse time).
    let tmpfs_mb = raw.tmpfs_mb.unwrap_or(256);
    if tmpfs_mb < 1 {
        errors.push(format!(
            "sandbox.resources.tmpfs_mb must be >= 1, got {tmpfs_mb}"
        ));
    }
    let shm_mb = raw.shm_mb.unwrap_or(64);
    if shm_mb < 1 {
        errors.push(format!(
            "sandbox.resources.shm_mb must be >= 1, got {shm_mb}"
        ));
    }
    SandboxResources {
        cpus,
        memory_mb,
        pids,
        tmpfs_mb,
        shm_mb,
    }
}

/// Resolve a token when the YAML did not provide one.
///
/// This is the production helper: it uses the real OS environment
/// and the real ``gh auth token`` runner. The public
/// [`Config::load_from_with_env`] entry point uses the runner-aware
/// variant so tests can inject a stub ``GhRunner``.
pub(crate) fn resolve_if_missing(cfg: &mut Config, env: &dyn TokenEnv) -> CaduceusResult<()> {
    resolve_if_missing_with_runner(cfg, env, &RealGhRunner)
}

fn resolve_if_missing_with_runner(
    cfg: &mut Config,
    env: &dyn TokenEnv,
    runner: &dyn GhRunner,
) -> CaduceusResult<()> {
    if cfg.github_token.is_some() {
        // YAML wins; do not consult the chain.
        return Ok(());
    }
    let resolved = resolve_token_chain(cfg, env, runner)?;
    cfg.github_token = Some(resolved.token);
    tracing::info!(source = ?resolved.source, "GitHub token resolved");
    Ok(())
}

// Submodule declarations and re-exports. The public surface keeps
// `Config`, `RawConfig`, `OciPullPolicy`, etc. at `crate::infra::config::*`.

pub mod load;
pub mod setup;
pub mod token;

use self::load::*;
use self::setup::*;
use self::token::*;

// `load` exposes only `pub(crate)` items, so its names are brought in
// with a private glob; `setup` and `token` also export `pub` items and
// are re-exported to keep `crate::infra::config::*` reachable.
pub use setup::*;
pub use token::*;
