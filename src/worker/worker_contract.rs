//! Worker invocation and result schema.
//!
//! # Normative worker filesystem contract
//!
//! The worker runs against a container filesystem with exactly
//! three writable locations:
//!
//! * `/workspace` — read-write; bind-mounts the host worktree
//!   (`CADUCEUS_WORKTREE_PATH`).
//! * `/output` — read-write; bind-mounts the host run output
//!   directory at `<state_dir>/oci-runs/<run_id>/output`.
//! * `/tmp` — a bounded writable tmpfs; no size guarantee beyond
//!   the container runtime's configured limit.
//!
//! Every other path is read-only or inaccessible; the worker must
//! not depend on writing anywhere else.
//!
//! # Canonical `CADUCEUS_*` environment
//!
//! [`sanitized_env`] exports exactly these daemon-owned variables on
//! every invocation (see also
//! `crate::infra::fixtures::CANONICAL_WORKER_ENV_VARS`, mirrored by
//! the bridge's `REQUIRED_ENV_VARS`). The set depends on the run's
//! [`WorkTarget`] (DAR §6.1): issue runs keep the historical
//! issue-shaped contract byte-for-byte; PR review runs carry a
//! `pr` mode marker plus the four `CADUCEUS_PR_*` variables and
//! **never** an `CADUCEUS_ISSUE_*` or `CADUCEUS_BRANCH_NAME`.
//!
//! Shared by both paths:
//!
//! | Variable | Value |
//! |---|---|
//! | `CADUCEUS_CONTEXT_JSON` | serialised run context |
//! | `CADUCEUS_RUN_ID` | run identifier |
//! | `CADUCEUS_WORKTREE_PATH` | host path of the mounted worktree |
//! | `CADUCEUS_RESULT_PATH` | host result-file path (`<worktree>/worker-result.json`) |
//!
//! Issue path (byte-for-byte v0.1 contract, unchanged):
//!
//! | Variable | Value |
//! |---|---|
//! | `CADUCEUS_BRANCH_NAME` | target branch name |
//! | `CADUCEUS_ISSUE_BODY` | issue body |
//! | `CADUCEUS_ISSUE_LABELS_JSON` | JSON array of issue labels |
//! | `CADUCEUS_ISSUE_NUMBER` | issue number |
//! | `CADUCEUS_ISSUE_REPO` | `owner/repo` |
//! | `CADUCEUS_ISSUE_TITLE` | issue title |
//!
//! PR review path (DAR §6.1 — no synthetic issue key, no branch):
//!
//! | Variable | Value |
//! |---|---|
//! | `CADUCEUS_WORK_TARGET` | `pr` |
//! | `CADUCEUS_PR_NUMBER` | pull request number |
//! | `CADUCEUS_PR_REPO` | `owner/repo` |
//! | `CADUCEUS_PR_BASE_SHA` | base SHA |
//! | `CADUCEUS_PR_HEAD_SHA` | head SHA |
//!
//! The bridge writes its result to `CADUCEUS_RESULT_PATH` (issue-path
//! legacy fallback: `<worktree>/worker-result.json`; the PR path has
//! no fallback — the variable is hard-required). The daemon then
//! [`parse_result_file`]s that file — opening it with
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
//! The deny-by-default worker environment lives here too.
//! [`sanitized_env`] is the single allowlist-and-denylist
//! authority and [`spawn`] is the canonical spawn that calls
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

use crate::executor::WorkTarget;
use crate::github::issue::IssueKey;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::review::ReviewTarget;

/// Container-side workspace mount target: read-write scratch and
/// working directory bound to the host worktree. Normative for every
/// containerized engine; see the module-level filesystem contract.
pub const CONTAINER_WORKSPACE_PATH: &str = "/workspace";

/// Container-side result-delivery mount target: read-write directory
/// bound to the host run output directory. Normative for every
/// containerized engine; see the module-level filesystem contract.
pub const CONTAINER_OUTPUT_PATH: &str = "/output";

/// File name of the worker result document, relative to
/// `CADUCEUS_RESULT_PATH`'s parent on the host and to
/// `CONTAINER_OUTPUT_PATH` inside the container.
pub const WORKER_RESULT_FILE: &str = "worker-result.json";

/// Hard cap on the worker-result file size.
pub const MAX_RESULT_FILE_BYTES: u64 = 1 << 20; // 1 MiB

