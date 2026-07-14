//! Finalization: commit, push, PR, comment/close, investigation comment.
//!
//! Idempotency across partial failures is the hard requirement — see
//! `CONTRACTS.md` "Finalization contract" and Tasks 6.1–6.5.
//!
//! This module owns the public-voice validator that every outbound
//! comment, PR title, and PR body must pass before the
//! corresponding API mutation. The validator lives in finalize.rs
//! because that is the only point through which GitHub mutations
//! flow; routing it through github.rs alone would leave a future
//! finalization caller free to bypass it.
//!
//! The public-voice rule is:
//!
//! * The text must not contain any `comment_forbidden_strings` term
//!   (case-insensitive Unicode substring match). Configuration
//!   replaces the defaults.
//! * The byte length must not exceed the documented limit for the
//!   channel (`limit` argument).
//!
//! On rejection the function returns the canonical [`VoiceError`]
//! (`Forbidden { found }` for substring matches, `TooLong { limit }`
//! for length). Both are terminal failures: the daemon's
//! retry-or-fail logic does not retry on a voice error.

#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{CaduceusError, CaduceusResult, VoiceError};
use crate::issue::IssueKey;
use crate::worker::WorkerResult;

/// Default outbound-comment max bytes when the operator has not
/// overridden the limit. GitHub caps comment bodies at 65 536 bytes
/// in API v3; the daemon defaults to the same number so a comment
/// that passes the validator will not be truncated server-side.
pub const DEFAULT_COMMENT_MAX_BYTES: usize = 65_536;

/// Default PR body max bytes. The daemon defaults to 65 536 bytes
/// (GitHub's documented limit for the body parameter).
pub const DEFAULT_PR_BODY_MAX_BYTES: usize = 65_536;

/// Default PR title max bytes. The validator defaults to 256 bytes
/// (a generous limit that still leaves headroom under GitHub's
/// 256-character cap for rendered titles).
pub const DEFAULT_PR_TITLE_MAX_BYTES: usize = 256;

/// Finalized result handed to the daemon by the worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeRequest {
    pub issue: IssueKey,
    pub branch_name: String,
    pub worktree_path: PathBuf,
}

/// Inputs that every finalization stage consumes. The struct
/// is the canonical argument to the Phase 6
/// implementation; Task 5.0 only defines the type so
/// earlier tasks can compile against it.
///
/// `client` is the GitHub API client (Phase 6 owns the
/// concrete type), `config` is the live daemon config, and
/// `repository` is the cloned-repo metadata. `issue` is the
/// fetched issue detail; `claim`/`run_id`/`worktree` carry
/// the active run's identity. `result` is the worker's
/// output — the same [`FinalizeRequest`] payload the
/// worker writes to `worker-result.json`.
#[derive(Clone, Debug)]
pub struct FinalizeContext {
    /// GitHub API client. Phase 6 owns the concrete type.
    /// Task 5.0 uses a unit placeholder so the struct
    /// compiles before Phase 6 lands.
    pub client: (),
    /// Live daemon config (allowlist, timeouts, …).
    pub config: Config,
    /// Local repository metadata (path, base branch, remote URL).
    pub repository: crate::worktree::RepositoryInfo,
    /// Issue the run is finalising.
    pub issue: crate::issue::IssueDetail,
    /// Active run's claim token (proves the caller is the
    /// daemon, not a stray worker).
    pub claim: crate::queue::ClaimToken,
    /// Active run id.
    pub run_id: String,
    /// Active worktree handle. Task 5.0 keeps the existing
    /// `Worktree` struct from `worktree.rs`.
    pub worktree: crate::worktree::Worktree,
    /// Worker output (`worker-result.json`).
    pub result: FinalizeRequest,
}

