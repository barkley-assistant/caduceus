//! Typed GitHub API HTTP client and request methods.

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

use super::cache::{cache_key, inert_cache, is_valid_etag, CacheEntry, HttpCache};
use super::http_helpers::{
    header_value, join_path, map_status, read_bounded_body, same_origin, split_query, ACCEPT_VALUE,
    BODY_TOO_LARGE_SENTINEL, CONNECT_TIMEOUT_SECONDS, GITHUB_API_VERSION_HEADER,
    GITHUB_API_VERSION_VALUE, MAX_BODY_BYTES, MAX_REDIRECTS, USER_AGENT_PREFIX,
};
use super::response::{RateLimitInfo, Response};
/// Typed HTTP client. Owns the underlying reqwest::Client, the
/// resolved token, the persistent ETag cache, and the
/// redirect/timeout policies.
#[derive(Debug)]
pub struct Client {
    base_url: Url,
    token: Option<String>,
    timeout: Duration,
    cache: Arc<HttpCache>,
    inner: reqwest::Client,
}

impl Client {
    /// Build an inert [`Client`] with just a base URL. Intended
    /// for type-only call sites (e.g. tests that wire a client
    /// through a helper but never make an HTTP request). The
    /// resulting client is *not* usable for real I/O — the cache
    /// and inner reqwest client are not initialised. Use
    /// [`Client::with_config`] (or [`Client::with_cache`]) for
    /// production.
    pub fn new(base_url: impl Into<Arc<str>>) -> Self {
        let url_text: Arc<str> = base_url.into();
        let parsed = Url::parse(&url_text).unwrap_or_else(|_| {
            Url::parse("https://api.github.com").expect("hard-coded fallback parses")
        });
        let inner = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
            .timeout(Duration::from_secs(60))
            .redirect(Policy::none())
            .build()
            .expect("inert reqwest client builds");
        Self {
            base_url: parsed,
            token: None,
            timeout: Duration::from_secs(60),
            cache: Arc::new(inert_cache()),
            inner,
        }
    }

    /// Construct a client from the daemon config. Loads or creates
    /// the persistent cache; configures the reqwest client with the
    /// 10-second connect timeout and the configured overall request
    /// timeout; disables automatic redirects (the daemon walks them
    /// itself so it can enforce the same-host policy).
    pub fn with_config(cfg: &Config) -> CaduceusResult<Self> {
        let base_url = Url::parse(&cfg.api_base).map_err(|err| {
            CaduceusError::Config(format!("invalid api_base {}: {err}", cfg.api_base))
        })?;
        let timeout = Duration::from_secs(cfg.http_timeout_seconds);
        let inner = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
            .timeout(timeout)
            .redirect(Policy::none())
            .user_agent(format!("{USER_AGENT_PREFIX}/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        let cache = HttpCache::open(&cfg.state_dir)?;
        Ok(Self {
            base_url,
            token: cfg.github_token.clone(),
            timeout,
            cache: Arc::new(cache),
            inner,
        })
    }

    /// Build a client with a caller-supplied cache (tests use this
    /// to point at a temporary directory).
    pub fn with_cache(cfg: &Config, cache: HttpCache) -> CaduceusResult<Self> {
        let base_url = Url::parse(&cfg.api_base).map_err(|err| {
            CaduceusError::Config(format!("invalid api_base {}: {err}", cfg.api_base))
        })?;
        let timeout = Duration::from_secs(cfg.http_timeout_seconds);
        let inner = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECONDS))
            .timeout(timeout)
            .redirect(Policy::none())
            .user_agent(format!("{USER_AGENT_PREFIX}/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base_url,
            token: cfg.github_token.clone(),
            timeout,
            cache: Arc::new(cache),
            inner,
        })
    }

    /// Expose the persistent cache for callers that need to
    /// pre-populate or inspect it (test seam).
    pub fn cache(&self) -> Arc<HttpCache> {
        Arc::clone(&self.cache)
    }

    /// The configured API base URL.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The configured per-request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Issue a conditional GET against *url_path* (joined onto
    /// ``api_base``) with the canonical GitHub headers. Stores
    /// successful bodies in the cache; on 304 reuses the last
    /// successfully parsed body verbatim.
    pub async fn get(&self, url_path: &str, accept: &str) -> CaduceusResult<Response> {
        let (path_only, query) = split_query(url_path);
        let mut url = self.base_url.clone();
        join_path(&mut url, path_only)?;
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        self.get_url(&url, accept).await
    }