/// Maximum size of the `summary` field.
pub const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// Maximum character count of `pull_request_title`.
pub const MAX_PULL_REQUEST_TITLE_CHARS: usize = 256;

/// Maximum length of an artifact key.
pub const MAX_ARTIFACT_KEY_LEN: usize = 128;

/// Maximum number of artifact entries.
pub const MAX_ARTIFACTS: usize = 100;

/// Default allowlist entries preserved from the parent environment.
/// Each entry is
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
/// Field semantics and size limits are pinned.
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
///
/// The run's [`WorkTarget`] selects the emitted set: issue runs
/// render the historical issue payload, PR review runs render the
/// `pr` mode marker plus the four `CADUCEUS_PR_*` values (DAR §6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SanitizedEnvInputs {
    /// The work item this run addresses. Issue runs emit
    /// `CADUCEUS_ISSUE_*` + `CADUCEUS_BRANCH_NAME` byte-for-byte;
    /// PR runs emit `CADUCEUS_WORK_TARGET=pr` + `CADUCEUS_PR_*` and
    /// never an issue-shaped variable.
    pub target: WorkTarget,
    /// Worktree path. Must be an absolute UTF-8 path; the
    /// `sanitized_env` validator rejects relative or non-UTF-8
    /// values to keep the bridge's `os.path` calls deterministic.
    pub worktree_path: PathBuf,
    /// Run identifier. Used for `CADUCEUS_RUN_ID` and to
    /// disambiguate concurrent runs in the bridge's logs.
    pub run_id: String,
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
    let result_path_str = format!("{worktree_str}/worker-result.json");
    if inputs.run_id.trim().is_empty() {
        return Err(CaduceusError::Config(
            "run_id must not be empty".to_string(),
        ));
    }
    if inputs.run_id.contains('\0') {
        return Err(CaduceusError::Config("run_id contains NUL".to_string()));
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
    // shell is never trusted — the daemon owns them). The
    // emitted set depends on the run's [`WorkTarget`] (DAR §6.1):
    // issue runs keep the historical 10-name issue contract
    // byte-for-byte; PR review runs emit the `pr` mode marker plus
    // the four `CADUCEUS_PR_*` values and never an issue-shaped
    // variable.
    let shared: &[(&str, &str)] = &[
        ("CADUCEUS_WORKTREE_PATH", &worktree_str),
        ("CADUCEUS_RESULT_PATH", &result_path_str),
        ("CADUCEUS_RUN_ID", &inputs.run_id),
        ("CADUCEUS_CONTEXT_JSON", &inputs.context_json),
    ];
    let canonical: Vec<(String, String)> = match &inputs.target {
        WorkTarget::Issue(issue) => {
            if issue.branch_name.trim().is_empty() {
                return Err(CaduceusError::Config(
                    "branch_name must not be empty".to_string(),
                ));
            }
            if issue.branch_name.contains('\0') {
                return Err(CaduceusError::Config(
                    "branch_name contains NUL".to_string(),
                ));
            }
            if issue.title.contains('\0') || issue.body.contains('\0') {
                return Err(CaduceusError::Config(
                    "issue title/body contains NUL".to_string(),
                ));
            }
            let labels_json = serde_json::to_string(&issue.labels)
                .map_err(|err| CaduceusError::Config(format!("labels JSON serialise: {err}")))?;
            let repo = format!("{}/{}", issue.key.owner, issue.key.repo);
            let issue_canonical: Vec<(String, String)> = vec![
                (
                    "CADUCEUS_ISSUE_NUMBER".to_string(),
                    issue.key.number.to_string(),
                ),
                ("CADUCEUS_ISSUE_TITLE".to_string(), issue.title.clone()),
                ("CADUCEUS_ISSUE_BODY".to_string(), issue.body.clone()),
                ("CADUCEUS_ISSUE_REPO".to_string(), repo),
                ("CADUCEUS_ISSUE_LABELS_JSON".to_string(), labels_json),
                (
                    "CADUCEUS_BRANCH_NAME".to_string(),
                    issue.branch_name.clone(),
                ),
            ];
            shared
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .chain(issue_canonical)
                .collect()
        }
        WorkTarget::PullRequest(pr) => {
            validate_pr_env(pr)?;
            let pr_canonical: Vec<(String, String)> = vec![
                ("CADUCEUS_WORK_TARGET".to_string(), "pr".to_string()),
                (
                    "CADUCEUS_PR_NUMBER".to_string(),
                    pr.pull_request.to_string(),
                ),
                ("CADUCEUS_PR_REPO".to_string(), pr.repository.full_name()),
                ("CADUCEUS_PR_BASE_SHA".to_string(), pr.base_sha.clone()),
                ("CADUCEUS_PR_HEAD_SHA".to_string(), pr.head_sha.clone()),
            ];
            shared
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .chain(pr_canonical)
                .collect()
        }
    };
    for (k, v) in canonical {
        out.insert(OsString::from(k), OsString::from(v));
    }

    Ok(out)
}

