//! Polling: discover watched repos and merge labeled open issues into the
//! queue. Phase 2 implements the body; the stub defines the typed surface.
//!
//! The repository discovery path is implemented in Task 2.2; the
//! labeled-issue poll is implemented in Task 2.3; the trigger-label
//! verification lives in Task 2.5. The module exposes the public
//! surface owned by Phase 2; per-task additions are layered on top.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use url::Url;

use crate::github::issue::IssueKey;
use crate::github::{rate_limit_from_headers, Client, Response, ACCEPT_VALUE};

// Preserve the historical public surface: tests reach this helper through
// `caduceus::poll::next_url_from_link_header`.
pub use crate::github::link_header::next_url_from_link_header;
use crate::infra::config::{is_valid_repo_slug, Config};
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::queue::TicketType;

/// Hard maximum pages the daemon will follow on a single paginated
/// endpoint. The contract requires an error rather than silent
/// truncation when the limit is exceeded.
pub const MAX_PAGES_PER_ENDPOINT: usize = 20;

/// Number of rows fetched per page on `/repos/{slug}/issues`. The
/// contract pins the GitHub-side maximum so a single page always
/// fits the documented 100-row envelope.
pub const ISSUES_PER_PAGE: u32 = 100;

/// Number of repositories fetched per page on `/user/repos`. The
/// contract pins the GitHub-side maximum so a single page always
/// fits the documented 100-row envelope.
pub const REPOS_PER_PAGE: u32 = 100;

/// One open issue surfaced by polling. The structured shape used by
/// the daemon's queue writer (Task 3.x) and the verification flow
/// (Task 2.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSummary {
    pub key: IssueKey,
    pub title: String,
    pub labels: Vec<String>,
    pub ticket_type: TicketType,
    pub updated_at: DateTime<Utc>,
}

/// Structured diagnostic surfaced alongside the canonical
/// `IssueSummary` list. These do not enqueue work; they describe
/// objects the daemon intentionally skipped so the operator can see
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuePollDiagnostic {
    /// The issue matched both trigger labels. Per
    /// `CONTRACTS.md` "Polling contract" it is not enqueued
    /// until a human removes one.
    Ambiguous {
        key: IssueKey,
        title: String,
        labels: Vec<String>,
    },
    /// The issue returned by the API carried a `pull_request`
    /// object and is therefore excluded.
    PullRequest { key: IssueKey, title: String },
    /// The issue matched neither trigger label when verified
    /// against its own `labels` array.
    Unmatched {
        key: IssueKey,
        title: String,
        labels: Vec<String>,
    },
    /// The issue was unusable (missing required field, malformed
    /// number, etc.).
    Malformed {
        key: Option<IssueKey>,
        reason: String,
    },
}

/// Outcome of one labeled-issue poll across all watched repos.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IssuePollOutcome {
    /// Unique issues matched exactly one of the two trigger
    /// labels, ready for queue admission.
    pub summaries: Vec<IssueSummary>,
    /// Objects the daemon intentionally skipped.
    pub diagnostics: Vec<IssuePollDiagnostic>,
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
    discover_via_api(client, cfg.discovery_max_pages as usize).await
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

