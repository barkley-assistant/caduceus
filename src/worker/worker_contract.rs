//! Worker invocation and result schema.
//!
//! The bridge writes `<worktree>/worker-result.json` on exit 0. The
//! daemon then [`parse_result_file`]s that file — opening it with
//! `O_NOFOLLOW`, verifying the descriptor is a regular file, and
//! reading with a 1 MiB cap before allocating the full document.
//!
//! Every string field is validated:
//!
//! * Trimmed, non-empty, NUL-free.
//! * `summary` ≤ 64 KiB.
//! * `commit_message` and `pull_request_title` ≤ 256 characters.
//! * `pull_request_title` is one line with no control characters.
//! * `commit_message` may contain newlines but no other control
//!   characters.
//!
//! Artifact keys are non-empty, control-free, at most 128 characters,
//! and the map is limited to 100 entries. The map is a
//! `BTreeMap<String, serde_json::Value>` so iteration is stable.
//!
//! Investigation tickets use the same schema: `commit_message` and
//! `pull_request_title` must still be present (schema stability),
//! but the finalization path ignores them. Code tickets require
//! meaningful repository changes later in finalize.
//!
//! The deny-by-default worker environment lives here too. Task 5.2
//! pins [`sanitized_env`] as the single allowlist-and-denylist
//! authority and [`spawn`] as the canonical spawn that calls
//! [`std::process::Command::env_clear`] before injecting the
//! sanitized env. The supervisor (`worker_supervisor`) sits on top
//! of this surface.
//!
//! All file- and schema-level failures are wrapped as a contextual
//! `CaduceusError::Worker` so the structured logger and the
//! queue retry logic can branch on the operation label.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Hard cap on the worker-result file size per `CONTRACTS.md`
/// "Worker environment and result".
pub const MAX_RESULT_FILE_BYTES: u64 = 1 << 20; // 1 MiB

/// Maximum size of the `summary` field.
pub const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// Maximum size of `commit_message` and `pull_request_title`.
pub const MAX_TITLE_BYTES: usize = 256;

/// Maximum length of an artifact key.
pub const MAX_ARTIFACT_KEY_LEN: usize = 128;

/// Maximum number of artifact entries.
pub const MAX_ARTIFACTS: usize = 100;

/// Default allowlist entries preserved from the parent environment
/// (CONTRACTS.md "Worker environment and result"). Each entry is
/// an exact portable variable name; the daemon never expands
/// partial matches here — the matching allowlist below carries
/// the documented prefix patterns.
pub const DEFAULT_ALLOWLIST_EXACT: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "LANG", "LC_ALL", "TERM", "TMPDIR",
];

/// Default allowlist prefix patterns preserved from the parent
/// environment. The single terminal `*` matches anything in the
/// suffix, so `OPENAI_API_KEY`, `OPENAI_ORG`, and
/// `OPENAI_PROJECT_ID` all reach the worker.
pub const DEFAULT_ALLOWLIST_PREFIXES: &[&str] =
    &["OPENAI_*", "ANTHROPIC_*", "OPENROUTER_*", "OPENCODE_*"];

/// Hard-deny list: exact variable names that never reach the
/// worker even when an operator adds them to the allowlist.
/// Source: CONTRACTS.md "Worker environment and result".
const DENIED_EXACT_VARS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "CADUCEUS_GITHUB_TOKEN",
    "AUTO_ISSUE_GITHUB_TOKEN",
];

/// Daemon-internal secrets are any `CADUCEUS_*` variable that
/// carries a credential or signing marker. The contract requires
/// the daemon's resolved GitHub token and any signing material to
/// never reach the worker; the pattern below mirrors that rule.
const INTERNAL_SECRET_MARKERS: &[&str] = &["SECRET", "TOKEN"];

/// Result the bridge writes to `<worktree>/worker-result.json`.
///
/// Field semantics and size limits are pinned in `CONTRACTS.md`
/// under "Worker environment and result".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub status: WorkerStatus,
    pub summary: String,
    pub commit_message: String,
    pub pull_request_title: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub investigation: bool,
}