/// Validate the PR-path env inputs. Mirrors the issue-arm checks:
/// every `CADUCEUS_PR_*` string must be non-empty and NUL-free and
/// the pull request number must be positive. `base_ref`/`merge_base`
/// are context (DAR §2.1) and do not enter the environment.
fn validate_pr_env(pr: &ReviewTarget) -> CaduceusResult<()> {
    for (field, value) in [
        ("pr.repository.owner", pr.repository.owner.as_str()),
        ("pr.repository.repo", pr.repository.repo.as_str()),
        ("pr.base_sha", pr.base_sha.as_str()),
        ("pr.head_sha", pr.head_sha.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(CaduceusError::Config(format!("{field} must not be empty")));
        }
        if value.contains('\0') {
            return Err(CaduceusError::Config(format!("{field} contains NUL")));
        }
    }
    if pr.pull_request == 0 {
        return Err(CaduceusError::Config(
            "pr.pull_request must be greater than 0".to_string(),
        ));
    }
    Ok(())
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

// Deny / allow helpers

/// Return true when *name* (an `OsStr`) is a credential or
/// daemon-internal secret the worker must never see.
///
/// `pub(crate)` so the OCI path (`sandbox.pass_env` config validation
/// in `infra::config::setup` and defensive re-checking in
/// `executor::sandbox_spec::resolve_with_env`) reuses this single
/// deny authority — the table is shared, NOT copied (spec R4,
/// design D1). Semantics unchanged.
pub(crate) fn denied_name(name: &OsStr) -> bool {
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
    let mut result: WorkerResult =
        serde_json::from_slice(&bytes).map_err(|err| CaduceusError::Worker {
            context: "parse",
            stderr: format!("{}: {err}", path.display()),
        })?;
    result.pull_request_title = truncate_pull_request_title(&result.pull_request_title);
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
    validate_commit_message(&result.commit_message)?;
    validate_pull_request_title(&result.pull_request_title)?;
    validate_artifacts(&result.artifacts)?;
    Ok(())
}

fn validate_required_no_length(field: &str, value: &str) -> CaduceusResult<()> {
    if value.contains('\0') {
        return Err(CaduceusError::Config(format!("{field} contains NUL")));
    }
    if value.trim().is_empty() {
        return Err(CaduceusError::Config(format!("{field} is empty")));
    }
    Ok(())
}

fn validate_required_string(field: &str, value: &str, max: usize) -> CaduceusResult<()> {
    validate_required_no_length(field, value)?;
    if value.len() > max {
        return Err(CaduceusError::Config(format!(
            "{field} exceeds limit of {max} bytes (got {})",
            value.len()
        )));
    }
    Ok(())
}

fn validate_commit_message(value: &str) -> CaduceusResult<()> {
    validate_required_no_length("commit_message", value)?;
    if contains_control_other_than_newline(value) {
        return Err(CaduceusError::Config(
            "commit_message contains control characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_pull_request_title(value: &str) -> CaduceusResult<()> {
    validate_required_no_length("pull_request_title", value)?;
    if value.contains('\n') {
        return Err(CaduceusError::Config(
            "pull_request_title must be a single line".to_string(),
        ));
    }
    if contains_control(value) {
        return Err(CaduceusError::Config(
            "pull_request_title contains control characters".to_string(),
        ));
    }
    if value.chars().count() > MAX_PULL_REQUEST_TITLE_CHARS {
        return Err(CaduceusError::Config(format!(
            "pull_request_title exceeds limit of {MAX_PULL_REQUEST_TITLE_CHARS} characters (got {})",
            value.chars().count()
        )));
    }
    Ok(())
}

pub fn truncate_pull_request_title(title: &str) -> String {
    if title.chars().count() <= MAX_PULL_REQUEST_TITLE_CHARS {
        return title.to_string();
    }
    title
        .chars()
        .take(MAX_PULL_REQUEST_TITLE_CHARS - 1)
        .collect::<String>()
        + "…"
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
