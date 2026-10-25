//! Combined commit status + check-run rollup for Auto Review's CI
//! gate (issue #435).
//!
//! The combined-status endpoint is legacy statuses only; GitHub
//! Actions reports check runs. The gate therefore reads both against
//! the watched **base** repo and rolls them up with precedence
//! failure > pending > success > absent.
//!
//! Pure [`rollup`] takes already-decoded payloads so the truth table
//! can be unit-tested without HTTP. [`fetch_ci_rollup`] owns the
//! GETs, the legacy-failure short-circuit, pagination, and
//! rate-limit / auth error mapping.

use serde::Deserialize;
use url::Url;

use crate::github::link_header::next_url_from_link_header;
use crate::github::poll::MAX_PAGES_PER_ENDPOINT;
use crate::github::{rate_limit_from_headers, Client, RateLimitInfo, Response, ACCEPT_VALUE};
use crate::infra::error::CaduceusError;

/// Check runs fetched per page. GitHub's documented maximum.
pub const CHECK_RUNS_PER_PAGE: u32 = 100;

/// Verdict of the CI gate for one head SHA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CiRollup {
    /// No statuses and no check runs reported.
    Absent,
    /// Something is still running, waiting, or needs maintainer
    /// approval (`action_required`). Re-evaluate next poll.
    Pending,
    /// All reported contexts finished successfully (or
    /// `neutral`/`skipped`).
    Success,
    /// At least one context failed.
    Failure,
}

/// Combined-status wire payload (`GET …/commits/{ref}/status`).
/// All-optional: GitHub adds fields benignly.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CombinedStatus {
    pub state: Option<String>,
    pub total_count: Option<u64>,
}

/// One check-run row from `GET …/commits/{ref}/check-runs`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CheckRun {
    pub status: Option<String>,
    pub conclusion: Option<String>,
}

/// Check-runs list envelope.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CheckRunsPage {
    pub total_count: Option<u64>,
    #[serde(default)]
    pub check_runs: Vec<CheckRun>,
}

/// True when `sha` is a single hex path segment of 1..=64 characters
/// (GitHub still uses SHA-1; SHA-256 commits are 64 hex).
pub fn is_safe_commit_sha(sha: &str) -> bool {
    let len = sha.len();
    (1..=64).contains(&len) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Pure rollup of already-fetched, well-formed payloads.
///
/// Callers that decode from the wire must reject unrecognized
/// conclusions before invoking this (see [`fetch_ci_rollup`]).
/// Unrecognized inputs are treated as Pending so a mis-wired caller
/// still does not admit.
pub fn rollup(combined: &CombinedStatus, checks: &CheckRunsPage) -> CiRollup {
    let status_count = combined.total_count.unwrap_or(0);
    let status_state = combined.state.as_deref().unwrap_or("");
    let check_count = checks.total_count.unwrap_or(0);

    let mut failure = false;
    let mut pending = false;
    let mut successish = false;

    if status_count > 0 {
        match status_state {
            "failure" | "error" => failure = true,
            "pending" => pending = true,
            "success" => successish = true,
            // Unrecognized → do not admit.
            _ => pending = true,
        }
    }

    for run in &checks.check_runs {
        match classify_check_run(run) {
            CheckVerdict::Failure => failure = true,
            CheckVerdict::Pending => pending = true,
            CheckVerdict::Successish => successish = true,
            // Fail-closed default for the pure function; the fetch
            // layer surfaces unrecognized as Retryable and never
            // reaches rollup with them.
            CheckVerdict::Unrecognized => pending = true,
        }
    }
    if checks.check_runs.is_empty() && check_count > 0 {
        // Envelope claims checks we did not see — do not admit.
        pending = true;
    }

    if failure {
        return CiRollup::Failure;
    }
    if pending {
        return CiRollup::Pending;
    }
    if successish {
        return CiRollup::Success;
    }
    if status_count == 0 && check_count == 0 {
        return CiRollup::Absent;
    }
    CiRollup::Success
}

/// Single check-run classifier shared by the pure rollup and the
/// fetch-layer unrecognized gate (issue #435 review).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckVerdict {
    Failure,
    Pending,
    Successish,
    Unrecognized,
}