/// Status the bridge can return.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Success,
    Failure,
}

/// Inputs for [`sanitized_env`]. The struct carries every value
/// the worker must see as a `CADUCEUS_*` variable plus the
/// operator-configured `worker_env_allowlist`. The parent
/// environment is supplied as a separate argument to keep the
/// function pure and easy to test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SanitizedEnvInputs {
    /// GitHub issue the worker is processing. The display key
    /// (`owner/repo#number`) is rendered to `CADUCEUS_ISSUE_REPO`
    /// and the number alone to `CADUCEUS_ISSUE_NUMBER`.
    pub issue: IssueKey,
    /// Issue title (one line, NUL-free, no embedded newlines
    /// per the GitHub API contract; the daemon copies it as-is
    /// because the upstream title is the authoritative value).
    pub issue_title: String,
    /// Issue body. NUL-free, may contain newlines.
    pub issue_body: String,
    /// Label names. Emitted to `CADUCEUS_ISSUE_LABELS_JSON` as a
    /// JSON array so embedded commas and quotes survive the env
    /// boundary intact.
    pub labels: Vec<String>,
    /// Worktree path. Must be an absolute UTF-8 path; the
    /// `sanitized_env` validator rejects relative or non-UTF-8
    /// values to keep the bridge's `os.path` calls deterministic.
    pub worktree_path: PathBuf,
    /// Run identifier. Used for `CADUCEUS_RUN_ID` and to
    /// disambiguate concurrent runs in the bridge's logs.
    pub run_id: String,
    /// Daemon-owned expected branch name. Emitted as
    /// `CADUCEUS_BRANCH_NAME` so the bridge never has to
    /// select its own ref.
    pub branch_name: String,
    /// Operator-configured `worker_env_allowlist`. Each entry is
    /// either an exact variable name or a single terminal-`*`
    /// prefix pattern (the syntax validated in `Config`).
    /// Credentials in this list are still hard-denied.
    pub allowlist: Vec<String>,
    /// The stable context JSON document. Emitted verbatim to
    /// `CADUCEUS_CONTEXT_JSON`.
    pub context_json: String,
}

