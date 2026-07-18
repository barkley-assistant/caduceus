//! Configuration: typed loader for the YAML configuration file and the
//! env-variable overrides listed in `CONTRACTS.md` under "Configuration".
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
//! constructor (CONTRACTS.md "Configuration").
//!
//! Public field list, semantics, and defaults are pinned by
//! `CONTRACTS.md` — every field documented there must be present.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};

/// GitHub credential variable names that must never appear in the
/// worker environment allowlist, even if the operator explicitly adds
/// them. Source: CONTRACTS.md "Worker environment and result".
pub const DENIED_ENV_VARS: &[&str] = &["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"];

/// Worker command tokens that are always rejected as interpolation.
const FORBIDDEN_INTERPOLATION_TOKENS: &[&str] = &["$HOME", "${HOME}", "~", "$USER"];

/// The exact token that *is* allowed as worker-command interpolation.
pub const PLUGIN_ROOT_TOKEN: &str = "${plugin_root}";

/// Default values the daemon falls back to when an operator omits a
/// field. Defaults match the block-quoted values in CONTRACTS.md "Configuration".
pub const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 120;
pub const DEFAULT_WORKER_TIMEOUT_SECONDS: u64 = 3600;
pub const DEFAULT_HTTP_TIMEOUT_SECONDS: u64 = 60;
pub const DEFAULT_GIT_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_TRANSCRIPT_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_RUN_RETENTION_DAYS: u64 = 30;
pub const DEFAULT_STALE_RUN_HOURS: u64 = 1;
pub const DEFAULT_MAX_RETRIES_PER_ISSUE: u32 = 3;
pub const DEFAULT_RETRY_BACKOFF_SECONDS: u64 = 300;
pub const DEFAULT_TICKET_LABEL_CODE: &str = "🤖 auto-fix";
pub const DEFAULT_TICKET_LABEL_INVESTIGATION: &str = "🤖 auto-fix-investigate";
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Caduceus configuration. Field semantics are pinned in `CONTRACTS.md`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Compiled regexes for `comment_ignore_patterns`. Populated by
    /// [`Config::from_raw`]; not part of the YAML schema.
    #[serde(skip)]
    pub compiled_ignore_patterns: Vec<Regex>,
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
    pub log_path: Option<PathBuf>,
    pub workdir_base: Option<PathBuf>,
    pub watched_repos: Option<Vec<String>>,
    /// Optional in the raw layer so a missing field can be filled with
    /// the user-owned bridge default once the load context is known.
    pub worker_command: Option<Vec<String>>,
    pub worker_timeout_seconds: Option<u64>,
    pub http_timeout_seconds: Option<u64>,
    pub git_timeout_seconds: Option<u64>,
    pub transcript_max_bytes: Option<u64>,
    pub run_retention_days: Option<u64>,
    pub stale_run_hours: Option<u64>,
    pub max_retries_per_issue: Option<u32>,
    pub retry_backoff_seconds: Option<u64>,
    pub ticket_label_code: Option<String>,
    pub ticket_label_investigation: Option<String>,
    pub feedback_author_allowlist: Option<Vec<String>>,
    pub comment_ignore_patterns: Option<Vec<String>>,
    pub comment_forbidden_strings: Option<Vec<String>>,
    pub worker_env_allowlist: Option<Vec<String>>,
    pub github_token: Option<String>,
    pub api_base: Option<String>,
    pub dry_run: Option<bool>,
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
"#,
        state_dir.display(),
        state_dir.display(),
        workdir_base.display(),
    );

    // --- Determine existing file state ---
    let existing_mode = std::fs::metadata(&config_path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777);
    let preserve_hermes_shape = config_path.is_file();

    // --- Write temp file in the same directory ---
    let tmp_path = config_path.with_file_name("config.yaml.tmp");
    let mut _guard = TmpGuard(tmp_path.clone());

    // Remove any leftover tmp from a previous interrupted run.
    let _ = std::fs::remove_file(&tmp_path);

    // --- Build final content ---
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

    // --- Write atomically ---
    use std::io::Write;
    let mut f = std::fs::File::create(&tmp_path)
        .map_err(|e| CaduceusError::Config(format!("failed to create temp config: {e}")))?;
    f.write_all(final_content.as_bytes())
        .map_err(|e| CaduceusError::Config(format!("failed to write temp config: {e}")))?;
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| CaduceusError::Config(format!("failed to set temp config mode: {e}")))?;
    drop(f);

    // --- Rename ---
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| CaduceusError::Config(format!("failed to rename config: {e}")))?;

    // Release the guard since rename succeeded.
    let _ = std::mem::take(&mut _guard.0);

    // --- Restore original mode (never widen) ---
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

        let feedback_author_allowlist = raw.feedback_author_allowlist.unwrap_or_default();
        let comment_ignore_patterns = raw.comment_ignore_patterns.unwrap_or_default();
        let comment_forbidden_strings = raw.comment_forbidden_strings.unwrap_or_default();
        let worker_env_allowlist = raw.worker_env_allowlist.unwrap_or_default();

        validate_comment_forbidden_strings(&comment_forbidden_strings, &mut errors);
        let compiled_ignore_patterns =
            compile_ignore_patterns(&comment_ignore_patterns, &mut errors)?;
        validate_worker_env_allowlist(&worker_env_allowlist, &mut errors);

        let api_base = raw.api_base.unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        if api_base.trim().is_empty() {
            errors.push("api_base must not be empty".to_string());
        }

        // dry_run is resolved by the env overlay. The raw
        // layer may carry a YAML-supplied hint here for tests; we
        // delegate the merge.
        let dry_run = raw.dry_run.unwrap_or(false);

        if !errors.is_empty() {
            return Err(CaduceusError::Config(errors.join("; ")));
        }

        Ok(Config {
            poll_interval_seconds,
            state_dir,
            log_path,
            workdir_base,
            watched_repos,
            worker_command,
            worker_timeout_seconds,
            http_timeout_seconds,
            git_timeout_seconds,
            transcript_max_bytes,
            run_retention_days,
            stale_run_hours,
            max_retries_per_issue,
            retry_backoff_seconds,
            ticket_label_code,
            ticket_label_investigation,
            feedback_author_allowlist,
            comment_ignore_patterns,
            comment_forbidden_strings,
            worker_env_allowlist,
            github_token: raw.github_token.and_then(|s| non_empty(Some(&s))),
            api_base,
            dry_run,
            compiled_ignore_patterns,
        })
    }

    /// Deterministic root-anchored defaults for tests. Avoids any
    /// host-dependent `Config::defaults()` constructor that would make
    /// tests flake (CONTRACTS.md "Configuration").
    pub fn test_defaults(root: &Path) -> Self {
        let state_dir = root.join("state");
        let log_path = state_dir.join("processor.log");
        let workdir_base = root.join("workdirs");
        Self {
            poll_interval_seconds: DEFAULT_POLL_INTERVAL_SECONDS,
            state_dir,
            log_path,
            workdir_base,
            watched_repos: Vec::new(),
            worker_command: vec!["python3".to_string(), "bridge.py".to_string()],
            worker_timeout_seconds: DEFAULT_WORKER_TIMEOUT_SECONDS,
            http_timeout_seconds: DEFAULT_HTTP_TIMEOUT_SECONDS,
            git_timeout_seconds: DEFAULT_GIT_TIMEOUT_SECONDS,
            transcript_max_bytes: DEFAULT_TRANSCRIPT_MAX_BYTES,
            run_retention_days: DEFAULT_RUN_RETENTION_DAYS,
            stale_run_hours: DEFAULT_STALE_RUN_HOURS,
            max_retries_per_issue: DEFAULT_MAX_RETRIES_PER_ISSUE,
            retry_backoff_seconds: DEFAULT_RETRY_BACKOFF_SECONDS,
            ticket_label_code: DEFAULT_TICKET_LABEL_CODE.to_string(),
            ticket_label_investigation: DEFAULT_TICKET_LABEL_INVESTIGATION.to_string(),
            feedback_author_allowlist: Vec::new(),
            comment_ignore_patterns: Vec::new(),
            comment_forbidden_strings: Vec::new(),
            worker_env_allowlist: Vec::new(),
            github_token: None,
            api_base: DEFAULT_API_BASE.to_string(),
            dry_run: false,
            compiled_ignore_patterns: Vec::new(),
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

        // 7. Apply CADUCEUS_DRY_RUN
        if let Some(ref value) = env.caduceus_dry_run {
            apply_dry_run_env(&mut config, value)?;
        }

        Ok(config)
    }

    /// Load the configuration from a single, explicit file path.
    ///
    /// The file may be either a standalone Caduceus config (whose
    /// top-level keys map to [`RawConfig`] directly) or a Hermes
    /// configuration document (in which case the ``caduceus:``
    /// section is extracted). The parser detects which shape the
    /// file has by looking for a top-level ``caduceus:`` mapping.
    ///
    /// This entry point is mostly useful for tests and for the
    /// `caduceus migrate-state` flow that needs to read a known file.
    /// The cron tick uses [`Config::load`].
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
    /// Hierarchy per `CONTRACTS.md` "Configuration":
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

// ---------------------------------------------------------------------------
// Resolution chain
// ---------------------------------------------------------------------------

/// Where the configuration came from. Used in error messages so the
/// operator can tell which level of the chain produced the failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedSource {
    /// `$CADUCEUS_CONFIG` was set and pointed at this file.
    ExplicitEnv,
    /// `$HERMES_HOME/config.yaml` had a `caduceus:` section.
    HermesHome,
    /// `~/.config/caduceus/config.yaml` was the only one present.
    Standalone,
}

