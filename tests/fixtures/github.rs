//! Reusable Wiremock-backed GitHub API fixture.
//!
//! This is the v1.0 Phase 1.2 helper. Each `MockGitHub` owns a
//! running `MockServer` and exposes the small surface most tests
//! need without hand-rolling `Mock::given(...).respond_with(...)`
//! boilerplate.
//!
//! The fixture is hermetic: it binds to a localhost ephemeral port,
//! requires no production credentials, and never touches
//! `api.github.com`. Tests that need to assert what the daemon sent
//! can call `received_requests()` to walk the in-memory request log
//! or `counts()` to read a per-method tally.
//!
//! Three top-level entry points cover the common cases:
//!
//! * [`MockGitHub::mount`] — single-response matcher
//! * [`MockGitHub::mount_paged`] — multi-page list response
//! * [`MockGitHub::mount_etag`] — 200 + ETag header (so the
//!   second poll can exercise the cache 304 path)
//!
//! Anything more elaborate drops down to [`MockGitHub::mount_with`]
//! which forwards a caller-supplied `wiremock::Mock` so the
//! full matcher/response language stays available without losing
//! the `received_requests` and `counts` ergonomics.
//!
//! Each helper test binary builds in isolation, so individual
//! methods (e.g. `mount_paged`) only appear "used" in the
//! binary that imports them. The fixture is shared, not
//! binary-local, so the file-level `dead_code` allow is the
//! accurate semantic: every public item on `MockGitHub` and
//! `Counts` has a real downstream user.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Per-method tally of requests the mock has served since
/// `start()`. The tally is updated by a single shared responder
/// so it survives multiple `mount` calls and stays consistent
/// with `received_requests()`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub get: usize,
    pub post: usize,
    pub patch: usize,
    pub put: usize,
    pub delete: usize,
    pub other: usize,
}

impl Counts {
    /// Total requests across every HTTP method.
    pub fn total(&self) -> usize {
        self.get + self.post + self.patch + self.put + self.delete + self.other
    }

    /// Mutation count: every method that mutates server state
    /// (POST, PATCH, PUT, DELETE) plus any non-GET that the
    /// caller has not explicitly bucketed. Used by Task 1.2
    /// AC-03 ("record exact GitHub mutation counts").
    pub fn mutations(&self) -> usize {
        self.post + self.patch + self.put + self.delete
    }
}

/// Wraps a Wiremock `MockServer` with the helpers the rest of
/// the v1.0 plan reaches for. `MockGitHub::start().await` is the
/// only constructor — every other method assumes the server is
/// already running.
///
/// `MockServer` is not `Clone`, so the fixture stores it behind
/// an `Arc` and derives `Clone` on `MockGitHub` itself. Sharing a
/// single mock across threads is cheap and the per-method counts
/// stay consistent because the tally is in its own `Arc<Mutex<_>>`.
#[derive(Clone)]
pub struct MockGitHub {
    inner: Arc<MockServer>,
    counts: Arc<std::sync::Mutex<Counts>>,
    log: Arc<std::sync::Mutex<Vec<Request>>>,
}