/// Build the deny-by-default environment the worker bridge
/// inherits. The function is pure: it reads *parent* (the
/// daemon's inherited environment, captured for testability)
/// and *inputs*, and returns the exact `BTreeMap` the
/// production spawner hands to `Command::envs` after a
/// prior `env_clear()`.
///
/// The deny list is the union of:
/// * the four exact credential names (`GITHUB_TOKEN`,
///   `GH_TOKEN`, `CADUCEUS_GITHUB_TOKEN`, `AUTO_ISSUE_GITHUB_TOKEN`);
/// * any variable whose name contains both `GITHUB` and `TOKEN`
///   as substrings (catches `MY_GITHUB_TOKEN`, `GITHUB_API_TOKEN`,
///   `GITHUB_FINEGRAINED_TOKEN`, …);
/// * any `CADUCEUS_*` variable whose name contains a daemon
///   internal-secret marker (`SECRET`, `TOKEN`) — this is the
///   "daemon-internal secret" clause of the contract, mirroring
///   the resolved GitHub token and any future signing key.
///
/// The allowlist is, in order:
/// 1. The eight documented exact names
///    ([`DEFAULT_ALLOWLIST_EXACT`]);
/// 2. The four documented provider prefix patterns
///    ([`DEFAULT_ALLOWLIST_PREFIXES`]);
/// 3. The operator's `worker_env_allowlist` entries (each
///    either an exact name or a single terminal-`*` prefix
///    pattern; credentials are still denied).
///
/// All `CADUCEUS_*` variables set in *inputs* are layered on
/// top, so a worker-visible variable never inherits from the
/// parent. `CADUCEUS_ISSUE_LABELS_JSON` is the JSON
/// serialisation of `inputs.labels`.
pub fn sanitized_env(
    parent: &BTreeMap<OsString, OsString>,
    inputs: &SanitizedEnvInputs,
) -> CaduceusResult<BTreeMap<OsString, OsString>> {
    let mut out: BTreeMap<OsString, OsString> = BTreeMap::new();

    // Step 1: validate the inputs that the worker's env can
    // surface. A bad path or empty run id is a configuration
    // error, not a runtime error.
    let worktree_str = require_absolute_utf8_path(&inputs.worktree_path, "worktree_path")?;
    if inputs.run_id.trim().is_empty() {
        return Err(CaduceusError::Config(
            "run_id must not be empty".to_string(),
        ));
    }
    if inputs.run_id.contains('\0') {
        return Err(CaduceusError::Config("run_id contains NUL".to_string()));
    }
    if inputs.branch_name.trim().is_empty() {
        return Err(CaduceusError::Config(
            "branch_name must not be empty".to_string(),
        ));
    }
    if inputs.branch_name.contains('\0') {
        return Err(CaduceusError::Config(
            "branch_name contains NUL".to_string(),
        ));
    }
    if inputs.issue_title.contains('\0') || inputs.issue_body.contains('\0') {
        return Err(CaduceusError::Config(
            "issue title/body contains NUL".to_string(),
        ));
    }
    if inputs.context_json.contains('\0') {
        return Err(CaduceusError::Config(
            "context_json contains NUL".to_string(),
        ));
    }

    // Step 2: copy every parent entry that survives the
    // allowlist + denylist filters. Order of checks: deny
    // first (so a credential on the allowlist is still
    // dropped), then allow.
    for (k, v) in parent.iter() {
        if denied_name(k) {
            continue;
        }
        if allowed_default(k) || allowed_explicit(k, &inputs.allowlist) {
            out.insert(k.clone(), v.clone());
        }
    }

    // Step 3: layer the canonical `CADUCEUS_*` variables on
    // top. These override any parent entry with the same name
    // (a `CADUCEUS_*` value the operator may have set in the
    // shell is never trusted — the daemon owns them).
    let labels_json = serde_json::to_string(&inputs.labels)
        .map_err(|err| CaduceusError::Config(format!("labels JSON serialise: {err}")))?;
    let repo = format!("{}/{}", inputs.issue.owner, inputs.issue.repo);
    let canonical: &[(&str, &str)] = &[
        ("CADUCEUS_ISSUE_NUMBER", &inputs.issue.number.to_string()),
        ("CADUCEUS_ISSUE_TITLE", &inputs.issue_title),
        ("CADUCEUS_ISSUE_BODY", &inputs.issue_body),
        ("CADUCEUS_ISSUE_REPO", &repo),
        ("CADUCEUS_ISSUE_LABELS_JSON", &labels_json),
        ("CADUCEUS_WORKTREE_PATH", &worktree_str),
        ("CADUCEUS_RUN_ID", &inputs.run_id),
        ("CADUCEUS_BRANCH_NAME", &inputs.branch_name),
        ("CADUCEUS_CONTEXT_JSON", &inputs.context_json),
    ];
    for (k, v) in canonical {
        out.insert(OsString::from(*k), OsString::from(*v));
    }

    Ok(out)
}

/// Spawn *command* with the sanitized environment. The function
/// is the single producer of the production spawn path: it
/// always calls `Command::env_clear()` before `envs()`, so a
/// credential injected via the inherited env cannot reach the
/// child even if the operator's allowlist is overly broad.
///
/// The caller is responsible for the rest of the supervision
/// contract (process group, timeout, parent-death cleanup) —
/// this function returns the `Command` ready for the supervisor
/// to exec.
pub fn spawn(
    command: &[String],
    cwd: &Path,
    inputs: &SanitizedEnvInputs,
) -> CaduceusResult<Command> {
    if command.is_empty() {
        return Err(CaduceusError::Worker {
            context: "spawn",
            stderr: "worker command is empty".to_string(),
        });
    }
    let mut cmd = Command::new(&command[0]);
    cmd.current_dir(cwd);
    for arg in &command[1..] {
        cmd.arg(arg);
    }
    cmd.env_clear();
    let parent: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    let env = sanitized_env(&parent, inputs)?;
    cmd.envs(env);
    Ok(cmd)
}

