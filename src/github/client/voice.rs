//! Public-voice validation and outbound text helpers for GitHub mutations.

#![allow(dead_code)]
#![allow(unused_imports)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::finalize::{
    validate_comment, validate_pr_body, validate_pr_title, validate_public_text,
};
use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult, VoiceError};

use super::client_core::Client;
/// Lookup result for an issue summary by key.
#[derive(Debug)]
pub struct IssueSummary {
    pub key: IssueKey,
    pub title: String,
    pub labels: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of a PR create-or-reuse call. The `mode`
/// records which path the orchestrator took: a fresh
/// POST or a single-matching reuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub reused: bool,
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

/// Backwards-compatible alias for [`Client`]. Earlier code paths
/// (and Task 6.6's voice-rule tests) refer to the HTTP client as
/// `HttpClient`; renaming the type would have rippled into tests
/// outside this task's ownership. The alias keeps the surface
/// stable while Phase 2 is in flight.
pub type HttpClient = Client;