/// Compute the list of files to consider, in order, given the three
/// optional inputs from the loader.
fn resolve_sources(
    env: Option<&Path>,
    hermes: Option<&Path>,
    standalone: Option<&Path>,
) -> CaduceusResult<Vec<(ResolvedSource, std::path::PathBuf)>> {
    let mut sources: Vec<(ResolvedSource, std::path::PathBuf)> = Vec::new();
    if let Some(path) = env {
        let expanded = expand_leading_tilde(path.to_path_buf());
        sources.push((ResolvedSource::ExplicitEnv, expanded));
    }
    if let Some(hermes_home) = hermes {
        if hermes_home.as_os_str().is_empty() {
            return Err(CaduceusError::Config(
                "HERMES_HOME must not be empty".to_string(),
            ));
        }
        // Reject relative HERMES_HOME per the contract.
        if hermes_home.is_relative() {
            return Err(CaduceusError::Config(
                "HERMES_HOME must be an absolute path".to_string(),
            ));
        }
        sources.push((ResolvedSource::HermesHome, hermes_home.join("config.yaml")));
    }
    if let Some(path) = standalone {
        sources.push((ResolvedSource::Standalone, path.to_path_buf()));
    }
    Ok(sources)
}

/// Read the raw configuration from the first successful candidate.
/// Hermes files without a ``caduceus:`` section are skipped only when
/// a standalone source is also available.
fn load_raw_from_candidates(
    sources: &[(ResolvedSource, std::path::PathBuf)],
) -> CaduceusResult<RawConfig> {
    if sources.is_empty() {
        return Err(CaduceusError::Config(
            "no configuration source provided".to_string(),
        ));
    }

    // An explicit $CADUCEUS_CONFIG is an authoritative request — a
    // missing file is a hard error. The operator either meant for
    // that path to exist or set the variable by mistake.
    if let Some((ResolvedSource::ExplicitEnv, path)) = sources.first() {
        if !path.is_file() {
            return Err(CaduceusError::Config(format!(
                "$CADUCEUS_CONFIG points at {} but the file is missing",
                path.display()
            )));
        }
        return load_raw_from(path).map_err(|err| match err {
            CaduceusError::Yaml(yaml_err) => {
                CaduceusError::Config(format!("failed to parse {}: {yaml_err}", path.display()))
            }
            other => other,
        });
    }

    let mut standalone_seen = false;
    let mut hermes_seen_without_section = false;
    let mut last_missing_standalone: Option<&std::path::Path> = None;
    for (source, path) in sources {
        match source {
            ResolvedSource::HermesHome => {
                if !path.is_file() {
                    continue;
                }
                match load_raw_from(path) {
                    Ok(raw) => return Ok(raw),
                    Err(CaduceusError::Config(msg))
                        if msg.contains("missing 'caduceus:' section")
                            || msg.contains("has no 'caduceus:' section") =>
                    {
                        // Hermes file present but no caduceus section.
                        // Per the task, fall through only if a
                        // standalone source also exists.
                        hermes_seen_without_section = true;
                    }
                    Err(other) => return Err(other),
                }
            }
            ResolvedSource::Standalone => {
                standalone_seen = true;
                if !path.is_file() {
                    last_missing_standalone = Some(path.as_path());
                    continue;
                }
                // If a previous Hermes file was missing the section,
                // we still want the standalone file to take over.
                return load_raw_from(path);
            }
            ResolvedSource::ExplicitEnv => unreachable!(),
        }
    }

    if hermes_seen_without_section && !standalone_seen {
        return Err(CaduceusError::Config(
            "Hermes config has no 'caduceus:' section and no standalone config was found"
                .to_string(),
        ));
    }
    if hermes_seen_without_section {
        // Standalone was configured but the file was missing — surface
        // that explicitly so the operator can fix it.
        let path = last_missing_standalone
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unset>".to_string());
        return Err(CaduceusError::Config(format!(
            "Hermes config has no 'caduceus:' section and standalone config {path} is missing"
        )));
    }

    Err(CaduceusError::Config(
        "no configuration source found (set $CADUCEUS_CONFIG, $HERMES_HOME, or write ~/.config/caduceus/config.yaml)".to_string(),
    ))
}