// ---------------------------------------------------------------------------
// Deny / allow helpers
// ---------------------------------------------------------------------------

/// Return true when *name* (an `OsStr`) is a credential or
/// daemon-internal secret the worker must never see.
fn denied_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    // Exact-name denials.
    for denied in DENIED_EXACT_VARS {
        if bytes == denied.as_bytes() {
            return true;
        }
    }
    // Pattern: variable name contains BOTH "GITHUB" and
    // "TOKEN" as case-sensitive substrings. Catches
    // MY_GITHUB_TOKEN, GITHUB_API_TOKEN, …
    let contains_github = contains_subslice(bytes, b"GITHUB");
    let contains_token = contains_subslice(bytes, b"TOKEN");
    if contains_github && contains_token {
        return true;
    }
    // Daemon-internal: any `CADUCEUS_*` whose name contains a
    // SECRET or TOKEN marker. The GitHub token, signing key,
    // and any future bearer material all sit behind this rule.
    if bytes.starts_with(b"CADUCEUS_") {
        for marker in INTERNAL_SECRET_MARKERS {
            if contains_subslice(bytes, marker.as_bytes()) {
                return true;
            }
        }
    }
    false
}

/// Return true when *name* is one of the default exact allowlist
/// entries.
fn allowed_default(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    for allowed in DEFAULT_ALLOWLIST_EXACT {
        if bytes == allowed.as_bytes() {
            return true;
        }
    }
    for prefix in DEFAULT_ALLOWLIST_PREFIXES {
        // The contract pins the syntax as a single terminal `*`.
        let prefix_bytes = prefix.as_bytes();
        let star = match prefix_bytes.iter().rposition(|b| *b == b'*') {
            Some(i) => i,
            None => continue,
        };
        // The star must be the last byte.
        if star + 1 != prefix_bytes.len() {
            continue;
        }
        let body = &prefix_bytes[..star];
        if bytes.len() >= body.len() && &bytes[..body.len()] == body {
            return true;
        }
    }
    false
}