fn classify_check_run(run: &CheckRun) -> CheckVerdict {
    let status = run.status.as_deref().unwrap_or("");
    if status != "completed" {
        // queued / in_progress / waiting / requested / pending / …
        return CheckVerdict::Pending;
    }
    match run.conclusion.as_deref() {
        Some("failure" | "timed_out" | "cancelled" | "startup_failure" | "stale") => {
            CheckVerdict::Failure
        }
        Some("action_required") => CheckVerdict::Pending,
        Some("success" | "neutral" | "skipped") => CheckVerdict::Successish,
        Some(_) | None => CheckVerdict::Unrecognized,
    }
}

/// Outcome of a CI lookup that should not admit and should not
/// record a durable failure — retry next poll.
#[derive(Debug)]
pub enum CiLookupError {
    /// Per-row: 404, 5xx, truncated pages, unreadable conclusion.
    Retryable(String),
    /// Step-level: rate limit.
    RateLimited(CaduceusError),
    /// Step-level: 401 / 403 (token scope or secondary rate limit).
    Auth(CaduceusError),
}

/// Fetch and roll up CI for `head_sha` on `owner/repo`.
///
/// Returns [`Ok`] with the rollup, or [`Err`] classifying
/// retryable vs step-level failures. Never admits on error.
pub async fn fetch_ci_rollup(
    client: &Client,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Result<CiRollup, CiLookupError> {
    if !is_safe_commit_sha(head_sha) {
        return Err(CiLookupError::Retryable(format!(
            "CI gate: head SHA is not a safe hex path segment ({head_sha})"
        )));
    }

    let combined = fetch_combined_status(client, owner, repo, head_sha).await?;
    let status_count = combined.total_count.unwrap_or(0);
    let status_state = combined.state.as_deref().unwrap_or("");
    if status_count > 0 && matches!(status_state, "failure" | "error") {
        // Failure wins; skip the check-runs GET.
        return Ok(CiRollup::Failure);
    }
    if status_count > 0 && !matches!(status_state, "pending" | "success" | "failure" | "error") {
        return Err(CiLookupError::Retryable(format!(
            "CI gate: unrecognized combined status state '{status_state}'"
        )));
    }

    let checks = fetch_check_runs(client, owner, repo, head_sha).await?;
    // Single classifier pass: unrecognized completed conclusions
    // become Retryable and never reach rollup (so they are not
    // durable-skipped as pending/failure).
    for run in &checks.check_runs {
        if classify_check_run(run) == CheckVerdict::Unrecognized {
            let detail = match run.conclusion.as_deref() {
                Some(other) => format!("unrecognized check-run conclusion '{other}'"),
                None => "completed check-run with null conclusion".to_string(),
            };
            return Err(CiLookupError::Retryable(format!("CI gate: {detail}")));
        }
    }

    Ok(rollup(&combined, &checks))
}

async fn fetch_combined_status(
    client: &Client,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Result<CombinedStatus, CiLookupError> {
    let path = format!("/repos/{owner}/{repo}/commits/{head_sha}/status");
    let response = get_ci(client, &path).await?;
    serde_json::from_slice(&response.body).map_err(|err| {
        CiLookupError::Retryable(format!(
            "CI gate: combined status JSON parse for {owner}/{repo}@{head_sha}: {err}"
        ))
    })
}

async fn fetch_check_runs(
    client: &Client,
    owner: &str,
    repo: &str,
    head_sha: &str,
) -> Result<CheckRunsPage, CiLookupError> {
    let initial_path = format!(
        "/repos/{owner}/{repo}/commits/{head_sha}/check-runs?filter=latest&per_page={CHECK_RUNS_PER_PAGE}"
    );
    let mut next: Option<Url> = Some(client.base_url().clone().join(&initial_path).map_err(
        |err| {
            CiLookupError::Retryable(format!(
                "CI gate: cannot join {initial_path} onto api_base: {err}"
            ))
        },
    )?);
    let mut collected = CheckRunsPage::default();
    let mut pages = 0usize;
    while let Some(url) = next.take() {
        pages += 1;
        let response = get_ci_url(client, &url).await?;
        let page: CheckRunsPage = serde_json::from_slice(&response.body).map_err(|err| {
            CiLookupError::Retryable(format!(
                "CI gate: check-runs JSON parse for {owner}/{repo}@{head_sha} page {pages}: {err}"
            ))
        })?;
        if pages == 1 {
            collected.total_count = page.total_count;
        }
        collected.check_runs.extend(page.check_runs);
        next = parse_next_link(&response);
        if next.is_some() && pages >= MAX_PAGES_PER_ENDPOINT {
            return Err(CiLookupError::Retryable(format!(
                "CI gate: check-runs for {owner}/{repo}@{head_sha} truncated at page cap"
            )));
        }
    }
    // Incomplete collection (missing Link, short page, empty array
    // while total_count > 0): do not admit on a partial view.
    let total = collected.total_count.unwrap_or(0) as usize;
    if total > collected.check_runs.len() {
        return Err(CiLookupError::Retryable(format!(
            "CI gate: check-runs for {owner}/{repo}@{head_sha} incomplete \
             (total_count {total} > collected {})",
            collected.check_runs.len()
        )));
    }
    Ok(collected)
}

async fn get_ci(client: &Client, path: &str) -> Result<Response, CiLookupError> {
    match client.get(path, ACCEPT_VALUE).await {
        Ok(response) => observe_rate_limit(&response).map(|()| response),
        Err(err) => map_transport_err(err),
    }
}

async fn get_ci_url(client: &Client, url: &Url) -> Result<Response, CiLookupError> {
    match client.get_url(url, ACCEPT_VALUE).await {
        Ok(response) => observe_rate_limit(&response).map(|()| response),
        Err(err) => map_transport_err(err),
    }
}

fn observe_rate_limit(response: &Response) -> Result<(), CiLookupError> {
    if let Some(observation) = rate_limit_from_headers(&response.headers, response.status) {
        if observation.remaining == 0 {
            return Err(CiLookupError::RateLimited(rate_limited(observation)));
        }
    }
    // 200/304 are success; anything else that arrived as Ok is unexpected.
    if !matches!(response.status, 200 | 304) {
        return Err(map_http_status(response.status, &response.body));
    }
    Ok(())
}

fn map_transport_err(err: CaduceusError) -> Result<Response, CiLookupError> {
    match err {
        CaduceusError::RateLimited { .. } => Err(CiLookupError::RateLimited(err)),
        CaduceusError::GitHubApi {
            status: 401 | 403, ..
        } => Err(CiLookupError::Auth(err)),
        CaduceusError::GitHubApi { status, message } => Err(CiLookupError::Retryable(format!(
            "CI gate: GitHub API {status}: {message}"
        ))),
        other => Err(CiLookupError::Retryable(format!("CI gate: {other}"))),
    }
}

fn map_http_status(status: u16, body: &[u8]) -> CiLookupError {
    let message = String::from_utf8_lossy(body).into_owned();
    match status {
        401 | 403 => CiLookupError::Auth(CaduceusError::GitHubApi { status, message }),
        429 => CiLookupError::RateLimited(CaduceusError::RateLimited {
            reset_at: 60,
            remaining: 0,
            limit: None,
        }),
        _ => CiLookupError::Retryable(format!("CI gate: HTTP {status}: {message}")),
    }
}

fn parse_next_link(response: &Response) -> Option<Url> {
    use reqwest::header::LINK;
    let header = response.headers.get(LINK)?.to_str().ok()?;
    next_url_from_link_header(header).and_then(|raw| Url::parse(&raw).ok())
}

fn rate_limited(observation: RateLimitInfo) -> CaduceusError {
    let now = chrono::Utc::now().timestamp();
    let reset_at = (observation.reset_at_unix.saturating_sub(now)).max(0) as u64;
    CaduceusError::RateLimited {
        reset_at,
        remaining: observation.remaining,
        limit: observation.limit,
    }
}
