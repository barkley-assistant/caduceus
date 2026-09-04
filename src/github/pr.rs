//! Typed pull-request wire model and endpoint wrappers.
//!
//! [`PullRequestDetail`] decodes `GET /repos/{owner}/{repo}/pulls`
//! rows with all-`Option` fields (GitHub omits or nullifies fields
//! benignly; `head.repo` becomes `None` when the head branch is
//! deleted). [`list_pull_requests`] follows `rel="next"` Link
//! headers with the daemon-wide page cap and per-page rate-limit
//! check; [`fetch_pull_request`] maps a 404 to `Ok(None)`
//! (auto-review gone-state B, §9.3 of the spec);
//! [`create_pr_comment`] and [`update_pr_comment`] ride the
//! public-voice gate ([`check_voice_or_error`],
//! `VoiceChannel::Comment`) before any HTTP.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use url::Url;

use crate::github::link_header::next_url_from_link_header;
use crate::github::poll::MAX_PAGES_PER_ENDPOINT;
use crate::github::{
    check_voice_or_error, rate_limit_from_headers, Client, RateLimitInfo, Response, VoiceChannel,
    ACCEPT_VALUE,
};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Number of rows fetched per page on `/repos/{slug}/pulls`. The
/// contract pins the GitHub-side maximum so a single page always
/// fits the documented 100-row envelope.
pub const PRS_PER_PAGE: u32 = 100;

/// One pull request as returned by the GitHub pulls endpoints.
///
/// The wire shape is deliberately all-optional: GitHub adds and
/// nullifies fields benignly, and the auto-review contract needs
/// `head.repo` to be `None` (deleted head branch) rather than a
/// parse failure. `draft` defaults to `false` when omitted,
/// matching the `RepoObject.archived` convention.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequestDetail {
    pub number: Option<u64>,
    pub title: Option<String>,
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    /// `user.login` from the wire payload; `None` for bots or
    /// deleted accounts where GitHub omits/nullifies `user`.
    #[serde(default, rename = "user", deserialize_with = "de_login_of_user")]
    pub author: Option<String>,
    /// `"open"` | `"closed"` as reported by the API.
    pub state: Option<String>,
    pub merged: Option<bool>,
    pub merged_at: Option<DateTime<Utc>>,
    pub base: Option<PullRequestBranch>,
    /// `head.repo: null` (deleted head branch) lands here as
    /// `repo: None` — never a parse failure.
    pub head: Option<PullRequestBranch>,
}

/// One side of the PR's `base` / `head` pair.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequestBranch {
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    pub sha: Option<String>,
    pub repo: Option<PullRequestRepo>,
}

/// The repository a PR branch points at; `full_name` is the
/// `owner/name` pair used to detect fork PRs (`head.repo !=
/// base.repo`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PullRequestRepo {
    pub full_name: Option<String>,
}

/// Decode the wire's `user` object into just its `login`. The
/// `user` key is an object (`{"login": "octocat"}`) or `null`,
/// while the model exposes the flattened `author` string.
fn de_login_of_user<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct UserWire {
        login: Option<String>,
    }
    Ok(Option::<UserWire>::deserialize(deserializer)?.and_then(|u| u.login))
}

/// List pull requests for *owner/repo*.
///
/// `state=all` is deliberate: the auto-review discovery step needs
/// closed/merged rows as discrimination inputs too (§5.1). The
/// loop follows `Link: rel="next"` headers, caps at
/// [`MAX_PAGES_PER_ENDPOINT`] with a hard error (never silent
/// truncation), and surfaces a typed [`CaduceusError::RateLimited`]
/// when a page reports `x-ratelimit-remaining: 0`.
pub async fn list_pull_requests(
    client: &Client,
    owner: &str,
    repo: &str,
) -> CaduceusResult<Vec<PullRequestDetail>> {
    let initial_path =
        format!("/repos/{owner}/{repo}/pulls?state=all&per_page={PRS_PER_PAGE}&sort=updated");
    let mut next: Option<Url> = Some(client.base_url().clone().join(&initial_path).map_err(
        |err| CaduceusError::Config(format!("cannot join {initial_path} onto api_base: {err}")),
    )?);
    let mut collected: Vec<PullRequestDetail> = Vec::new();
    let mut pages = 0usize;
    while let Some(url) = next.take() {
        if pages >= MAX_PAGES_PER_ENDPOINT {
            return Err(CaduceusError::Other(format!(
                "pull request list for {owner}/{repo} exceeded {MAX_PAGES_PER_ENDPOINT} pages"
            )));
        }
        pages += 1;
        let response = client.get_url(&url, ACCEPT_VALUE).await?;
        if let Some(observation) = rate_limit_from_headers(&response.headers, response.status) {
            if observation.remaining == 0 {
                return Err(rate_limited(observation));
            }
        }
        let wire: Vec<PullRequestDetail> =
            serde_json::from_slice(&response.body).map_err(|err| {
                CaduceusError::Other(format!(
                    "pull request list for {owner}/{repo} page {pages}: JSON parse: {err}"
                ))
            })?;
        collected.extend(wire);
        next = parse_next_link(&response);
    }
    Ok(collected)
}