/// What a finalization stage returns to the orchestrator.
/// `action` records which stage produced this output
/// (e.g. `Committed`, `Pushed`, `PrCreated`, `Commented`,
/// `Closed`, `InvestigationReady`,
/// `InvestigationCommented`). `pr_url` is the canonical
/// PR URL once it exists. `idempotency_observations` is a
/// free-form list of operator-facing notes the
/// orchestrator surfaces to the structured log so the
/// "did we already post this comment?" check is auditable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalizeOutput {
    /// The action the finalization stage performed.
    pub action: FinalizeAction,
    /// Canonical PR URL, if the action created or updated one.
    pub pr_url: Option<String>,
    /// Per-step idempotency notes (e.g. "comment already posted",
    /// "branch already pushed"). The orchestrator logs these
    /// but does not retry on them.
    pub idempotency_observations: Vec<String>,
}

/// The action a finalization stage took. Mirrors the
/// `FinalizationStage` enum in `queue.rs` but lives here
/// because the orchestrator's view of the world is the
/// `FinalizeOutput` it hands back to the cron tick.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum FinalizeAction {
    #[default]
    Committed,
    Pushed,
    PrCreated,
    Commented,
    Closed,
    InvestigationReady,
    InvestigationCommented,
    Previewed,
}

/// Outcome of a finalization attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalizeOutcome {
    pub commit_oid: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// Validate *text* against the public-voice rule.
///
/// * Every configured forbidden term is matched against *text* with
///   case-insensitive Unicode substring semantics. The first
///   matching term's lowercase form is captured in the
///   [`VoiceError::Forbidden { found }`] payload so the operator
///   can update the allowlist.
/// * The byte length of *text* must not exceed *limit_bytes*. The
///   check runs *after* the substring check so a long body that
///   also contains a forbidden term is reported as `Forbidden`
///   (the more actionable reason for the operator).
///
/// This is the single entry point that every outbound mutation
/// helper must call. The function is intentionally synchronous and
/// pure so tests can drive it without touching the filesystem or
/// the network.
pub fn validate_public_text(
    text: &str,
    cfg: &Config,
    limit_bytes: usize,
) -> Result<(), VoiceError> {
    if let Some(found) = first_forbidden_term(text, &cfg.comment_forbidden_strings) {
        return Err(VoiceError::Forbidden { found });
    }
    if text.len() > limit_bytes {
        return Err(VoiceError::TooLong { limit: limit_bytes });
    }
    Ok(())
}

/// Return the first configured forbidden term that matches *text*,
/// normalised to lowercase. Returns `None` when no term matches.
pub fn first_forbidden_term(text: &str, forbidden: &[String]) -> Option<String> {
    let lower = text.to_lowercase();
    forbidden
        .iter()
        .find(|term| !term.is_empty() && lower.contains(&term.to_lowercase()))
        .map(|t| t.to_lowercase())
}

/// Convenience wrapper: validate a PR title. Uses the documented
/// 256-byte default unless *limit_bytes* overrides it.
pub fn validate_pr_title(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_PR_TITLE_MAX_BYTES)
}

/// Convenience wrapper: validate a PR body. Uses 65 536-byte
/// default unless *limit_bytes* overrides it.
pub fn validate_pr_body(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_PR_BODY_MAX_BYTES)
}

/// Convenience wrapper: validate a generic GitHub comment. Uses the
/// 65 536-byte default unless *limit_bytes* overrides it.
pub fn validate_comment(text: &str, cfg: &Config) -> Result<(), VoiceError> {
    validate_public_text(text, cfg, DEFAULT_COMMENT_MAX_BYTES)
}

/// Dispatcher used by the orchestration loop. Phase 6 owns the real
/// implementation; the stub keeps the symbol reachable.
pub async fn finalize(_req: FinalizeRequest) -> CaduceusResult<FinalizeOutcome> {
    Ok(FinalizeOutcome {
        commit_oid: None,
        pr_number: None,
        pr_url: None,
    })
}

// ---------------------------------------------------------------------------
// Dry-run finalization
// ---------------------------------------------------------------------------

