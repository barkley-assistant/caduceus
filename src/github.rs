//! Typed GitHub API surface. `HttpClient`, repositories endpoint, issues
//! endpoint, and ETag-aware conditional GET are owned here.
//!
//! Every outbound mutation that posts a comment, a pull-request
//! title, or a pull-request body MUST route through
//! [`check_voice_or_error`] first. The validator is the single
//! entry point for the public-voice rule; nothing else in the
//! crate bypasses it.

#![allow(dead_code)]

use std::sync::Arc;

use crate::config::Config;
use crate::error::{CaduceusError, CaduceusResult, VoiceError};
use crate::finalize::{
    validate_comment, validate_pr_body, validate_pr_title, validate_public_text,
};
use crate::issue::IssueKey;

/// HTTP client wrapper carrying the resolved token and the cached HTTP
/// state. Constructed once per tick.
#[derive(Debug)]
pub struct HttpClient {
    pub base_url: Arc<str>,
}

impl HttpClient {
    pub fn new(base_url: impl Into<Arc<str>>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

/// Lookup result for an issue summary by key.
#[derive(Debug)]
pub struct IssueSummary {
    pub key: IssueKey,
    pub title: String,
    pub labels: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn fetch_issue(_client: &HttpClient, _key: &IssueKey) -> CaduceusResult<IssueSummary> {
    Ok(IssueSummary {
        key: IssueKey {
            owner: String::new(),
            repo: String::new(),
            number: 0,
        },
        title: String::new(),
        labels: Vec::new(),
        updated_at: chrono::Utc::now(),
    })
}

/// HTTP helper for posting an issue comment. The helper is the
/// only legitimate path for a comment to leave the daemon; tests
/// use [`VoiceError`] to assert the validator's role.
pub fn post_issue_comment(
    _client: &HttpClient,
    _key: &IssueKey,
    body: &str,
    cfg: &Config,
) -> CaduceusResult<()> {
    check_voice_or_error(body, cfg, VoiceChannel::Comment)?;
    // Real implementation lives in Task 6.x; the stub is here so
    // callers and tests can wire through the validator today.
    Ok(())
}

/// HTTP helper for posting or updating a pull-request title and
/// body. Both fields are validated; the title uses the 256-byte
/// limit and the body uses the 65 536-byte limit.
pub fn post_pull_request(
    _client: &HttpClient,
    _key: &IssueKey,
    title: &str,
    body: &str,
    cfg: &Config,
) -> CaduceusResult<()> {
    check_voice_or_error(title, cfg, VoiceChannel::PrTitle)?;
    check_voice_or_error(body, cfg, VoiceChannel::PrBody)?;
    Ok(())
}

/// HTTP helper for the investigation comment. Uses the comment
/// 65 536-byte limit.
pub fn post_investigation_comment(
    _client: &HttpClient,
    _key: &IssueKey,
    body: &str,
    cfg: &Config,
) -> CaduceusResult<()> {
    check_voice_or_error(body, cfg, VoiceChannel::Comment)?;
    Ok(())
}

/// Distinguishes which validator to apply. The mapping is fixed by
/// the contract; this enum exists so the helper cannot accept an
/// arbitrary limit and accidentally weaken the rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceChannel {
    Comment,
    PrTitle,
    PrBody,
    /// Free-form text with an explicit caller-supplied limit
    /// (used by tests and by rare helpers that need a custom
    /// bound). The caller MUST pass a limit that matches the
    /// channel.
    Custom(usize),
}

/// Run the public-voice check for *text* against *channel* and
/// return a [`CaduceusError::Other`] on rejection. The check is
/// the single chokepoint for outbound public-text mutations.
pub fn check_voice_or_error(text: &str, cfg: &Config, channel: VoiceChannel) -> CaduceusResult<()> {
    let result = match channel {
        VoiceChannel::Comment => validate_comment(text, cfg),
        VoiceChannel::PrTitle => validate_pr_title(text, cfg),
        VoiceChannel::PrBody => validate_pr_body(text, cfg),
        VoiceChannel::Custom(limit) => validate_public_text(text, cfg, limit),
    };
    result.map_err(super_err_from_voice)
}

/// Convert a [`VoiceError`] into a [`CaduceusError`] for callers
/// that consume the helper through the unified error path. The
/// matcher keeps the structured logger's "Other" tag generic; the
/// retry-or-fail logic can branch on the rendered message.
pub fn super_err_from_voice(err: VoiceError) -> CaduceusError {
    match err {
        VoiceError::Forbidden { found } => {
            CaduceusError::Other(format!("public-voice: forbidden term matched: {found:?}"))
        }
        VoiceError::TooLong { limit } => {
            CaduceusError::Other(format!("public-voice: text exceeds limit of {limit} bytes"))
        }
    }
}

/// Re-export the documented defaults so the helpers and the tests
/// share a single source of truth for the channel limits.
pub use crate::finalize::{
    DEFAULT_COMMENT_MAX_BYTES, DEFAULT_PR_BODY_MAX_BYTES, DEFAULT_PR_TITLE_MAX_BYTES,
};
