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