    /// Same as [`Client::get`] but with a fully-qualified URL. Used
    /// by callers that need to follow a Link header without
    /// rebuilding the base URL.
    pub async fn get_url(&self, url: &Url, accept: &str) -> CaduceusResult<Response> {
        let key = cache_key(url.as_str(), accept);
        let cached = self.cache.get(&key);
        let mut current = url.clone();
        let mut last_status: Option<u16> = None;
        let mut last_final_url: Option<String> = None;
        let mut last_body: Option<Vec<u8>> = None;

        for hop in 0..=MAX_REDIRECTS {
            let mut headers = HeaderMap::new();
            headers.insert(
                USER_AGENT,
                header_value(&format!(
                    "{USER_AGENT_PREFIX}/{}",
                    env!("CARGO_PKG_VERSION")
                )),
            );
            headers.insert(ACCEPT, header_value(accept));
            headers.insert(
                HeaderName::from_static("x-github-api-version"),
                header_value(GITHUB_API_VERSION_VALUE),
            );
            if hop == 0
                && current.scheme() == self.base_url.scheme()
                && current.host_str() == self.base_url.host_str()
                && current.port_or_known_default() == self.base_url.port_or_known_default()
            {
                if let Some(token) = &self.token {
                    headers.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));
                }
            }
            if hop == 0 {
                if let Some(entry) = &cached {
                    // The ETag value contains double-quotes which
                    // are rejected by `HeaderValue::from_str`. Use
                    // the bytes-based constructor instead so the
                    // exact bytes the server returned are sent
                    // back verbatim on the next request.
                    let value = HeaderValue::from_bytes(entry.etag.as_bytes())
                        .unwrap_or_else(|_| HeaderValue::from_static(""));
                    if !value.is_empty() {
                        headers.insert(reqwest::header::IF_NONE_MATCH, value);
                    }
                }
            }
            let response = self
                .inner
                .get(current.as_str())
                .headers(headers)
                .send()
                .await?;
            let status = response.status().as_u16();
            last_status = Some(status);
            last_final_url = Some(current.to_string());

            // Capture headers and stream body in parallel; the
            // body reader takes ownership of the response, so the
            // ETag/Location lookups must finish first.
            let headers_snapshot = response.headers().clone();
            let body = match read_bounded_body(response).await {
                Ok(bytes) => bytes,
                Err(CaduceusError::Other(msg)) if msg == BODY_TOO_LARGE_SENTINEL => {
                    return Err(CaduceusError::Other(format!(
                        "response body exceeds {} bytes",
                        MAX_BODY_BYTES
                    )));
                }
                Err(err) => return Err(err),
            };
            last_body = Some(body.clone());

