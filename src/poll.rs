//! Polling: discover watched repos and merge labeled open issues into the
//! queue. Phase 2 implements the body; the stub defines the typed surface.
//!
//! The repository discovery path is implemented in this task (2.2);
//! the labeled-issue poll and verification flows land in Tasks 2.3
//! and 2.5 respectively. The module exposes the public surface
//! owned by Phase 2; per-task additions are layered on top.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use reqwest::header::LINK;
use serde::Deserialize;
use url::Url;

use crate::config::{is_valid_repo_slug, Config};
use crate::error::{CaduceusError, CaduceusResult};
use crate::github::{Client, Response, ACCEPT_VALUE};
use crate::issue::IssueKey;

/// Hard maximum pages the daemon will follow on a single paginated
/// endpoint. The contract requires an error rather than silent
/// truncation when the limit is exceeded.
pub const MAX_PAGES_PER_ENDPOINT: usize = 20;

/// Number of repositories fetched per page on `/user/repos`. The
/// contract pins the GitHub-side maximum so a single page always
/// fits the documented 100-row envelope.
pub const REPOS_PER_PAGE: u32 = 100;

/// One open issue surfaced by polling.
#[derive(Debug)]
pub struct PollHit {
    pub key: IssueKey,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub labels: Vec<String>,
    pub ambiguous: bool,
}

/// Discover the watched repositories for the current tick. If
/// ``cfg.watched_repos`` is non-empty, return its validated
/// sorted, case-insensitively deduplicated contents without any
/// HTTP call. Otherwise paginate ``GET /user/repos?per_page=100
/// &sort=full_name`` until the response no longer carries a
/// ``rel="next"`` Link header, dropping archived and disabled
/// repositories, and capped at [`MAX_PAGES_PER_ENDPOINT`] pages.
///
/// Repository discovery uses the persistent HTTP cache (Task 2.1),
/// so repeat ticks are bandwidth-friendly. The configured bypass
/// is fully deterministic — no cache lookup is attempted.
pub async fn discover_watched_repos(client: &Client, cfg: &Config) -> CaduceusResult<Vec<String>> {
    if !cfg.watched_repos.is_empty() {
        return Ok(validated_configured_repos(&cfg.watched_repos));
    }
    discover_via_api(client).await
}

fn validated_configured_repos(configured: &[String]) -> Vec<String> {
    // Per CONTRACTS.md "Issue identity and queue schema":
    // configured repositories deduplicate case-insensitively.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for raw in configured {
        if !is_valid_repo_slug(raw) {
            // Configuration validation already rejected malformed
            // slugs at load time; the inner check here is a safety
            // net for callers that bypass `Config::from_raw`.
            continue;
        }
        let key = raw.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(raw.clone());
        }
    }
    out.sort_by_key(|a| a.to_ascii_lowercase());
    out
}

