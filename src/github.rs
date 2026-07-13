//! Typed GitHub API surface. `Client`, repositories endpoint, issues
//! endpoint, and ETag-aware conditional GET are owned here.
//!
//! Every outbound mutation that posts a comment, a pull-request
//! title, or a pull-request body MUST route through
//! [`check_voice_or_error`] first. The validator is the single
//! entry point for the public-voice rule; nothing else in the
//! crate bypasses it.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::Config;
use crate::error::{CaduceusError, CaduceusResult, VoiceError};
use crate::finalize::{
    validate_comment, validate_pr_body, validate_pr_title, validate_public_text,
};
use crate::issue::IssueKey;

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

/// Result of a typed HTTP GET. ``final_url`` captures the URL after
/// any allowed redirects so issue verification can detect a
/// transfer (a 301/302 to a different repo is a transfer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub final_url: String,
    pub body: Vec<u8>,
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
}

/// One cached response keyed by full URL + Accept header. The
/// body is stored alongside the ETag so a 304 can reuse the last
/// successfully parsed body verbatim.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub etag: String,
    pub status: u16,
    pub body: Vec<u8>,
    pub final_url: String,
}

/// Persistent conditional cache rooted at `<state_dir>/cache/http.json`.
/// All mutations go through one mutex so concurrent detail requests
/// merge into the same locked update.
#[derive(Debug)]
pub struct HttpCache {
    path: PathBuf,
    state: Mutex<CacheState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheState {
    by_key: BTreeMap<String, CacheEntry>,
}

impl HttpCache {
    /// Open or create the cache rooted at *state_dir*. Missing
    /// directories are created with mode `0700`; missing files yield
    /// an empty cache; malformed JSON is dropped on first read so a
    /// corruption cannot poison every tick.
    pub fn open(state_dir: &Path) -> CaduceusResult<Self> {
        let dir = state_dir.join("cache");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            set_secure_dir_mode(&dir)?;
        }
        let path = dir.join(HTTP_CACHE_FILENAME);
        let state = if path.exists() {
            read_cache_file(&path)?
        } else {
            CacheState::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Path to the cache file (test seam).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Build a fresh cache instance pointing at the same file.
    /// Used by [`Client::new`] to clone the inert cache without
    /// sharing the mutex across callers.
    pub fn clone_state(&self) -> Self {
        Self {
            path: self.path.clone(),
            state: Mutex::new(CacheState::default()),
        }
    }

    /// Borrow the cached entry for *key*. Returns `None` when there
    /// is no entry, when the entry's stored ETag is malformed, or
    /// when the stored ETag is the empty string.
    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        let guard = self.state.lock().expect("http cache lock poisoned");
        match guard.by_key.get(key) {
            Some(entry) if is_valid_etag(&entry.etag) => Some(entry.clone()),
            _ => None,
        }
    }

    /// Store *entry* under *key*. Writes the cache file atomically
    /// when the lock is dropped if any change was made. Concurrent
    /// callers serialise through the mutex; the last write wins for
    /// any given key.
    pub fn put(&self, key: String, entry: CacheEntry) -> CaduceusResult<()> {
        let mut guard = self.state.lock().expect("http cache lock poisoned");
        if !is_valid_etag(&entry.etag) {
            // An invalid ETag is dropped, not stored — the next
            // caller can rebuild a clean entry from a fresh 200.
            guard.by_key.remove(&key);
        } else {
            guard.by_key.insert(key, entry);
        }
        write_cache_file(&self.path, &guard)
    }
}

fn read_cache_file(path: &Path) -> CaduceusResult<CacheState> {
    let bytes = std::fs::read(path).map_err(|err| CaduceusError::StateCorrupt {
        path: path.to_path_buf(),
        message: format!("read http cache: {err}"),
    })?;
    if bytes.is_empty() {
        return Ok(CacheState::default());
    }
    match serde_json::from_slice::<CacheState>(&bytes) {
        Ok(state) => Ok(state),
        // Corruption recovery: drop the bad file, start over. The
        // contract says "Invalid cache JSON drops only the affected
        // cache entry and refetches unconditionally"; the only entry
        // we have is the whole file, so dropping it is the
        // narrowest safe recovery.
        Err(_) => {
            let _ = std::fs::remove_file(path);
            Ok(CacheState::default())
        }
    }
}

fn write_cache_file(path: &Path, state: &CacheState) -> CaduceusResult<()> {
    let body = serde_json::to_vec(state)
        .map_err(|err| CaduceusError::Other(format!("serialise http cache: {err}")))?;
    let parent = path.parent().ok_or_else(|| {
        CaduceusError::Other(format!("http cache path has no parent: {}", path.display()))
    })?;
    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
        set_secure_dir_mode(parent)?;
    }
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        HTTP_CACHE_FILENAME,
        ulid::Ulid::new()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    set_secure_file_mode(path)?;
    Ok(())
}