/// Return true when *name* matches one of the operator's
/// explicit allowlist entries. Syntax is either an exact
/// portable name or a single terminal-`*` prefix pattern. The
/// caller is expected to have validated the pattern at config
/// time; this helper is conservative and only honours a
/// well-formed pattern.
fn allowed_explicit(name: &OsStr, allowlist: &[String]) -> bool {
    let bytes = name.as_bytes();
    for entry in allowlist {
        if entry.is_empty() || entry.contains('=') || entry.contains('\0') {
            continue;
        }
        if entry.ends_with('*') {
            // A single terminal `*`. The contract forbids
            // multiple `*` or non-terminal placement at config
            // time; we re-check defensively.
            if entry.matches('*').count() != 1 {
                continue;
            }
            let body = &entry[..entry.len() - 1];
            if body.is_empty() {
                continue;
            }
            if bytes.len() >= body.len() && &bytes[..body.len()] == body.as_bytes() {
                return true;
            }
        } else if entry.as_bytes() == bytes {
            return true;
        }
    }
    false
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn require_absolute_utf8_path(path: &Path, field: &str) -> CaduceusResult<String> {
    if !path.is_absolute() {
        return Err(CaduceusError::Config(format!(
            "{field} must be an absolute path (got {})",
            path.display()
        )));
    }
    match path.to_str() {
        Some(s) => Ok(s.to_string()),
        None => Err(CaduceusError::Config(format!(
            "{field} must be valid UTF-8 (got {})",
            path.display()
        ))),
    }
}

/// Parse + validate a `worker-result.json` file at *path* against
/// the canonical schema. The function performs the read-side
/// invariants the contract requires: `O_NOFOLLOW` open, regular
/// file check, 1 MiB read cap, then JSON parse + validation.
pub fn parse_result_file(path: &Path, issue: &IssueKey) -> CaduceusResult<WorkerResult> {
    let bytes =
        read_capped_file(path, MAX_RESULT_FILE_BYTES).map_err(|err| CaduceusError::Worker {
            context: "read",
            stderr: format!("{}: {err}", path.display()),
        })?;
    let result: WorkerResult =
        serde_json::from_slice(&bytes).map_err(|err| CaduceusError::Worker {
            context: "parse",
            stderr: format!("{}: {err}", path.display()),
        })?;
    validate_worker_result(&result, issue).map_err(|err| CaduceusError::Worker {
        context: "validate",
        stderr: format!("{}: {err}", path.display()),
    })?;
    Ok(result)
}

/// Pure validator: takes an already-parsed [`WorkerResult`] and
/// confirms the document satisfies every field-level rule. Exposed
/// separately so tests can drive the validator without a file.
pub fn validate_worker_result(result: &WorkerResult, _issue: &IssueKey) -> CaduceusResult<()> {
    validate_required_string("summary", &result.summary, MAX_SUMMARY_BYTES)?;
    validate_required_string("commit_message", &result.commit_message, MAX_TITLE_BYTES)?;
    validate_required_string(
        "pull_request_title",
        &result.pull_request_title,
        MAX_TITLE_BYTES,
    )?;
    if contains_control_other_than_newline(&result.commit_message) {
        return Err(CaduceusError::Config(
            "commit_message contains control characters".to_string(),
        ));
    }
    if contains_control(&result.pull_request_title) {
        return Err(CaduceusError::Config(
            "pull_request_title contains control characters".to_string(),
        ));
    }
    if result.pull_request_title.contains('\n') {
        return Err(CaduceusError::Config(
            "pull_request_title must be a single line".to_string(),
        ));
    }
    validate_artifacts(&result.artifacts)?;
    Ok(())
}

fn validate_required_string(field: &str, value: &str, max: usize) -> CaduceusResult<()> {
    if value.contains('\0') {
        return Err(CaduceusError::Config(format!("{field} contains NUL")));
    }
    if value.trim().is_empty() {
        return Err(CaduceusError::Config(format!("{field} is empty")));
    }
    if value.len() > max {
        return Err(CaduceusError::Config(format!(
            "{field} exceeds limit of {max} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

fn contains_control(value: &str) -> bool {
    value.chars().any(|c| c.is_control())
}

fn contains_control_other_than_newline(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r')
}

fn validate_artifacts(artifacts: &BTreeMap<String, serde_json::Value>) -> CaduceusResult<()> {
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(CaduceusError::Config(format!(
            "artifacts exceeds limit of {MAX_ARTIFACTS} entries (got {})",
            artifacts.len()
        )));
    }
    for key in artifacts.keys() {
        if key.is_empty() {
            return Err(CaduceusError::Config("artifact key is empty".to_string()));
        }
        if key.len() > MAX_ARTIFACT_KEY_LEN {
            return Err(CaduceusError::Config(format!(
                "artifact key exceeds limit of {MAX_ARTIFACT_KEY_LEN} chars (got {})",
                key.len()
            )));
        }
        if contains_control(key) {
            return Err(CaduceusError::Config(
                "artifact key contains control characters".to_string(),
            ));
        }
    }
    Ok(())
}

/// Open *path* with `O_NOFOLLOW`, verify the resolved descriptor is
/// a regular file, then read at most *cap* bytes. Returns a clean
/// `CaduceusError::Config` for the read-side failures.
fn read_capped_file(path: &Path, cap: u64) -> CaduceusResult<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| CaduceusError::Config(format!("open {}: {err}", path.display())))?;
    let meta = file
        .metadata()
        .map_err(|err| CaduceusError::Config(format!("stat {}: {err}", path.display())))?;
    if !meta.is_file() {
        return Err(CaduceusError::Config(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > cap {
        return Err(CaduceusError::Config(format!(
            "{} exceeds cap of {cap} bytes (got {})",
            path.display(),
            meta.len()
        )));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    let mut handle = file.take(cap);
    handle
        .read_to_end(&mut buf)
        .map_err(|err| CaduceusError::Config(format!("read {}: {err}", path.display())))?;
    Ok(buf)
}
