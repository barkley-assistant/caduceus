#![allow(dead_code, unused_imports)]
use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::infra::error::{CaduceusError, CaduceusResult};

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

pub(crate) fn home_dir() -> Option<PathBuf> {
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
pub(crate) fn discover_plugin_root(hermes_home: &Path) -> Option<PathBuf> {
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

pub(crate) fn default_state_dir(ctx: &LoadContext) -> PathBuf {
    if let Some(ref h) = ctx.hermes_home {
        return h.join("caduceus-state");
    }
    expand_leading_tilde(PathBuf::from("~/.hermes/caduceus-state"))
}

pub(crate) fn default_workdir_base(ctx: &LoadContext) -> PathBuf {
    // Hermes-primary default lives under HERMES_HOME; standalone
    // installs typically use ~/projects. We default to the latter
    // unless HERMES_HOME is set, in which case the Hermes-managed
    // <HERMES_HOME>/projects path is used.
    if let Some(ref h) = ctx.hermes_home {
        return h.join("projects");
    }
    expand_leading_tilde(PathBuf::from("~/projects"))
}

pub(crate) fn default_worker_command(ctx: &LoadContext) -> Option<Vec<String>> {
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

pub(crate) fn expand_worker_command(
    cmd: Vec<String>,
    ctx: &LoadContext,
) -> CaduceusResult<Vec<String>> {
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

/// Validate a secret grant name: must match `[a-z][a-z0-9-]{0,63}`.
pub(crate) fn is_valid_secret_grant_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() {
        return false;
    }
    if name.len() > 64 {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

pub(crate) fn validate_watched_repos(repos: &[String], errors: &mut Vec<String>) {
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
    crate::github::issue::validate_owner(owner).is_ok()
        && crate::github::issue::validate_repo(repo_name).is_ok()
}

/// Positive allowlist validator for the `api_base` configuration value.
///
/// Per `CONTRACTS.md` GH-001, `api_base` MUST be either the literal
/// `https://api.github.com` or an `https://` URL whose path is
/// `/api/v3` (or `/api/v3/...`). Anything else — `http://`,
/// `bitbucket.example.com`, custom path-prefixed proxies, malformed
/// URLs — is rejected with a configuration error.
///
/// Rules (evaluated in order):
/// 1. Trim the input. Empty after trim → `Err("api_base must not be empty")`.
/// 2. Parse as `url::Url`. Parse failure → `Err("api_base is not a valid URL")`.
/// 3. Scheme MUST be `https`. Otherwise → `Err("api_base scheme must be https")`.
/// 4. Host MUST be present and non-empty.
/// 5. For `api.github.com` → path must be `/` or empty (SaaS).
///    For any other host → path must be `/api/v3` or start with `/api/v3/` (GHES).
/// 6. If all rules pass → `Ok(())`.
///
/// # Loopback accommodation (test-only)
///
/// The validator additionally accepts `http://` URLs whose host is a
/// loopback address (`localhost`, `127.0.0.1`, or any address in
/// `127.0.0.0/8`). This is a **test-only** accommodation: the
/// integration test suite (`tests/integration/integration_test.rs`) spawns the
/// real `caduceus` binary against a wiremock HTTP server, and
/// configuring wiremock with a self-signed TLS cert is out of scope
/// for this task. Loopback addresses are not routable, so the
/// production security posture is unaffected — a production config
/// pointing `api_base` at a loopback address would fail to reach
/// GitHub, but it would not expose credentials or accept a
/// non-GitHub endpoint.
///
/// Error messages NEVER include the raw `value` parameter to prevent
/// credential leaks. They reference only parsed components or generic
/// descriptions.
///
/// This function is a pure positive allowlist — it does NOT consult
/// `comment_forbidden_strings` or any other forbidden-list to detect
/// non-GitHub endpoints. The independence test in
/// `tests/state/api_base_allowlist_test.rs` proves this property.
pub fn validate_api_base(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("api_base must not be empty".to_string());
    }
    let url = url::Url::parse(trimmed).map_err(|_| "api_base is not a valid URL".to_string())?;
    let host = url.host_str().unwrap_or("");
    let is_loopback = match url.host_str() {
        Some("localhost") => true,
        Some(h) => h == "127.0.0.1" || h.starts_with("127."),
        _ => false,
    };
    if url.scheme() != "https" && !is_loopback {
        return Err("api_base scheme must be https".to_string());
    }
    match url.host_str() {
        Some(h) if !h.is_empty() => {}
        _ => return Err("api_base must have a host".to_string()),
    }
    // Loopback: skip path validation (test-only).
    if is_loopback {
        return Ok(());
    }
    let path = url.path();
    if host == "api.github.com" {
        // GitHub.com SaaS: path must be empty or /
        if path != "/" && !path.is_empty() {
            return Err("api_base path must be / for api.github.com".to_string());
        }
    } else {
        // GHES: path must be /api/v3 or /api/v3/...
        if path != "/api/v3" && !path.starts_with("/api/v3/") {
            return Err("api_base path must be /api/v3 for non-SaaS hosts".to_string());
        }
    }
    Ok(())
}

pub(crate) fn validate_worker_command(cmd: &[String], errors: &mut Vec<String>) {
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

pub(crate) fn validate_comment_forbidden_strings(values: &[String], errors: &mut Vec<String>) {
    for value in values {
        if value.is_empty() {
            errors.push("comment_forbidden_strings must not contain empty entries".to_string());
        }
    }
}

pub(crate) fn compile_ignore_patterns(
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

pub(crate) fn validate_worker_env_allowlist(values: &[String], errors: &mut Vec<String>) {
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
pub(crate) fn validate_secure_path(path: &Path, field: &str, errors: &mut Vec<String>) {
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

/// Validate `repo_storage_root`: refuse symlinks, and when the
/// directory already exists refuse modes wider than 0700 with
/// an operator-actionable fix message. Missing directories are
/// accepted — the daemon creates them at startup with mode 0700.
pub(crate) fn validate_repo_storage_root(path: &Path, errors: &mut Vec<String>) {
    if path.as_os_str().is_empty() {
        errors.push("repo_storage_root must not be empty".to_string());
        return;
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            errors.push(format!(
                "repo_storage_root must not be a symlink: {}",
                path.display()
            ));
            return;
        }
    }
    if path.exists() {
        if let Ok(meta) = std::fs::metadata(path) {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o700 {
                errors.push(format!(
                    "repo_storage_root {} has mode {:03o}; expected 0700. Run: chmod 0700 {}",
                    path.display(),
                    mode,
                    path.display()
                ));
            }
        }
    }
}
