//! Typed GitHub API response and rate-limit observation.

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

/// Result of a typed HTTP GET. ``final_url`` captures the URL after
/// any allowed redirects so issue verification can detect a
/// transfer (a 301/302 to a different repo is a transfer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub final_url: String,
    pub body: Vec<u8>,
    /// Raw response headers (case-insensitive). Used by the
    /// pagination loop (Link header) and by the rate-limit observer
    /// (Task 2.4).
    pub headers: HeaderMap,
    /// True when the body was reused from the cache after a 304.
    pub from_cache: bool,
}

impl Response {
    pub fn body_text(&self) -> CaduceusResult<&str> {
        std::str::from_utf8(&self.body).map_err(|_| {
            CaduceusError::Other(format!(
                "response body is not valid UTF-8 ({} bytes)",
                self.body.len()
            ))
        })
    }

    /// Parsed GitHub rate-limit observation from this response's
    /// headers, if any. Returns `None` when the response does not
    /// carry the documented headers or when a header value is
    /// malformed. The caller persists the result via the
    /// [`meta::RateLimitObserver`].
    pub fn rate_limit_observation(&self) -> Option<RateLimitInfo> {
        rate_limit_from_headers(&self.headers, self.status)
    }

    /// Server-suggested `X-Poll-Interval` value, in seconds, if
    /// present. GitHub returns this on user-search and a few
    /// other endpoints; missing/malformed values are ignored.
    pub fn poll_interval_seconds(&self) -> Option<u64> {
        poll_interval_from_headers(&self.headers)
    }
}

/// Parsed GitHub rate-limit observation. The fields are
/// optional so the cadence / rate-limit gate can work with
/// partial headers; `remaining` is mandatory for the observer
/// to do anything useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitInfo {
    pub limit: Option<u32>,
    pub remaining: u32,
    pub reset_at_unix: i64,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

impl RateLimitInfo {
    /// Translate the seconds-until-reset relative to *now*.
    pub fn reset_at(&self, now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
        let seconds = (self.reset_at_unix - now.timestamp()).max(0);
        now + chrono::Duration::seconds(seconds)
    }
}

/// Parse `X-RateLimit-*` headers. Returns `None` when no
/// `X-RateLimit-Remaining` header is present. The
/// `status == 429` case is treated as exhaustion even if
/// `Remaining` is non-zero (e.g. legacy proxies that drop the
/// header on 429). The `meta` layer is responsible for the
/// `remaining == 0` policy when *not* a 429.
pub fn rate_limit_from_headers(headers: &HeaderMap, status: u16) -> Option<RateLimitInfo> {
    let remaining = headers
        .get("x-ratelimit-remaining")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())?;
    let limit = headers
        .get("x-ratelimit-limit")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());
    let reset_unix = headers
        .get("x-ratelimit-reset")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let observed_at = chrono::Utc::now();
    // 429 always counts as exhausted.
    let remaining = if status == 429 { 0 } else { remaining };
    Some(RateLimitInfo {
        limit,
        remaining,
        reset_at_unix: reset_unix,
        observed_at,
    })
}

/// Parse `X-Poll-Interval` (GitHub user-search) as seconds.
/// Malformed values return `None` so the caller can fall back to
/// the configured `poll_interval_seconds`.
pub fn poll_interval_from_headers(headers: &HeaderMap) -> Option<u64> {
    let raw = headers
        .get("x-poll-interval")
        .and_then(|h| h.to_str().ok())?
        .trim();
    raw.parse::<u64>().ok()
}