async fn discover_via_api(client: &Client) -> CaduceusResult<Vec<String>> {
    let initial_path = format!("/user/repos?per_page={}&sort=full_name", REPOS_PER_PAGE);
    let mut next: Option<Url> = Some(client.base_url().clone().join(&initial_path).map_err(
        |err| {
            CaduceusError::Config(format!(
                "cannot join /user/repos onto api_base {}: {err}",
                client.base_url()
            ))
        },
    )?);
    let mut collected: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pages = 0usize;

    while let Some(url) = next.take() {
        if pages >= MAX_PAGES_PER_ENDPOINT {
            return Err(CaduceusError::Other(format!(
                "repository discovery exceeded {MAX_PAGES_PER_ENDPOINT} pages"
            )));
        }
        pages += 1;
        let response = client.get_url(&url, ACCEPT_VALUE).await?;
        // Rate-limit responses carry `x-ratelimit-remaining: 0` plus
        // a `x-ratelimit-reset` Unix timestamp; surface them as a
        // typed RateLimited error so the meta layer can persist the
        // observation (Task 2.4).
        if let Some(observation) = rate_limit_from_headers(&response) {
            return Err(observation);
        }
        // The response body must be a JSON array of repository
        // objects. Anything else is malformed.
        let repos: Vec<RepoObject> = serde_json::from_slice(&response.body).map_err(|err| {
            CaduceusError::Other(format!(
                "repository discovery page {pages}: JSON parse: {err}"
            ))
        })?;
        for repo in repos {
            if repo.archived || repo.disabled {
                continue;
            }
            let Some(full_name) = repo.full_name else {
                return Err(CaduceusError::Other(format!(
                    "repository discovery page {pages}: missing full_name"
                )));
            };
            if !is_valid_repo_slug(&full_name) {
                return Err(CaduceusError::Other(format!(
                    "repository discovery page {pages}: invalid slug {full_name}"
                )));
            }
            let key = full_name.to_ascii_lowercase();
            if seen.insert(key) {
                collected.push(full_name);
            }
        }
        // GitHub returns the next page URL in the Link header
        // (`<url>; rel="next"`). Absent or no rel="next" → done.
        next = parse_next_link(&response);
    }

    // Stable order so callers can diff snapshots across ticks.
    collected.sort();
    Ok(collected)
}

/// Parse the GitHub Link header and return the URL marked
/// `rel="next"` if any. The header looks like
/// `<https://api.github.com/...?page=2>; rel="next", <...>; rel="last"`.
fn parse_next_link(response: &Response) -> Option<Url> {
    let header = response.headers.get(LINK)?.to_str().ok()?;
    next_url_from_link_header(header).and_then(|raw| Url::parse(&raw).ok())
}

/// Extract the `rel="next"` URL out of a raw Link header. Returns
/// `None` when no `rel="next"` is present (signalling the last
/// page) or when the URL cannot be parsed. Exposed as `pub` so
/// the test suite can drive it directly without a network fixture.
pub fn next_url_from_link_header(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let mut parts = segment.split(';');
        let url_part = parts.next()?.trim();
        let url = url_part
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))?;
        let mut is_next = false;
        for rel in parts {
            let rel = rel.trim();
            if rel == "rel=\"next\"" {
                is_next = true;
                break;
            }
        }
        if is_next {
            return Some(url.to_string());
        }
    }
    None
}

/// Translate GitHub rate-limit headers into a typed error.
/// `x-ratelimit-remaining: 0` plus a `x-ratelimit-reset` Unix
/// timestamp is the documented exhaustion signal. Other pages may
/// carry the headers but still succeed; we treat a remaining of
/// zero as the threshold so a successful response is never
/// mis-classified as rate-limited.
fn rate_limit_from_headers(response: &Response) -> Option<CaduceusError> {
    let remaining = response
        .headers
        .get("x-ratelimit-remaining")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())?;
    let limit = response
        .headers
        .get("x-ratelimit-limit")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());
    let reset_unix = response
        .headers
        .get("x-ratelimit-reset")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())?;
    if remaining != 0 {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    let reset_at = (reset_unix.saturating_sub(now)).max(0) as u64;
    Some(CaduceusError::RateLimited {
        reset_at,
        remaining,
        limit,
    })
}

/// One row of the GitHub `/user/repos` payload. We deliberately
/// decode only the fields the discovery loop needs; unknown
/// fields are silently ignored so the daemon does not break on
/// benign schema additions.
#[derive(Debug, Deserialize)]
struct RepoObject {
    full_name: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    disabled: bool,
}

/// Poll for label `ticket_label_code`.
pub async fn poll_code(_now: DateTime<Utc>) -> CaduceusResult<Vec<PollHit>> {
    Ok(Vec::new())
}

/// Poll for label `ticket_label_investigation`.
pub async fn poll_investigation(_now: DateTime<Utc>) -> CaduceusResult<Vec<PollHit>> {
    Ok(Vec::new())
}