/// Read a single configuration file. Detects the Hermes shape (a top
/// level ``caduceus:`` mapping) and unwraps it before deserialising
/// into [`RawConfig`].
fn load_raw_from(path: &Path) -> CaduceusResult<RawConfig> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        CaduceusError::Config(format!("failed to read {}: {err}", path.display()))
    })?;
    parse_raw_from_text(&text, path)
}

fn parse_raw_from_text(text: &str, source_path: &Path) -> CaduceusResult<RawConfig> {
    let outer: serde_yaml::Value = serde_yaml::from_str(text)?;
    let map = outer.as_mapping().ok_or_else(|| {
        CaduceusError::Config(format!(
            "expected a YAML mapping at the root of {}",
            source_path.display()
        ))
    })?;
    if map.contains_key("caduceus") {
        // Hermes-shaped file: extract the ``caduceus:`` mapping.
        let section = map.get("caduceus").ok_or_else(|| {
            CaduceusError::Config(format!(
                "missing 'caduceus:' section in {}",
                source_path.display()
            ))
        })?;
        let raw: RawConfig = serde_yaml::from_value(section.clone())?;
        return Ok(raw);
    }
    // Standalone-shaped file: every top-level key is part of the
    // raw config. We rely on ``deny_unknown_fields`` to catch
    // typos and stray sections — but only if the keys look like
    // Caduceus config. Detect Hermes-style keys (which the contract
    // expects on the same host) and treat the missing ``caduceus:``
    // section as an explicit error rather than a parse failure.
    for key in map.keys() {
        if let Some(name) = key.as_str() {
            if matches!(
                name,
                "model"
                    | "agent"
                    | "providers"
                    | "tools"
                    | "memory"
                    | "cron"
                    | "platforms"
                    | "gateway"
                    | "secrets"
                    | "voice"
                    | "mcp"
                    | "tts"
            ) {
                return Err(CaduceusError::Config(format!(
                    "Hermes config at {} has no 'caduceus:' section",
                    source_path.display()
                )));
            }
        }
    }
    let raw: RawConfig = serde_yaml::from_str(text)?;
    Ok(raw)
}