/// Fetch a single pull request by number.
///
/// A 404 surfaces from the transport as
/// [`CaduceusError::GitHubApi { status: 404, .. }`] and maps to
/// `Ok(None)` — the auto-review gone-state B input (§9.3, "never
/// recreate"). Every other error propagates unchanged; a 200 body
/// that does not parse as a [`PullRequestDetail`] is a malformed
/// response error.
pub async fn fetch_pull_request(
    client: &Client,
    owner: &str,
    repo: &str,
    number: u64,
) -> CaduceusResult<Option<PullRequestDetail>> {
    let path = format!("/repos/{owner}/{repo}/pulls/{number}");
    let response = match client.get(&path, ACCEPT_VALUE).await {
        Ok(response) => response,
        Err(CaduceusError::GitHubApi { status: 404, .. }) => return Ok(None),
        Err(err) => return Err(err),
    };
    let wire: PullRequestDetail = serde_json::from_slice(&response.body).map_err(|err| {
        CaduceusError::Other(format!(
            "pull request {number} JSON parse for {owner}/{repo}: {err}"
        ))
    })?;
    Ok(Some(wire))
}

/// Create the auto-review sticky comment on PR *pr_number*.
///
/// The public-voice gate runs **before any HTTP**: a rejected body
/// returns [`CaduceusError::Other`] without touching the network.
/// On success the comment `id` is parsed from the 201 response so
/// the caller can persist it as `ReviewState.sticky_comment_id`
/// (§9.2).
pub async fn create_pr_comment(
    client: &Client,
    cfg: &Config,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> CaduceusResult<u64> {
    check_voice_or_error(body, cfg, VoiceChannel::Comment)?;
    let path = format!("/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let payload = serde_json::to_vec(&serde_json::json!({ "body": body }))
        .map_err(|err| CaduceusError::Other(format!("serialize comment body: {err}")))?;
    let response = client.post(&path, ACCEPT_VALUE, &payload).await?;
    if response.status != 201 {
        return Err(CaduceusError::GitHubApi {
            status: response.status,
            message: format!("create PR comment: expected 201, got {}", response.status),
        });
    }
    let id = serde_json::from_slice::<serde_json::Value>(&response.body)
        .ok()
        .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64))
        .ok_or_else(|| {
            CaduceusError::Other(format!(
                "create PR comment for {owner}/{repo}#{pr_number}: response missing id"
            ))
        })?;
    Ok(id)
}

/// Update an existing comment by id (`PATCH /issues/comments/{id}`).
///
/// Same voice gate as [`create_pr_comment`]; a non-2xx (e.g. a 404
/// when a human deleted the comment) arrives from the transport as
/// [`CaduceusError::GitHubApi`] and propagates — the caller (#310)
/// discriminates gone-state A from it.
pub async fn update_pr_comment(
    client: &Client,
    cfg: &Config,
    owner: &str,
    repo: &str,
    comment_id: u64,
    body: &str,
) -> CaduceusResult<()> {
    check_voice_or_error(body, cfg, VoiceChannel::Comment)?;
    let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}");
    let payload = serde_json::to_vec(&serde_json::json!({ "body": body }))
        .map_err(|err| CaduceusError::Other(format!("serialize comment body: {err}")))?;
    let response = client.patch(&path, ACCEPT_VALUE, &payload).await?;
    if response.status != 200 {
        return Err(CaduceusError::GitHubApi {
            status: response.status,
            message: format!("update PR comment: expected 200, got {}", response.status),
        });
    }
    Ok(())
}

/// Parse the GitHub Link header and return the URL marked
/// `rel="next"` if any. Mirrors the poll loop's helper.
fn parse_next_link(response: &Response) -> Option<Url> {
    use reqwest::header::LINK;
    let header = response.headers.get(LINK)?.to_str().ok()?;
    next_url_from_link_header(header).and_then(|raw| Url::parse(&raw).ok())
}

/// Translate a [`RateLimitInfo`] into the typed
/// [`CaduceusError::RateLimited`] variant the daemon's outer loop
/// recognises. The list wrapper calls this only for a
/// `remaining == 0` observation.
fn rate_limited(observation: RateLimitInfo) -> CaduceusError {
    let now = chrono::Utc::now().timestamp();
    let reset_at = (observation.reset_at_unix.saturating_sub(now)).max(0) as u64;
    CaduceusError::RateLimited {
        reset_at,
        remaining: observation.remaining,
        limit: observation.limit,
    }
}