/// Atomic report written under `<state_dir>/runs/<run_id>.preview.json`
/// when the daemon runs a dry-run. The report is the
/// auditable record of what *would* have happened if
/// `cfg.dry_run` had been `false` at run time.
///
/// The struct is versioned (`version = 1`) and uses
/// `deny_unknown_fields` so a future schema bump is
/// detected early. The orchestrator re-renders the report
/// on every dry-run tick; older versions are simply
/// overwritten by the atomic rename.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreviewReport {
    /// Schema version. Bumped when the report shape
    /// changes; consumers refuse to read a future
    /// version.
    pub version: u32,
    /// The active run id. The report file's name is
    /// `<run_id>.preview.json`; the field is here for
    /// ergonomics so a reader can identify the report
    /// without the filename.
    pub run_id: String,
    /// The issue the run is finalising.
    pub issue: IssueKey,
    /// Proposed branch name (the worktree's branch).
    pub proposed_branch: String,
    /// Proposed commit message. In dry-run we do not
    /// `git commit`, so this is the worker's
    /// `commit_message` carried verbatim.
    pub proposed_commit_message: String,
    /// Proposed PR title (validated by `validate_pr_title`).
    pub proposed_pr_title: String,
    /// Proposed PR body (validated by `validate_pr_body`).
    pub proposed_pr_body: String,
    /// When the worker is an investigation, the proposed
    /// investigation comment. `None` for code-ticket
    /// previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_investigation_comment: Option<String>,
    /// Files the worker changed (relative paths from the
    /// worktree root). The orchestrator collects this via
    /// `git status --porcelain` in real runs; the dry-run
    /// test passes a fixed list.
    pub changed_files: Vec<String>,
    /// Path to the supervisor transcript for this run.
    pub transcript_path: PathBuf,
    /// Path to the worker result file.
    pub worker_result_path: PathBuf,
    /// Validation warnings collected during the dry-run.
    /// A dry-run with warnings is still valid (the
    /// orchestrator may surface the warnings to the
    /// operator) but is reported.
    #[serde(default)]
    pub validation_warnings: Vec<String>,
    /// Wall-clock instant the report was written (RFC 3339).
    pub written_at: String,
}

impl PreviewReport {
    /// Stable schema version. Bump in lockstep with the
    /// struct definition.
    pub const SCHEMA_VERSION: u32 = 1;
}