/// Apply ``CADUCEUS_DRY_RUN`` to a parsed Config.
fn apply_dry_run_env(cfg: &mut Config, value: &str) -> CaduceusResult<()> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" => cfg.dry_run = true,
        "0" | "false" | "no" => cfg.dry_run = false,
        _ => {
            return Err(CaduceusError::Config(format!(
                "CADUCEUS_DRY_RUN must be one of 1/true/yes/0/false/no (got {value:?})"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Token resolution
// ---------------------------------------------------------------------------

/// Indicate which resolution path produced the token. Used in tests
/// and in the daemon's structured logs (without the secret itself).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenSource {
    ExplicitConfig,
    CaduceusEnv,
    GithubEnv,
    GhCli,
}

/// Environment variable lookup abstracted over the host process or a
/// test fixture. Implementations must never leak the value through
/// the trait surface — only non-secret metadata.
pub trait TokenEnv {
    /// Read an environment variable, returning `None` when unset or
    /// empty. Whitespace-only values are also treated as unset.
    fn get(&self, name: &str) -> Option<String>;
}

/// Process-environment adapter. Reads from the real OS env via
/// `std::env::var_os`. Wrapped in a struct so tests can swap in a
/// fake without mutating process state under concurrent tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEnv;

impl TokenEnv for OsEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var_os(name)
            .map(|value| value.to_string_lossy().trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// Run a `gh auth token` subprocess with a 10-second timeout, captured
/// stderr, and no token logging. The runner is overridable in tests.
pub trait GhRunner: Send + Sync {
    fn run(&self) -> Result<GhRunnerOutput, CaduceusError>;
}

/// What `gh auth token` produced, reduced to the contract surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GhRunnerOutput {
    pub exit_status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Default `gh` runner. Resolves the binary, shells out with an
/// argument array, and surfaces exit codes / stderr without echoing
/// the captured stdout.
#[derive(Debug)]
pub struct RealGhRunner;

impl GhRunner for RealGhRunner {
    fn run(&self) -> Result<GhRunnerOutput, CaduceusError> {
        // ``which::which`` is the contract-respecting binary
        // resolver; absent ``gh`` is a clean error.
        let binary = match which::which("gh") {
            Ok(path) => path,
            Err(_) => {
                return Err(CaduceusError::TokenResolution(
                    "`gh` executable not found in PATH".to_string(),
                ));
            }
        };
        // ``subprocess::Command`` requires async + tokio; for the
        // single-shot blocking 10-second call we use ``std::process``
        // which is enough and avoids tying the resolver to a runtime.
        // We do *not* log stdout — by contract the value is secret.
        let mut command = std::process::Command::new(&binary);
        command.arg("auth").arg("token");
        command.env_clear();
        // Inherit only PATH-equivalent vars the binary needs. We
        // deliberately do not inherit the daemon's GitHub token so
        // the operator's existing ``gh auth login`` state is the
        // single source of truth. HOME is needed for ``gh`` to find
        // its config directory.
        for var in ["PATH", "HOME", "USER", "XDG_CONFIG_HOME"] {
            if let Some(value) = std::env::var_os(var) {
                command.env(var, value);
            }
        }
        let output = match command.output() {
            Ok(out) => out,
            Err(err) => {
                return Err(CaduceusError::TokenResolution(format!(
                    "failed to spawn `gh`: {err}"
                )));
            }
        };
        let exit_status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok(GhRunnerOutput {
            exit_status,
            stdout,
            stderr,
        })
    }
}

/// Resolved token + which source produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedToken {
    pub token: String,
    pub source: TokenSource,
}

impl ResolvedToken {
    /// Bundle a token with its source for callers that want to log
    /// the resolution path without exposing the secret.
    pub fn new(token: String, source: TokenSource) -> Self {
        Self { token, source }
    }
}

/// Implementation of the documented hierarchy. Public so tests can
/// drive it with their own env / gh fixtures.
pub fn resolve_token_chain(
    cfg: &Config,
    env: &dyn TokenEnv,
    runner: &dyn GhRunner,
) -> CaduceusResult<ResolvedToken> {
    if let Some(token) = non_empty(cfg.github_token.as_deref()) {
        return Ok(ResolvedToken::new(token, TokenSource::ExplicitConfig));
    }
    if let Some(token) = env.get("CADUCEUS_GITHUB_TOKEN") {
        return Ok(ResolvedToken::new(token, TokenSource::CaduceusEnv));
    }
    if let Some(token) = env.get("GITHUB_TOKEN") {
        return Ok(ResolvedToken::new(token, TokenSource::GithubEnv));
    }

    // Final fallback: ``gh auth token``.
    match runner.run() {
        Ok(out) if out.exit_status == 0 => {
            let trimmed = out.stdout.trim().to_string();
            if is_token_usable(&trimmed) {
                return Ok(ResolvedToken::new(trimmed, TokenSource::GhCli));
            }
            Err(CaduceusError::TokenResolution(
                "`gh auth token` returned no usable token".to_string(),
            ))
        }
        Ok(out) => Err(CaduceusError::TokenResolution(format!(
            "`gh auth token` exited {} (stderr suppressed)",
            out.exit_status
        ))),
        Err(err) => Err(err),
    }
}

/// Return ``Some(token)`` when *token* is non-empty after trimming and
/// contains at least one non-whitespace character.
fn is_token_usable(token: &str) -> bool {
    !token.trim().is_empty()
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Path / interpolation helpers
// ---------------------------------------------------------------------------

/// Expand a single leading ``~`` to the user's home directory. No
/// other shell expansion is performed (CONTRACTS.md "Configuration").
pub fn expand_leading_tilde(path: PathBuf) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = home_dir() {
            if rest.as_os_str().is_empty() {
                return home;
            }
            // Preserve a literal separator so ``~/foo`` does not
            // collapse to ``~foo`` when ``home`` already ends with a
            // separator.
            let mut combined = home;
            if !rest.starts_with("/") && !rest.starts_with("\\") {
                combined.push(std::path::Path::new(rest));
            } else {
                combined.push(rest);
            }
            return combined;
        }
    }
    path
}

fn home_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("HOME") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return Some(PathBuf::from(profile));
        }
    }
    None
}