async fn discover_via_api(client: &Client, max_pages: usize) -> CaduceusResult<Vec<String>> {
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
        if pages >= max_pages {
            return Err(CaduceusError::Other(format!(
                "repository discovery exceeded {max_pages} pages"
            )));
        }
        pages += 1;
        let response = client.get_url(&url, ACCEPT_VALUE).await?;
        // Rate-limit responses carry `x-ratelimit-remaining: 0` plus
        // a `x-ratelimit-reset` Unix timestamp; surface them as a
        // typed RateLimited error so the meta layer can persist the
        // observation (Task 2.4). Non-zero `remaining` (or a 429
        // response) is observed for cadence but does not short-circuit
        // a successful page.
        if let Some(observation) = rate_limit_from_headers(&response.headers, response.status) {
            if observation.remaining == 0 {
                return Err(rate_limit_error(observation));
            }
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
    use reqwest::header::LINK;
    let header = response.headers.get(LINK)?.to_str().ok()?;
    next_url_from_link_header(header).and_then(|raw| Url::parse(&raw).ok())
}

/// Translate a [`crate::github::RateLimitInfo`] into the
/// typed `CaduceusError::RateLimited` variant the daemon's
/// outer loop recognises. Only a `remaining == 0` observation
/// (or a 429) is treated as exhausted; other responses carry
/// the headers for observation but proceed normally.
fn rate_limit_error(observation: crate::github::RateLimitInfo) -> CaduceusError {
    if observation.remaining != 0 {
        return CaduceusError::Other(format!(
            "rate_limit_error called with remaining={} (not exhausted)",
            observation.remaining
        ));
    }
    let now = chrono::Utc::now().timestamp();
    let reset_at = (observation.reset_at_unix.saturating_sub(now)).max(0) as u64;
    CaduceusError::RateLimited {
        reset_at,
        remaining: observation.remaining,
        limit: observation.limit,
    }
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

// ---------------------------------------------------------------------------
// Task 2.3 — labeled-issue poll
// ---------------------------------------------------------------------------

/// Poll for label `ticket_label_code` across every watched repo.
/// Returns a `Vec<IssueSummary>` and a `Vec<IssuePollDiagnostic>`;
/// the caller is responsible for queue admission.
pub async fn poll_code(
    client: &Client,
    cfg: &Config,
    repos: &[String],
) -> CaduceusResult<IssuePollOutcome> {
    poll_label(client, cfg, repos, &cfg.ticket_label_code, TicketType::Code).await
}

/// Poll for label `ticket_label_investigation` across every
/// watched repo. Same shape as [`poll_code`].
pub async fn poll_investigation(
    client: &Client,
    cfg: &Config,
    repos: &[String],
) -> CaduceusResult<IssuePollOutcome> {
    poll_label(
        client,
        cfg,
        repos,
        &cfg.ticket_label_investigation,
        TicketType::Investigation,
    )
    .await
}

/// Poll every repo for *label*, classifying each returned object
/// into a unique `IssueSummary`, an ambiguous record, a
/// pull-request skip, an unmatched skip, or a malformed skip.
/// Rate-limit and page-cap failures short-circuit the whole poll.
async fn poll_label(
    client: &Client,
    cfg: &Config,
    repos: &[String],
    label: &str,
    ticket_type: TicketType,
) -> CaduceusResult<IssuePollOutcome> {
    let max_pages = cfg.discovery_max_pages as usize;
    let mut outcome = IssuePollOutcome::default();
    for repo in repos {
        if !is_valid_repo_slug(repo) {
            return Err(CaduceusError::Config(format!(
                "watched_repos contains invalid slug: {repo}"
            )));
        }
        let query_label = url_encode_label(label);
        let initial_path = format!(
            "/repos/{repo}/issues?state=open&labels={query_label}&per_page={}&sort=updated&direction=desc",
            ISSUES_PER_PAGE
        );
        let mut next: Option<Url> = Some(client.base_url().clone().join(&initial_path).map_err(
            |err| CaduceusError::Config(format!("cannot join {initial_path} onto api_base: {err}")),
        )?);
        let mut pages = 0usize;
        while let Some(url) = next.take() {
            if pages >= max_pages {
                return Err(CaduceusError::Other(format!(
                    "{ticket_type:?} issue poll for {repo} exceeded {max_pages} pages"
                )));
            }
            pages += 1;
            let response = client.get_url(&url, ACCEPT_VALUE).await?;
            if let Some(observation) = rate_limit_from_headers(&response.headers, response.status) {
                if observation.remaining == 0 {
                    return Err(rate_limit_error(observation));
                }
            }
            let issues: Vec<IssueObject> =
                serde_json::from_slice(&response.body).map_err(|err| {
                    CaduceusError::Other(format!(
                        "{ticket_type:?} issue poll for {repo} page {pages}: JSON parse: {err}"
                    ))
                })?;
            for issue in issues {
                classify_issue(issue, repo, label, ticket_type, &mut outcome);
            }
            next = parse_next_link(&response);
        }
    }
    // Stable order so callers can diff snapshots across ticks.
    outcome.summaries.sort_by_key(|a| a.key.display_key());
    outcome.diagnostics.sort_by_key(diagnostic_key);
    Ok(outcome)
}

/// Classify one GitHub `/issues` row into the canonical
/// `IssueSummary` stream or one of the diagnostic buckets.
fn classify_issue(
    issue: IssueObject,
    repo: &str,
    expected_label: &str,
    ticket_type: TicketType,
    outcome: &mut IssuePollOutcome,
) {
    // Pull-request objects appear in `/issues` because GitHub
    // unifies the endpoint; exclude them per the contract.
    if issue.pull_request.is_some() {
        if let Ok(key) = key_from_issue(repo, &issue) {
            outcome.diagnostics.push(IssuePollDiagnostic::PullRequest {
                key,
                title: issue.title.unwrap_or_default(),
            });
        } else {
            outcome.diagnostics.push(IssuePollDiagnostic::Malformed {
                key: None,
                reason: "pull-request object missing issue identity".to_string(),
            });
        }
        return;
    }
    let key = match key_from_issue(repo, &issue) {
        Ok(key) => key,
        Err(err) => {
            outcome.diagnostics.push(IssuePollDiagnostic::Malformed {
                key: None,
                reason: err.to_string(),
            });
            return;
        }
    };
    let title = issue.title.unwrap_or_default();
    let labels = issue
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.name.unwrap_or_default())
        .collect::<Vec<_>>();
    let updated_at = issue.updated_at.unwrap_or_else(Utc::now);
    // Verify the label is on the issue — the contract says the
    // query alone is not enough.
    let label_match = labels
        .iter()
        .any(|name| name.eq_ignore_ascii_case(expected_label));
    if !label_match {
        outcome
            .diagnostics
            .push(IssuePollDiagnostic::Unmatched { key, title, labels });
        return;
    }
    outcome.summaries.push(IssueSummary {
        key,
        title,
        labels,
        ticket_type,
        updated_at,
    });
}

/// Build an `IssueKey` from the repo path and an issue object,
/// returning a precise reason string for malformed rows.
fn key_from_issue(repo: &str, issue: &IssueObject) -> CaduceusResult<IssueKey> {
    let number = issue
        .number
        .ok_or_else(|| CaduceusError::Other(format!("issue in {repo} missing number")))?;
    if number == 0 {
        return Err(CaduceusError::Other(format!(
            "issue in {repo} has non-positive number {number}"
        )));
    }
    let (owner, repo_name) = repo
        .split_once('/')
        .ok_or_else(|| CaduceusError::Other(format!("watched_repos entry missing '/': {repo}")))?;
    let key = IssueKey {
        owner: owner.to_string(),
        repo: repo_name.to_string(),
        number,
    };
    key.validate().map_err(|err| {
        CaduceusError::Other(format!("issue key invalid for {repo}#{number}: {err:?}"))
    })?;
    Ok(key)
}

/// Sortable discriminator for diagnostics. `PullRequest` /
/// `Malformed` entries may not carry a usable key, so we render a
/// textual discriminator instead.
fn diagnostic_key(diag: &IssuePollDiagnostic) -> String {
    match diag {
        IssuePollDiagnostic::Ambiguous { key, .. }
        | IssuePollDiagnostic::Unmatched { key, .. }
        | IssuePollDiagnostic::PullRequest { key, .. } => key.display_key(),
        IssuePollDiagnostic::Malformed { key: Some(key), .. } => key.display_key(),
        IssuePollDiagnostic::Malformed { key: None, reason } => format!("malformed:{reason}"),
    }
}

/// UTF-8 percent-encoding for label values. GitHub's `/issues`
/// `labels=` query parameter must be URL-encoded even when the
/// label contains only ASCII; emoji-laden labels (the documented
/// default `🤖 auto-fix`) MUST round-trip through a UTF-8
/// percent-encoded form.
pub fn url_encode_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for byte in label.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            // Non-ASCII bytes are percent-encoded as `%XX` of the
            // underlying UTF-8 byte sequence. This matches what
            // the `url` crate does for `Url::set_query`.
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Merge the code and investigation poll outcomes. Issues that
/// appear in both with different ticket types are reported as
/// `Ambiguous`; everything else passes through. This is the
/// `merge_results` step the contract alludes to.
pub fn merge_outcomes(code: IssuePollOutcome, investigation: IssuePollOutcome) -> IssuePollOutcome {
    let mut by_key: BTreeMap<String, IssueSummary> = BTreeMap::new();
    let mut diagnostics = code.diagnostics;
    diagnostics.extend(investigation.diagnostics);

    // Walk the code summaries first so the ticket_type the merge
    // records for an ambiguous row is consistent across runs.
    for summary in code.summaries {
        by_key.insert(summary.key.display_key(), summary);
    }
    for summary in investigation.summaries {
        let key = summary.key.display_key();
        if let Some(existing) = by_key.get(&key) {
            if existing.ticket_type != summary.ticket_type {
                diagnostics.push(IssuePollDiagnostic::Ambiguous {
                    key: summary.key.clone(),
                    title: summary.title.clone(),
                    labels: summary.labels.clone(),
                });
                by_key.remove(&key);
            }
            // Same ticket type from both queries: keep the first.
        } else {
            by_key.insert(key, summary);
        }
    }
    let mut merged: Vec<IssueSummary> = by_key.into_values().collect();
    merged.sort_by_key(|a| a.key.display_key());
    diagnostics.sort_by_key(diagnostic_key);
    IssuePollOutcome {
        summaries: merged,
        diagnostics,
    }
}

/// One row of the GitHub `/repos/{slug}/issues` payload. The
/// `pull_request` field is the documented PR discriminator. The
/// `title` and `labels` are intentionally optional so the
/// parser can survive the "empty/null body tolerance" cases.
#[derive(Debug, Deserialize)]
struct IssueObject {
    number: Option<u64>,
    title: Option<String>,
    labels: Option<Vec<IssueLabelObject>>,
    updated_at: Option<DateTime<Utc>>,
    /// Presence of this field signals the object is a pull
    /// request, not an issue.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct IssueLabelObject {
    name: Option<String>,
}