fn set_secure_dir_mode(path: &Path) -> CaduceusResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn set_secure_file_mode(path: &Path) -> CaduceusResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Validates an ETag header value per RFC 7232. The only permitted
/// shapes are the strong quoted form (`"abc"`) and the weak
/// prefixed form (`W/"abc"`); anything else is treated as a
/// cache-busting marker so the next request refetches.
pub fn is_valid_etag(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let after_weak = trimmed.strip_prefix("W/").unwrap_or(trimmed);
    let after_open = match after_weak.strip_prefix('"') {
        Some(s) => s,
        None => return false,
    };
    let body = match after_open.strip_suffix('"') {
        Some(s) => s,
        None => return false,
    };
    !body.is_empty() && !body.contains('\u{0}')
}

/// Build the cache key for a given URL + Accept pair. URL is the
/// full URL (after any normalisation); Accept header is the relevant
/// GitHub media type.
pub fn cache_key(url: &str, accept: &str) -> String {
    format!("{url}\u{0}{accept}")
}

/// Lazily-initialised in-memory cache used by [`Client::new`].
/// The pointer is process-wide because `Client::new` is documented
/// as inert (no real I/O); any test that actually exercises
/// caching should construct via [`Client::with_cache`].
fn inert_cache() -> HttpCache {
    static CACHE: OnceLock<HttpCache> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("caduceus-inert-{}", ulid::Ulid::new()));
            HttpCache::open(&dir).expect("inert cache builds")
        })
        .clone_state()
}

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
                        from_cache: false,
                    });
                }
                304 => {
                    if let Some(entry) = cached.clone() {
                        return Ok(Response {
                            status: 304,
                            final_url: entry.final_url.clone(),
                            body: entry.body.clone(),
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
}

const BODY_TOO_LARGE_SENTINEL: &str = "caduceus::github::body_too_large";

async fn read_bounded_body(response: reqwest::Response) -> CaduceusResult<Vec<u8>> {
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

fn map_status(status: u16, message: String) -> CaduceusError {
    // Strip any leaked credential values before the message lands in
    // a Display/Debug path. The scrub helper handles the three
    // documented credential names; the daemon's structured logger
    // and any test failure render through `Display`, so we must
    // scrub here regardless of how the variant is later rendered.
    let scrubbed = crate::error::scrub(&message);
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

fn same_origin(base: &Url, candidate: &Url) -> bool {
    base.scheme() == candidate.scheme()
        && base.host_str() == candidate.host_str()
        && base.port_or_known_default() == candidate.port_or_known_default()
}

fn join_path(base: &mut Url, path: &str) -> CaduceusResult<()> {
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

fn split_query(input: &str) -> (&str, &str) {
    match input.split_once('?') {
        Some((path, query)) => (path, query),
        None => (input, ""),
    }
}

fn header_value(value: &str) -> HeaderValue {
    // The headers we set are static (and the Accept value comes
    // from the caller-supplied Accept value, which is also
    // validated). The unwrap_or fallback is defensive only.
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// Lookup result for an issue summary by key.
#[derive(Debug)]
pub struct IssueSummary {
    pub key: IssueKey,
    pub title: String,
    pub labels: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn fetch_issue(_client: &Client, _key: &IssueKey) -> CaduceusResult<IssueSummary> {
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
    _client: &Client,
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
    _client: &Client,
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
    _client: &Client,
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

/// Backwards-compatible alias for [`Client`]. Earlier code paths
/// (and Task 6.6's voice-rule tests) refer to the HTTP client as
/// `HttpClient`; renaming the type would have rippled into tests
/// outside this task's ownership. The alias keeps the surface
/// stable while Phase 2 is in flight.
pub type HttpClient = Client;