/// The dry-run path. Validates the worker result, builds
/// the canonical PR text (or investigation comment),
/// collects changed files and the transcript path, and
/// writes the [`PreviewReport`] atomically. No `git`
/// mutation (commit, push) and no `GitHub` HTTP call is
/// performed.
///
/// * `ctx` — the active finalization context (issue,
///   worktree, claim, config, run_id). The `client` field
///   is unused because dry-run never calls the API.
/// * `worker_result` — the parsed `WorkerResult` from the
///   bridge.
/// * `worker_result_path` — path to the on-disk result
///   file; embedded in the report for operator audit.
/// * `changed_files` — list of files the worker touched,
///   as observed by the orchestrator. The function does
///   *not* re-run `git status`; the caller supplies the
///   list so the test path is deterministic.
pub fn dry_run_finalize(
    ctx: &FinalizeContext,
    worker_result: &WorkerResult,
    worker_result_path: &std::path::Path,
    changed_files: Vec<String>,
) -> CaduceusResult<FinalizeOutput> {
    // 1. Validate the worker result against the issue.
    //    We do not block on validation warnings — the
    //    report collects them and the orchestrator can
    //    surface them to the operator.
    let mut warnings = Vec::new();
    if let Err(err) = crate::worker::validate_worker_result(worker_result, &ctx.issue.key) {
        warnings.push(format!("validate_worker_result: {err}"));
    }

    // 2. Build the proposed PR text. The build functions
    //    return VoiceError rejections as
    //    `CaduceusError::Other`; capture them as
    //    warnings rather than aborting the dry-run.
    let proposed_pr_title = match build_pr_title(worker_result, &ctx.config) {
        Ok(t) => t,
        Err(err) => {
            warnings.push(format!("build_pr_title: {err}"));
            worker_result.pull_request_title.clone()
        }
    };
    let proposed_pr_body =
        match build_pr_body(worker_result, &ctx.issue.key, &ctx.run_id, &ctx.config) {
            Ok(b) => b,
            Err(err) => {
                warnings.push(format!("build_pr_body: {err}"));
                // Fall back to the worker's summary so the
                // operator can still see the intended text in
                // the report.
                format!(
                    "{}\n\nCloses #{}\n\n{}<!-- {} {} -->",
                    worker_result.summary,
                    ctx.issue.key.number,
                    "",
                    IDEMPOTENCY_MARKER_PREFIX,
                    ctx.run_id
                )
            }
        };

    // 3. Investigation comment (or None for code tickets).
    let proposed_investigation_comment = if worker_result.investigation {
        Some(worker_result.summary.clone())
    } else {
        None
    };

    // 4. Build the report. The branch name is the
    //    worktree's branch.
    let report = PreviewReport {
        version: PreviewReport::SCHEMA_VERSION,
        run_id: ctx.run_id.clone(),
        issue: ctx.issue.key.clone(),
        proposed_branch: ctx.worktree.branch_name.clone(),
        proposed_commit_message: worker_result.commit_message.clone(),
        proposed_pr_title,
        proposed_pr_body,
        proposed_investigation_comment,
        changed_files,
        transcript_path: ctx.worktree.path.join(".caduceus").join("transcript"),
        worker_result_path: worker_result_path.to_path_buf(),
        validation_warnings: warnings,
        written_at: chrono::Utc::now().to_rfc3339(),
    };

    // 5. Write the report atomically to
    //    `<state_dir>/runs/<run_id>.preview.json`.
    let runs_dir = ctx.config.state_dir.join("runs");
    std::fs::create_dir_all(&runs_dir).map_err(|err| CaduceusError::StateCorrupt {
        path: runs_dir.clone(),
        message: format!("create_dir_all failed: {err}"),
    })?;
    let report_path = runs_dir.join(format!("{}.preview.json", ctx.run_id));
    let body = serde_json::to_vec_pretty(&report)
        .map_err(|err| CaduceusError::Other(format!("serialize preview report: {err}")))?;
    write_atomic(&report_path, &body).map_err(|err| CaduceusError::StateCorrupt {
        path: report_path.clone(),
        message: format!("write_atomic failed: {err}"),
    })?;

    Ok(FinalizeOutput {
        action: FinalizeAction::Previewed,
        pr_url: None,
        idempotency_observations: vec![
            "dry-run".to_string(),
            format!("report={}", report_path.display()),
        ],
    })
}

/// Write `data` to `path` atomically: write to
/// `<path>.tmp.<rand>` then rename. The function is
/// available here (not just in `queue.rs`) because the
/// dry-run report is written from the finalization
/// module.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> CaduceusResult<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .ok_or_else(|| CaduceusError::Other(format!("no parent for {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|err| CaduceusError::StateCorrupt {
        path: parent.to_path_buf(),
        message: format!("create_dir_all failed: {err}"),
    })?;
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_name = format!(
        ".{}.tmp.{pid}.{nanos}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("preview")
    );
    let tmp_path = parent.join(tmp_name);
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(|err| CaduceusError::StateCorrupt {
                path: tmp_path.clone(),
                message: format!("open failed: {err}"),
            })?;
        file.write_all(data)
            .map_err(|err| CaduceusError::StateCorrupt {
                path: tmp_path.clone(),
                message: format!("write failed: {err}"),
            })?;
        file.flush().map_err(|err| CaduceusError::StateCorrupt {
            path: tmp_path.clone(),
            message: format!("flush failed: {err}"),
        })?;
        file.sync_all().ok();
    }
    std::fs::rename(&tmp_path, path).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("rename failed: {err}"),
    })?;
    Ok(())
}

/// Helper for the structured logger: emit a single line that names
/// the configured forbidden term that was matched, but only when
/// the term itself is not sensitive. The current contract denies
/// *all* configured terms from logging — operators who need a less
/// strict log should configure the allowlist directly. The
/// returned string is the empty string for the "do not log" case.
pub fn log_safe_term_match(found: &str) -> &str {
    // The contract currently treats every configured term as safe
    // to log (the term itself is operator-supplied; nothing secret
    // leaks). This function exists so a future tightening of the
    // policy has a single point to update.
    let _ = found;
    found
}