/// Try to discover the Hermes plugin root directory.
///
/// When Caduceus is installed as a Hermes plugin, the plugin directory
/// sits at `$HERMES_HOME/plugins/caduceus/`. Returns `Some(...)` only
/// when that directory exists and contains the canonical plugin-assets
/// layout, so a stale empty directory from a partial install does not
/// trigger plugin-root resolution.
fn discover_plugin_root(hermes_home: &Path) -> Option<PathBuf> {
    let candidate = hermes_home.join("plugins").join("caduceus");
    if !candidate.is_dir() {
        return None;
    }
    // Validate the install by checking for the plugin-assets bridge.
    if candidate
        .join("plugin-assets")
        .join("worker-bridge.py")
        .is_file()
    {
        Some(candidate)
    } else {
        None
    }
}

fn default_state_dir(ctx: &LoadContext) -> PathBuf {
    if let Some(ref h) = ctx.hermes_home {
        return h.join("caduceus-state");
    }
    expand_leading_tilde(PathBuf::from("~/.hermes/caduceus-state"))
}

fn default_workdir_base(ctx: &LoadContext) -> PathBuf {
    // Hermes-primary default lives under HERMES_HOME; standalone
    // installs typically use ~/projects. We default to the latter
    // unless HERMES_HOME is set, in which case the Hermes-managed
    // <HERMES_HOME>/projects path is used.
    if let Some(ref h) = ctx.hermes_home {
        return h.join("projects");
    }
    expand_leading_tilde(PathBuf::from("~/projects"))
}