            match status {
                200 | 201 => {
                    let etag = headers_snapshot
                        .get(reqwest::header::ETAG)
                        .and_then(|h| h.to_str().ok())
                        .map(str::to_string)
                        .unwrap_or_default();
                    if is_valid_etag(&etag) {
                        let entry = CacheEntry {
                            etag,
                            status,
                            body: body.clone(),
                            final_url: current.to_string(),
                        };
                        self.cache.put(key.clone(), entry)?;
                    }
                    return Ok(Response {
                        status,
                        final_url: current.to_string(),
                        body,
                        headers: headers_snapshot,
                        from_cache: false,
                    });
                }
                304 => {
                    if let Some(entry) = cached.clone() {
                        return Ok(Response {
                            status: 304,
                            final_url: entry.final_url.clone(),
                            body: entry.body.clone(),
                            headers: headers_snapshot,
                            from_cache: true,
                        });
                    }
                    // No cached body: treat as an error. The
                    // contract says the cached body is reused on
                    // 304; absent that, there's nothing to return.
                    return Err(CaduceusError::GitHubApi {
                        status: 304,
                        message: "304 Not Modified with no cached body".to_string(),
                    });
                }
                301 | 302 | 303 | 307 | 308 => {
                    if hop == MAX_REDIRECTS {
                        return Err(CaduceusError::Other(format!(
                            "too many redirects (limit {MAX_REDIRECTS})"
                        )));
                    }
                    let location = headers_snapshot
                        .get(reqwest::header::LOCATION)
                        .and_then(|h| h.to_str().ok())
                        .ok_or_else(|| CaduceusError::GitHubApi {
                            status,
                            message: "redirect with no Location header".to_string(),
                        })?;
                    let next = current.join(location).map_err(|err| {
                        CaduceusError::Other(format!("bad redirect target {location}: {err}"))
                    })?;
                    if !same_origin(&self.base_url, &next) {
                        return Err(CaduceusError::Other(format!(
                            "cross-origin redirect refused: {} -> {}",
                            current, next
                        )));
                    }
                    current = next;
                    continue;
                }
                _ => {
                    // 429 is rate-limit exhaustion even when the
                    // X-RateLimit-Remaining header is missing or
                    // non-zero. Surface a typed RateLimited error
                    // so the daemon's tick wrapper can record the
                    // observation via CadenceGate.
                    if status == 429 {
                        let now_unix = chrono::Utc::now().timestamp();
                        let reset_unix = headers_snapshot
                            .get("x-ratelimit-reset")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(now_unix + 60);
                        let reset_at = (reset_unix - now_unix).max(0) as u64;
                        return Err(CaduceusError::RateLimited {
                            reset_at,
                            remaining: 0,
                            limit: None,
                        });
                    }
                    let text = String::from_utf8_lossy(&body).into_owned();
                    return Err(map_status(status, text));
                }
            }
        }

        // Defensive: should be unreachable because the loop
        // returns or continues on every iteration. Surface the
        // last observation as an error if we ever get here.
        let status = last_status.unwrap_or(0);
        let final_url = last_final_url.unwrap_or_else(|| url.to_string());
        let body = last_body.unwrap_or_default();
        let text = String::from_utf8_lossy(&body).into_owned();
        let _ = final_url;
        Err(map_status(status, text))
    }

    /// POST a JSON body to *url_path* under the configured
    /// `api_base`. The response is returned verbatim — no
    /// caching (writes are not idempotent from the
    /// server's perspective) and no automatic redirect
    /// walk (a 201 must be the *original* response). The
    /// function applies the canonical `User-Agent`,
    /// `Accept`, `X-GitHub-Api-Version`, and
    /// `Authorization: Bearer …` headers; the body is sent
    /// as `application/json`.
    pub async fn post(
        &self,
        url_path: &str,
        accept: &str,
        body: &[u8],
    ) -> CaduceusResult<Response> {
        let (path_only, query) = split_query(url_path);
        let mut url = self.base_url.clone();
        join_path(&mut url, path_only)?;
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        self.post_url(&url, accept, body).await
    }

    /// Same as [`Client::post`] but with a fully-qualified URL.
    /// Used when the orchestrator already has the post URL
    /// in hand (e.g. from a Link header).
    pub async fn post_url(&self, url: &Url, accept: &str, body: &[u8]) -> CaduceusResult<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            header_value(&format!(
                "{USER_AGENT_PREFIX}/{}",
                env!("CARGO_PKG_VERSION")
            )),
        );
        headers.insert(ACCEPT, header_value(accept));
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            header_value(GITHUB_API_VERSION_VALUE),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            header_value("application/json"),
        );
        if url.scheme() == self.base_url.scheme()
            && url.host_str() == self.base_url.host_str()
            && url.port_or_known_default() == self.base_url.port_or_known_default()
        {
            if let Some(token) = &self.token {
                headers.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));
            }
        }
        let response = self
            .inner
            .post(url.as_str())
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await?;
        let status = response.status().as_u16();
        let headers_snapshot = response.headers().clone();
        let body_bytes = match read_bounded_body(response).await {
            Ok(b) => b,
            Err(CaduceusError::Other(msg)) if msg == BODY_TOO_LARGE_SENTINEL => {
                return Err(CaduceusError::Other(format!(
                    "response body exceeds {} bytes",
                    MAX_BODY_BYTES
                )));
            }
            Err(err) => return Err(err),
        };
        if (200..300).contains(&status) || status == 304 {
            return Ok(Response {
                status,
                final_url: url.to_string(),
                body: body_bytes,
                headers: headers_snapshot,
                from_cache: false,
            });
        }
        let text = String::from_utf8_lossy(&body_bytes).into_owned();
        Err(map_status(status, text))
    }

    /// PATCH a JSON body to *url_path* under the configured
    /// `api_base`. Follows the same pattern as [`Client::post`]
    /// but uses the HTTP PATCH method. Returns the response
    /// verbatim on 2xx or 304.
    pub async fn patch(
        &self,
        url_path: &str,
        accept: &str,
        body: &[u8],
    ) -> CaduceusResult<Response> {
        let (path_only, query) = split_query(url_path);
        let mut url = self.base_url.clone();
        join_path(&mut url, path_only)?;
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        self.patch_url(&url, accept, body).await
    }

    /// Same as [`Client::patch`] but with a fully-qualified URL.
    pub async fn patch_url(
        &self,
        url: &Url,
        accept: &str,
        body: &[u8],
    ) -> CaduceusResult<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            header_value(&format!(
                "{USER_AGENT_PREFIX}/{}",
                env!("CARGO_PKG_VERSION")
            )),
        );
        headers.insert(ACCEPT, header_value(accept));
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            header_value(GITHUB_API_VERSION_VALUE),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            header_value("application/json"),
        );
        if url.scheme() == self.base_url.scheme()
            && url.host_str() == self.base_url.host_str()
            && url.port_or_known_default() == self.base_url.port_or_known_default()
        {
            if let Some(token) = &self.token {
                headers.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));
            }
        }
        let response = self
            .inner
            .patch(url.as_str())
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await?;
        let status = response.status().as_u16();
        let headers_snapshot = response.headers().clone();
        let body_bytes = match read_bounded_body(response).await {
            Ok(b) => b,
            Err(CaduceusError::Other(msg)) if msg == BODY_TOO_LARGE_SENTINEL => {
                return Err(CaduceusError::Other(format!(
                    "response body exceeds {} bytes",
                    MAX_BODY_BYTES
                )));
            }
            Err(err) => return Err(err),
        };
        if (200..300).contains(&status) || status == 304 {
            return Ok(Response {
                status,
                final_url: url.to_string(),
                body: body_bytes,
                headers: headers_snapshot,
                from_cache: false,
            });
        }
        let text = String::from_utf8_lossy(&body_bytes).into_owned();
        Err(map_status(status, text))
    }

    /// DELETE the resource at *url_path* under the configured
    /// `api_base`. Follows the same pattern as [`Client::post`]
    /// but uses the HTTP DELETE method. Returns the response
    /// verbatim on 2xx or 304 (204 No Content is a valid success).
    pub async fn delete(&self, url_path: &str, accept: &str) -> CaduceusResult<Response> {
        let (path_only, query) = split_query(url_path);
        let mut url = self.base_url.clone();
        join_path(&mut url, path_only)?;
        if !query.is_empty() {
            url.set_query(Some(query));
        }
        self.delete_url(&url, accept).await
    }

    /// Same as [`Client::delete`] but with a fully-qualified URL.
    pub async fn delete_url(&self, url: &Url, accept: &str) -> CaduceusResult<Response> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            header_value(&format!(
                "{USER_AGENT_PREFIX}/{}",
                env!("CARGO_PKG_VERSION")
            )),
        );
        headers.insert(ACCEPT, header_value(accept));
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            header_value(GITHUB_API_VERSION_VALUE),
        );
        if url.scheme() == self.base_url.scheme()
            && url.host_str() == self.base_url.host_str()
            && url.port_or_known_default() == self.base_url.port_or_known_default()
        {
            if let Some(token) = &self.token {
                headers.insert(AUTHORIZATION, header_value(&format!("Bearer {token}")));
            }
        }
        let response = self
            .inner
            .delete(url.as_str())
            .headers(headers)
            .send()
            .await?;
        let status = response.status().as_u16();
        let headers_snapshot = response.headers().clone();
        let body_bytes = match read_bounded_body(response).await {
            Ok(b) => b,
            Err(CaduceusError::Other(msg)) if msg == BODY_TOO_LARGE_SENTINEL => {
                return Err(CaduceusError::Other(format!(
                    "response body exceeds {} bytes",
                    MAX_BODY_BYTES
                )));
            }
            Err(err) => return Err(err),
        };
        if (200..300).contains(&status) || status == 304 {
            return Ok(Response {
                status,
                final_url: url.to_string(),
                body: body_bytes,
                headers: headers_snapshot,
                from_cache: false,
            });
        }
        let text = String::from_utf8_lossy(&body_bytes).into_owned();
        Err(map_status(status, text))
    }

    /// Refuse to enable auto-merge on a pull request. This is the
    /// runtime defence for AC-04 ("Never auto-merge"). Every code
    /// path that would call the GitHub merge API must route through
    /// this method first — it always returns an error.
    ///
    /// The returned error carries the contract message from
    /// CONTRACTS.md FINAL-001 AC-04.
    pub fn enable_auto_merge(&self) -> CaduceusResult<()> {
        crate::runtime::audit::refuse_auto_merge()
    }
}
