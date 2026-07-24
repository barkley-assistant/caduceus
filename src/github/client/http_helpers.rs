//! HTTP helper functions and constants used by the GitHub API client.

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

/// HTTP header name for the GitHub API version pin.
pub const GITHUB_API_VERSION_HEADER: &str = "X-GitHub-Api-Version";
/// Value of the API version pin per `CONTRACTS.md` "Polling contract".
pub const GITHUB_API_VERSION_VALUE: &str = "2022-11-28";
/// Accept header for the GitHub JSON API.
pub const ACCEPT_VALUE: &str = "application/vnd.github+json";
/// User-Agent prefix the daemon sends on every request.
pub const USER_AGENT_PREFIX: &str = "caduceus";
/// Maximum number of redirects allowed per request.
pub const MAX_REDIRECTS: usize = 3;
/// Connect timeout enforced on every outbound request.
pub const CONNECT_TIMEOUT_SECONDS: u64 = 10;
/// Streaming body cap before full allocation (10 MiB).
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
/// Filename of the persistent HTTP cache under `state_dir/cache/`.
pub const HTTP_CACHE_FILENAME: &str = "http.json";

pub(crate) const BODY_TOO_LARGE_SENTINEL: &str = "caduceus::github::body_too_large";

pub(crate) async fn read_bounded_body(response: reqwest::Response) -> CaduceusResult<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(CaduceusError::Other(BODY_TOO_LARGE_SENTINEL.to_string()));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

pub(crate) fn map_status(status: u16, message: String) -> CaduceusError {
    // Strip any leaked credential values before the message lands in
    // a Display/Debug path. The scrub helper handles the three
    // documented credential names; the daemon's structured logger
    // and any test failure render through `Display`, so we must
    // scrub here regardless of how the variant is later rendered.
    let scrubbed = crate::infra::error::scrub(&message);
    match status {
        401 | 403 | 404 | 500 => CaduceusError::GitHubApi {
            status,
            message: scrubbed,
        },
        429 => {
            // Rate-limited responses carry headers; the caller is
            // expected to observe them via RateLimitObserver. We
            // surface the message as a regular GitHubApi error so
            // the daemon's tick wrapper can map it; rate-limit
            // parsing is owned by the meta layer.
            CaduceusError::GitHubApi {
                status,
                message: scrubbed,
            }
        }
        _ => CaduceusError::GitHubApi {
            status,
            message: scrubbed,
        },
    }
}

pub(crate) fn same_origin(base: &Url, candidate: &Url) -> bool {
    base.scheme() == candidate.scheme()
        && base.host_str() == candidate.host_str()
        && base.port_or_known_default() == candidate.port_or_known_default()
}

pub(crate) fn join_path(base: &mut Url, path: &str) -> CaduceusResult<()> {
    if path.starts_with('/') {
        base.set_path(path);
    } else {
        let mut combined = base.path().trim_end_matches('/').to_string();
        combined.push('/');
        combined.push_str(path.trim_start_matches('/'));
        base.set_path(&combined);
    }
    Ok(())
}

pub(crate) fn split_query(input: &str) -> (&str, &str) {
    match input.split_once('?') {
        Some((path, query)) => (path, query),
        None => (input, ""),
    }
}

pub(crate) fn header_value(value: &str) -> HeaderValue {
    // The headers we set are static (and the Accept value comes
    // from the caller-supplied Accept value, which is also
    // validated). The unwrap_or fallback is defensive only.
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}