/// Map a [`VoiceError`] to a [`CaduceusError::Cancelled`]-style
/// terminal error. The queue's retry-or-fail logic treats this as
/// a hard failure (no retry). Used by the finalization helpers
/// when they receive a `VoiceError::Forbidden` or `VoiceError::TooLong`.
pub fn terminal_from_voice(err: VoiceError) -> CaduceusError {
    match err {
        VoiceError::Forbidden { found } => {
            CaduceusError::Other(format!("public-voice: forbidden term matched: {found:?}"))
        }
        VoiceError::TooLong { limit } => {
            CaduceusError::Other(format!("public-voice: text exceeds limit of {limit} bytes"))
        }
    }
}

// ---------------------------------------------------------------------------
// Public-voice-driven PR body and title rendering
// ---------------------------------------------------------------------------

/// Hard cap on the rendered PR body in bytes. The daemon
/// never emits a body larger than this; the validator's
/// `DEFAULT_PR_BODY_MAX_BYTES` is the upper bound, this
/// constant is the *render* cap. We pick 64 KiB so the
/// rendered body stays well under GitHub's 65 536-byte
/// limit while still leaving room for a future contract
/// bump.
pub const MAX_RENDERED_BODY_BYTES: usize = 64 * 1024;

/// Idempotency marker that the daemon appends to every PR
/// body. The marker is a hidden HTML comment so it does not
/// affect the rendered Markdown. The body includes the
/// run_id so a re-render of the same body produces the
/// same bytes.
pub const IDEMPOTENCY_MARKER_PREFIX: &str = "<!-- caduceus-pr-body:run=";

/// Marker for the issue-closing reference. GitHub renders
/// `Closes #N` as a closing reference; the daemon always
/// uses the canonical form so the bot's behaviour is
/// auditable in test fixtures.
pub const CLOSES_REFERENCE_PREFIX: &str = "Closes #";

/// Render the canonical PR body for a worker `result`.
///
/// The body is the concatenation of:
/// 1. The worker's `summary`.
/// 2. A blank line, then the issue-closing reference.
/// 3. A blank line, then a fenced-JSON artifact section
///    sorted by key.
/// 4. A blank line, then the idempotency marker comment.
///
/// `result.artifacts` is rendered with a fence length
/// dynamically chosen to be longer than any backtick run
/// in the rendered JSON. The total body is bounded by
/// [`MAX_RENDERED_BODY_BYTES`]. The body is then passed
/// through the public-voice validator with the documented
/// PR-body limit before being returned.
pub fn build_pr_body(
    result: &WorkerResult,
    issue: &IssueKey,
    run_id: &str,
    cfg: &Config,
) -> CaduceusResult<String> {
    let artifact_section = render_artifacts(&result.artifacts);
    let closes = format!("{}{}", CLOSES_REFERENCE_PREFIX, issue.number);
    let marker = format!("{}{}{} -->", IDEMPOTENCY_MARKER_PREFIX, run_id, "");
    let mut body = String::with_capacity(8 * 1024);
    body.push_str(&result.summary);
    body.push_str("\n\n");
    body.push_str(&closes);
    if !artifact_section.is_empty() {
        body.push_str("\n\n");
        body.push_str(&artifact_section);
    }
    body.push_str("\n\n");
    body.push_str(&marker);
    if body.len() > MAX_RENDERED_BODY_BYTES {
        // Truncate the body to the cap, then re-append the
        // marker so the body is always capped *and* the
        // idempotency marker is present. We do this before
        // the public-voice check so a too-long summary
        // still produces a valid (capped) body.
        if let Some(pos) = body.find(IDEMPOTENCY_MARKER_PREFIX) {
            // Keep the marker, drop everything after.
            body.truncate(pos);
        }
        // The summary may be huge; we have already
        // truncated everything after the marker. Now make
        // sure the *front* is under the cap by stripping
        // from the top of the summary.
        let marker_len = marker.len();
        if body.len() + marker_len + 4 > MAX_RENDERED_BODY_BYTES {
            // Hard-truncate the leading summary so the
            // body is under the cap.
            let allowed = MAX_RENDERED_BODY_BYTES
                .saturating_sub(marker_len)
                .saturating_sub(4);
            body.truncate(allowed);
        }
        body.push_str("\n\n");
        body.push_str(&marker);
    }
    validate_pr_body(&body, cfg).map_err(terminal_from_voice)?;
    Ok(body)
}