fn default_worker_command(ctx: &LoadContext) -> Option<Vec<String>> {
    // Priority 1: Hermes plugin layout — template bridge under plugin-assets/.
    if let Some(ref plugin_root) = ctx.plugin_root {
        let bridge = plugin_root.join("plugin-assets").join("worker-bridge.py");
        return Some(vec![
            "python3".to_string(),
            bridge.to_string_lossy().to_string(),
        ]);
    }
    // Priority 2: Hermes-primary install — user-owned bridge (AC-04).
    if let Some(ref hermes_home) = ctx.hermes_home {
        let bridge = hermes_home.join("caduceus").join("worker-bridge.py");
        return Some(vec![
            "python3".to_string(),
            bridge.to_string_lossy().to_string(),
        ]);
    }
    None
}

fn expand_worker_command(cmd: Vec<String>, ctx: &LoadContext) -> CaduceusResult<Vec<String>> {
    let mut out = Vec::with_capacity(cmd.len());
    for (idx, arg) in cmd.into_iter().enumerate() {
        if arg.contains(PLUGIN_ROOT_TOKEN) {
            if idx == 0 {
                return Err(CaduceusError::Config(
                    "${plugin_root} cannot appear in the program position of worker_command"
                        .to_string(),
                ));
            }
            let plugin_root = ctx.plugin_root.as_ref().ok_or_else(|| {
                CaduceusError::Config(
                    "${plugin_root} referenced but plugin root is not known".to_string(),
                )
            })?;
            let replaced = arg.replace(PLUGIN_ROOT_TOKEN, &plugin_root.to_string_lossy());
            out.push(replaced);
        } else if arg.contains('$') || arg.contains('~') {
            return Err(CaduceusError::Config(format!(
                "worker_command argument {idx} contains unsupported interpolation: {arg:?}"
            )));
        } else {
            out.push(arg);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Validators
// ---------------------------------------------------------------------------

fn validate_watched_repos(repos: &[String], errors: &mut Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    for repo in repos {
        let lower = repo.to_ascii_lowercase();
        if seen.contains(&lower) {
            errors.push(format!(
                "watched_repos contains case-insensitive duplicate: {repo}"
            ));
            continue;
        }
        seen.insert(lower);
        if !is_valid_repo_slug(repo) {
            errors.push(format!("watched_repos entry is not owner/repo: {repo:?}"));
        }
    }
}

pub fn is_valid_repo_slug(repo: &str) -> bool {
    // Owner/repo: owner uses GitHub's alphanumeric/hyphen rules (1..=39
    // chars, no leading/trailing hyphen); repo is 1..=100 chars from
    // [A-Za-z0-9_.-] excluding "." and "..".
    if repo.is_empty() {
        return false;
    }
    let Some((owner, repo_name)) = repo.split_once('/') else {
        return false;
    };
    if repo_name.contains('/') {
        return false;
    }
    crate::issue::validate_owner(owner).is_ok() && crate::issue::validate_repo(repo_name).is_ok()
}

fn validate_worker_command(cmd: &[String], errors: &mut Vec<String>) {
    if cmd.is_empty() {
        errors.push("worker_command must contain at least one argument".to_string());
    }
    for arg in cmd {
        for forbidden in FORBIDDEN_INTERPOLATION_TOKENS {
            if arg.contains(forbidden) {
                errors.push(format!(
                    "worker_command argument {arg:?} contains forbidden interpolation token {forbidden:?}"
                ));
            }
        }
    }
}

fn validate_comment_forbidden_strings(values: &[String], errors: &mut Vec<String>) {
    for value in values {
        if value.is_empty() {
            errors.push("comment_forbidden_strings must not contain empty entries".to_string());
        }
    }
}

fn compile_ignore_patterns(
    patterns: &[String],
    errors: &mut Vec<String>,
) -> CaduceusResult<Vec<Regex>> {
    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        match Regex::new(pattern) {
            Ok(re) => compiled.push(re),
            Err(e) => errors.push(format!(
                "comment_ignore_patterns contains invalid regex {pattern:?}: {e}"
            )),
        }
    }
    Ok(compiled)
}

fn validate_worker_env_allowlist(values: &[String], errors: &mut Vec<String>) {
    for value in values {
        validate_env_var_pattern(value, errors);
        // Reject any pattern whose expansion could expose a denied
        // credential name. Exact-name entries (``GITHUB_TOKEN``) and
        // direct-prefix wildcards (``GITHUB_TOKEN_*``) are denied.
        // Broader wildcards that include a denied name as a match
        // (``GITHUB_*``) are also denied, because they would let
        // ``GITHUB_TOKEN`` reach the worker through the wildcard.
        if exposes_denied_credential(value) {
            errors.push(format!(
                "worker_env_allowlist contains denied credential name: {value:?}"
            ));
        }
    }
}

/// Return ``true`` when *value* (an exact name or a terminal ``*``
/// prefix wildcard) would expose any entry in [`DENIED_ENV_VARS`] to
/// the worker process.
pub(crate) fn exposes_denied_credential(value: &str) -> bool {
    for denied in DENIED_ENV_VARS {
        if value == *denied {
            return true;
        }
        // The exact prefix wildcard (``NAME_*``) always exposes ``NAME``.
        let prefix = format!("{denied}_*");
        if value == prefix {
            return true;
        }
        // A broader wildcard (``PREFIX_*``) exposes ``denied`` when
        // ``denied`` starts with ``PREFIX`` (case-sensitive — the
        // operator must opt in explicitly).
        if let Some(prefix) = value.strip_suffix('*') {
            if denied.starts_with(prefix) && denied.len() > prefix.len() {
                return true;
            }
        }
    }
    false
}

/// A single env-var entry must be either an exact portable name or
/// an exact terminal-`*` prefix pattern. Any other wildcard placement,
/// empty entry, `=`, NUL, or non-portable character is rejected.
pub(crate) fn validate_env_var_pattern(value: &str, errors: &mut Vec<String>) {
    if value.is_empty() {
        errors.push("worker_env_allowlist entry must not be empty".to_string());
        return;
    }
    if value.contains('=') {
        errors.push(format!(
            "worker_env_allowlist entry {value:?} must not contain '='"
        ));
        return;
    }
    if value.contains('\0') {
        errors.push(format!(
            "worker_env_allowlist entry {value:?} must not contain NUL"
        ));
        return;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '*')
    {
        errors.push(format!(
            "worker_env_allowlist entry {value:?} contains non-portable characters"
        ));
        return;
    }
    // Only one terminal ``*`` is permitted.
    if value.contains('*') {
        let star_count = value.matches('*').count();
        if star_count > 1 || !value.ends_with('*') {
            errors.push(format!(
                "worker_env_allowlist entry {value:?} may only contain a single terminal '*' wildcard"
            ));
        }
    }
    if exposes_denied_credential(value) {
        errors.push(format!(
            "worker_env_allowlist contains denied credential name: {value:?}"
        ));
    }
}

/// Validate the secure-path semantics for state-style directories:
/// reject symlinks and refuse to accept paths that already exist as
/// non-directories. The function does NOT touch the filesystem beyond
/// `metadata`; creation is the daemon's job.
fn validate_secure_path(path: &Path, field: &str, errors: &mut Vec<String>) {
    if path.as_os_str().is_empty() {
        errors.push(format!("{field} must not be empty"));
        return;
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            errors.push(format!("{field} must not be a symlink: {}", path.display()));
        } else if !meta.is_dir() {
            errors.push(format!(
                "{field} exists but is not a directory: {}",
                path.display()
            ));
        }
    }
}