impl MockGitHub {
    /// Start a fresh mock server on an ephemeral localhost port.
    /// Returns once the server is accepting connections.
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Self {
            inner: Arc::new(server),
            counts: Arc::new(std::sync::Mutex::new(Counts::default())),
            log: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// The base URI the daemon should use as its `api_base`.
    /// Always `http://127.0.0.1:<port>` — never a production
    /// host.
    pub fn uri(&self) -> String {
        self.inner.uri()
    }

    /// Raw `MockServer` handle for tests that need to drop
    /// down to the wiremock API directly (e.g. low-level
    /// `expect(0)` assertions or request-body matchers).
    pub fn server(&self) -> &MockServer {
        &self.inner
    }

    /// Mount a single-response matcher. `method` is one of
    /// `"GET"`, `"POST"`, `"PATCH"`, `"PUT"`, `"DELETE"`.
    /// `path_pattern` is forwarded to wiremock's `PathMatcher`,
    /// so callers may pass either a literal `"/repos/o/r/issues"`
    /// or a regex.
    ///
    /// `body` is serialised as JSON. `status` defaults to 200.
    pub async fn mount<B>(&self, method: &str, path_pattern: &str, body: B)
    where
        B: Serialize,
    {
        self.mount_status(method, path_pattern, 200, body).await;
    }

    /// Same as [`mount`] but lets the caller pin the response
    /// status code. Use for 201 (create), 204 (no-content),
    /// 422 (validation), 429 (rate-limited), 5xx (failure).
    pub async fn mount_status<B>(&self, method: &str, path_pattern: &str, status: u16, body: B)
    where
        B: Serialize,
    {
        let counted = CountingResponder {
            counts: Arc::clone(&self.counts),
            log: Arc::clone(&self.log),
            method_label: method.to_string(),
            inner: ResponseTemplate::new(status).set_body_json(body),
        };
        Mock::given(wiremock::matchers::method(method))
            .and(wiremock::matchers::path(path_pattern))
            .respond_with(counted)
            .mount(&self.inner)
            .await;
    }

    /// Mount a multi-page list endpoint. The daemon polls page 1
    /// by default, sees a `Link: <uri>; rel="next"` header, and
    /// follows until the link is absent. `pages` is the list of
    /// page bodies in order (page 1 first).
    ///
    /// The `Link` header is constructed by `MockGitHub` using the
    /// mock's own URI so the daemon never resolves a host other
    /// than `127.0.0.1:<port>`. The first page is matched with
    /// `query_param_is_missing("page")` so callers don't have to
    /// think about the no-query-string case explicitly.
    pub async fn mount_paged<B>(&self, path_pattern: &str, pages: Vec<B>)
    where
        B: Serialize,
    {
        let total = pages.len();
        let prefix = path_pattern.trim_start_matches('/').to_string();
        for (idx, page) in pages.into_iter().enumerate() {
            let page_num = idx + 1;
            let is_last = page_num == total;
            let next_link = if is_last {
                None
            } else {
                Some(format!(
                    "<{}/{}?page={}>; rel=\"next\"",
                    self.uri(),
                    prefix,
                    page_num + 1
                ))
            };
            let mut template = ResponseTemplate::new(200).set_body_json(page);
            if let Some(link) = next_link {
                template = template.append_header("Link", link);
            }
            let counted = CountingResponder {
                counts: Arc::clone(&self.counts),
                log: Arc::clone(&self.log),
                method_label: "GET".to_string(),
                inner: template,
            };
            if page_num == 1 {
                // First page: caller passes no `page` query param.
                Mock::given(self::method("GET"))
                    .and(self::path(path_pattern))
                    .and(wiremock::matchers::query_param_is_missing("page"))
                    .respond_with(counted)
                    .mount(&self.inner)
                    .await;
            } else {
                Mock::given(self::method("GET"))
                    .and(self::path(path_pattern))
                    .and(wiremock::matchers::query_param(
                        "page",
                        page_num.to_string().as_str(),
                    ))
                    .respond_with(counted)
                    .mount(&self.inner)
                    .await;
            }
        }
    }

    /// Mount a 200 response that carries an `ETag` header. Use
    /// this when a test wants to exercise the cache layer: the
    /// daemon stores the ETag on the first call and replays the
    /// cached body on the second call when the server replies
    /// 304. `etag_value` should be quoted (e.g. `"\"v1\""`).
    pub async fn mount_etag<B>(&self, path_pattern: &str, etag_value: &str, body: B)
    where
        B: Serialize,
    {
        let counted = CountingResponder {
            counts: Arc::clone(&self.counts),
            log: Arc::clone(&self.log),
            method_label: "GET".to_string(),
            inner: ResponseTemplate::new(200)
                .append_header("ETag", etag_value)
                .set_body_json(body),
        };
        Mock::given(self::method("GET"))
            .and(self::path(path_pattern))
            .respond_with(counted)
            .mount(&self.inner)
            .await;
    }

    /// Mount the 304 reply a server returns when the client's
    /// `If-None-Match` matches the current ETag. The body is
    /// empty by definition.
    pub async fn mount_not_modified(&self, path_pattern: &str) {
        let counted = CountingResponder {
            counts: Arc::clone(&self.counts),
            log: Arc::clone(&self.log),
            method_label: "GET".to_string(),
            inner: ResponseTemplate::new(304),
        };
        Mock::given(self::method("GET"))
            .and(self::path(path_pattern))
            .respond_with(counted)
            .mount(&self.inner)
            .await;
    }

    /// Escape hatch: build a fully custom `Mock` and mount it
    /// directly. Useful when a test needs request-body matchers,
    /// conditional state, or a non-standard response shape that
    /// the convenience methods don't cover.
    ///
    /// Caveat: mocks registered through `mount_with` do **not**
    /// contribute to [`Self::counts`] or
    /// [`Self::received_requests`] — wiremock owns the response
    /// lifecycle once the `Mock` is mounted. Use [`Self::server`]
    /// to call `received_requests()` on the underlying mock
    /// server if you need request inspection, and assume any
    /// count assertion will be off-by-N where N is the number
    /// of requests this method's mock served. The convenience
    /// methods above are the right choice whenever they fit.
    pub async fn mount_with<F>(&self, build: F)
    where
        F: FnOnce(&MockServer) -> Mock,
    {
        let mock = build(&self.inner);
        mock.mount(&self.inner).await;
    }

    /// Snapshot of the per-method request tally.
    pub fn counts(&self) -> Counts {
        self.counts.lock().expect("counts lock").clone()
    }

    /// Full request log (cloned). Order is the order the
    /// server received the requests. Used by tests that need to
    /// inspect headers, bodies, or query strings.
    pub fn received_requests(&self) -> Vec<Request> {
        self.log.lock().expect("log lock").clone()
    }

    /// Per-path tally of requests. Convenience for tests that
    /// assert "the daemon POSTed exactly once to
    /// `/repos/o/r/issues/1/comments`". Returned map is keyed
    /// by the request's path; values are the request count.
    pub fn path_counts(&self) -> HashMap<String, usize> {
        let log = self.log.lock().expect("log lock");
        let mut out: HashMap<String, usize> = HashMap::new();
        for req in log.iter() {
            *out.entry(req.url.path().to_string()).or_insert(0) += 1;
        }
        out
    }
}

/// Responder wrapper that updates the shared `Counts` tally and
/// appends the request to the shared log before delegating to
/// the inner response. Lives once per mount and is `Clone` so
/// wiremock can store it on either side of a match.
struct CountingResponder {
    counts: Arc<std::sync::Mutex<Counts>>,
    log: Arc<std::sync::Mutex<Vec<Request>>>,
    method_label: String,
    inner: ResponseTemplate,
}

impl Respond for CountingResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        {
            let mut counts = self.counts.lock().expect("counts lock");
            match self.method_label.as_str() {
                "GET" => counts.get += 1,
                "POST" => counts.post += 1,
                "PATCH" => counts.patch += 1,
                "PUT" => counts.put += 1,
                "DELETE" => counts.delete += 1,
                _ => counts.other += 1,
            }
        }
        self.log.lock().expect("log lock").push(request.clone());
        self.inner.clone()
    }
}

// (wiremock's `method` and `path` matchers are used directly via
// `wiremock::matchers::method` / `path` inside the mount helpers
// above, so no local helpers are needed here.)