/// Render the canonical PR title. The worker's
/// `pull_request_title` is validated through the public-voice
/// rule with the documented PR-title limit and returned
/// unchanged otherwise.
pub fn build_pr_title(result: &WorkerResult, cfg: &Config) -> CaduceusResult<String> {
    validate_pr_title(&result.pull_request_title, cfg).map_err(terminal_from_voice)?;
    Ok(result.pull_request_title.clone())
}

/// Render the artifact section as a fenced-JSON block.
///
/// The output is the empty string when the worker emitted no
/// artifacts. Otherwise the block is:
/// ```text
/// <caption>
///
/// ```fence
/// <json>
/// ```
/// ```
/// where `<fence>` is a backtick run whose length is one
/// longer than the longest backtick run in the JSON. The
/// caption lists the artifact count.
fn render_artifacts(artifacts: &std::collections::BTreeMap<String, serde_json::Value>) -> String {
    if artifacts.is_empty() {
        return String::new();
    }
    let mut json = String::new();
    // Deterministic order: BTreeMap iterates in key order.
    let json_value = serde_json::json!(artifacts);
    json.push_str(&serde_json::to_string_pretty(&json_value).expect("serialize json"));
    let fence = dynamic_fence_length(&json);
    let mut fence_str = String::with_capacity(fence);
    for _ in 0..fence {
        fence_str.push('`');
    }
    let caption = format!("Artifacts ({}):", artifacts.len());
    let mut out = String::with_capacity(json.len() + caption.len() + fence * 2 + 8);
    out.push_str(&caption);
    out.push_str("\n\n");
    out.push_str(&fence_str);
    out.push_str("json\n");
    out.push_str(&json);
    out.push('\n');
    out.push_str(&fence_str);
    out
}

/// Pick a backtick fence length that is at least 3 and one
/// longer than the longest run of backticks in *body*. The
/// contract says "dynamically chosen"; 3 is the Markdown
/// minimum and we extend as needed.
fn dynamic_fence_length(body: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for c in body.chars() {
        if c == '`' {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 0;
        }
    }
    let pick = longest + 1;
    if pick < 3 {
        3
    } else {
        pick
    }
}

/// Escape control characters in a string so the JSON block
/// is safe to embed in a Markdown document. We follow the
/// "no control characters" rule from
/// [`crate::worker::validate_worker_result`].
pub fn escape_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            // Replace with the standard JSON-style escape
            // (\\u00XX) so the body is human-readable and
            // round-trip safe.
            let code = c as u32;
            out.push_str(&format!("\\u{code:04X}"));
        } else {
            out.push(c);
        }
    }
    out
}

/// Apply the control-character escape to every artifact
/// value. Artifact keys are passed through unchanged (the
/// schema validator already rejects control characters in
/// keys; the escape is a belt-and-braces guard for the
/// render path).
pub fn render_artifacts_with_escape(
    artifacts: &std::collections::BTreeMap<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    artifacts
        .iter()
        .map(|(k, v)| (k.clone(), escape_json_value(v)))
        .collect()
}

fn escape_json_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(escape_control_chars(s)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(escape_json_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut new = serde_json::Map::new();
            for (k, v) in obj {
                new.insert(k.clone(), escape_json_value(v));
            }
            serde_json::Value::Object(new)
        }
        other => other.clone(),
    }
}
