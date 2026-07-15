//! Typed issue identity and detail schema. The shape is normative and is
//! re-exported from `lib.rs`. Validation rules live in this module per
//! `CONTRACTS.md` "Issue identity and queue schema".
//!
//! The detail fetcher (Task 2.6) issues three concurrent GitHub
//! requests — the issue, its comments, and its timeline — and
//! cancels the other two on the first error. The output is the
//! typed `IssueDetail` the worker uses to build its prompt.

use chrono::{DateTime, Utc};
use futures_util::future::try_join3;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{CaduceusError, CaduceusResult};
use crate::github::{Client, Response, ACCEPT_VALUE};

/// GitHub-canonical issue identifier: `(owner, repo, number)`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IssueKey {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl IssueKey {
    /// Lowercased `owner/repo#number` form used as a queue-key and to
    /// derive the on-disk claim filename via SHA-256.
    pub fn display_key(&self) -> String {
        format!(
            "{}/{}{}{}",
            self.owner.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
            '#',
            self.number
        )
    }

    /// Parse an `owner/repo#number` reference. The input may have
    /// any casing for owner/repo; validation normalises the casing
    /// rules but preserves the original case in the returned
    /// struct (so API paths use GitHub's canonical case). Returns
    /// a [`CaduceusError::Config`] for malformed input — never
    /// panics.
    pub fn parse(input: &str) -> CaduceusResult<Self> {
        let (head, number_text) = input
            .split_once('#')
            .ok_or_else(|| CaduceusError::Config(format!("issue ref missing '#': {input}")))?;
        let (owner, repo) = head
            .split_once('/')
            .ok_or_else(|| CaduceusError::Config(format!("issue ref missing '/': {input}")))?;
        if owner.is_empty() || repo.is_empty() {
            return Err(CaduceusError::Config(format!(
                "issue ref has empty owner or repo: {input}"
            )));
        }
        if repo.contains('/') {
            return Err(CaduceusError::Config(format!(
                "issue ref has extra '/': {input}"
            )));
        }
        let number = number_text.parse::<u64>().map_err(|err| {
            CaduceusError::Config(format!("issue ref number parse: {input} ({err})"))
        })?;
        if number == 0 {
            return Err(CaduceusError::Config(format!(
                "issue number must be positive: {input}"
            )));
        }
        let key = Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        };
        key.validate()?;
        Ok(key)
    }

    /// Validate identifier components per `CONTRACTS.md`.
    pub fn validate(&self) -> CaduceusResult<()> {
        validate_owner(&self.owner)?;
        validate_repo(&self.repo)?;
        if self.number == 0 {
            return Err(CaduceusError::Other(
                "issue number must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for IssueKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Maximum number of comments retained after pagination. The
/// contract pins "most recent 100 in chronological order" so a
/// very long-running issue doesn't blow up the prompt.
pub const MAX_RETAINED_COMMENTS: usize = 100;
/// Maximum number of events retained from the timeline. Matches
/// the comments cap.
pub const MAX_RETAINED_EVENTS: usize = 100;
/// Comments and events pages are capped at 20 per endpoint
/// (matches the GitHub-side hard limit; CONTRACTS.md).
pub const MAX_PAGES: usize = 20;
/// Per-page row count. GitHub's documented maximum.
pub const COMMENTS_PER_PAGE: u32 = 100;
/// Per-page row count for the timeline endpoint.
pub const EVENTS_PER_PAGE: u32 = 100;

/// Full issue detail assembled from the GitHub API. The fields
/// here are exactly the ones the worker uses to build its
/// prompt and the public-voice validator inspects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub key: IssueKey,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub comments: Vec<IssueComment>,
    /// Comments from authors who pass the configured
    /// `feedback_author_allowlist` (or who are not matched by
    /// any `comment_ignore_patterns`). Filtered server-side by
    /// the daemon, not by the wire format.
    pub trusted_comments: Vec<IssueComment>,
    /// Label events extracted from the timeline in chronological
    /// order. Used by the public-voice validator to attribute
    /// label changes.
    pub events: Vec<IssueEvent>,
    pub fetched_at: DateTime<Utc>,
}

/// One GitHub comment attached to an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// One event from the issue's timeline. The kind is intentionally
/// a string so the daemon can carry unknown event kinds forward
/// without losing information; label events are the primary
/// type the worker cares about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueEvent {
    /// Event kind as reported by GitHub (`labeled`,
    /// `unlabeled`, `closed`, `reopened`, `assigned`, …).
    pub kind: String,
    /// The actor's login. Empty when the actor is missing
    /// (e.g. a deleted account) or the event is system-generated.
    pub actor: String,
    /// Timestamp of the event in UTC.
    pub created_at: DateTime<Utc>,
    /// Label name for `labeled` / `unlabeled` events; `None` for
    /// any other kind.
    pub label_name: Option<String>,
}

impl IssueDetail {
    /// Convenience for tests: build a `CADUCEUS_CONTEXT_JSON` payload.
    pub fn to_context_json(&self) -> CaduceusResult<String> {
        serde_json::to_string(self)
            .map_err(|err| CaduceusError::Other(format!("serialize IssueDetail: {err}")))
    }
}

/// Fetch the full issue detail for *key*. The function issues
/// three concurrent GitHub requests — the issue, its comments,
/// and its timeline — and cancels the other two on the first
/// error. The `trusted_comments` filter is applied here so the
/// returned struct is the worker's authoritative view.
pub async fn fetch_issue_detail(
    client: &Client,
    key: &IssueKey,
    trusted_authors: &[String],
) -> CaduceusResult<IssueDetail> {
    let issue_path = format!("/repos/{}/{}/issues/{}", key.owner, key.repo, key.number);
    let comments_path = format!(
        "/repos/{}/{}/issues/{}/comments",
        key.owner, key.repo, key.number
    );
    let events_path = format!(
        "/repos/{}/{}/issues/{}/events",
        key.owner, key.repo, key.number
    );

    let issue_fut = fetch_issue_object(client, &issue_path);
    let comments_fut = fetch_comments(client, &comments_path);
    let events_fut = fetch_events(client, &events_path);
    let (issue, comments, events) = try_join3(issue_fut, comments_fut, events_fut).await?;

    let body = issue.body.unwrap_or_default();
    let labels = issue
        .labels
        .unwrap_or_default()
        .into_iter()
        .map(|l| l.name.unwrap_or_default())
        .collect();
    let (comments, trusted_comments) = partition_comments(comments, trusted_authors);

    Ok(IssueDetail {
        key: key.clone(),
        title: issue.title.unwrap_or_default(),
        body,
        labels,
        comments,
        trusted_comments,
        events,
        fetched_at: Utc::now(),
    })
}

/// Issue fetched from `/repos/{owner}/{repo}/issues/{n}`.
#[derive(Debug, Deserialize)]
struct IssueWire {
    title: Option<String>,
    body: Option<String>,
    labels: Option<Vec<LabelWire>>,
}

#[derive(Debug, Deserialize)]
struct LabelWire {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommentWire {
    body: Option<String>,
    user: Option<UserWire>,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct UserWire {
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventWire {
    #[serde(rename = "event")]
    kind: Option<String>,
    actor: Option<UserWire>,
    created_at: Option<DateTime<Utc>>,
    label: Option<LabelWire>,
}

async fn fetch_issue_object(client: &Client, path: &str) -> CaduceusResult<IssueWire> {
    let response = client.get(path, ACCEPT_VALUE).await?;
    decode_issue_body(&response)
}

async fn fetch_comments(client: &Client, path: &str) -> CaduceusResult<Vec<IssueComment>> {
    let mut page = 0usize;
    let mut url: Option<Url> = Some(join_issues_path(client, path));
    let mut all: Vec<IssueComment> = Vec::new();
    while let Some(current) = url.take() {
        if page >= MAX_PAGES {
            return Err(CaduceusError::Other(format!(
                "comments exceeded {MAX_PAGES} pages for {path}"
            )));
        }
        page += 1;
        let response = client.get_url(&current, ACCEPT_VALUE).await?;
        let wire: Vec<CommentWire> = serde_json::from_slice(&response.body).map_err(|err| {
            CaduceusError::Other(format!("comments JSON parse page {page}: {err}"))
        })?;
        for c in wire {
            all.push(IssueComment {
                author: c.user.and_then(|u| u.login).unwrap_or_default(),
                body: c.body.unwrap_or_default(),
                created_at: c.created_at.unwrap_or_else(Utc::now),
            });
        }
        url = next_page(&response, page as u32 + 1, COMMENTS_PER_PAGE);
    }
    // Most recent 100 in chronological order: sort ascending by
    // created_at, then take the last 100.
    all.sort_by_key(|c| c.created_at);
    if all.len() > MAX_RETAINED_COMMENTS {
        let drop = all.len() - MAX_RETAINED_COMMENTS;
        all.drain(..drop);
    }
    Ok(all)
}

async fn fetch_events(client: &Client, path: &str) -> CaduceusResult<Vec<IssueEvent>> {
    let mut page = 0usize;
    let mut url: Option<Url> = Some(join_issues_path(client, path));
    let mut all: Vec<IssueEvent> = Vec::new();
    while let Some(current) = url.take() {
        if page >= MAX_PAGES {
            return Err(CaduceusError::Other(format!(
                "events exceeded {MAX_PAGES} pages for {path}"
            )));
        }
        page += 1;
        let response = client.get_url(&current, ACCEPT_VALUE).await?;
        let wire: Vec<EventWire> = serde_json::from_slice(&response.body)
            .map_err(|err| CaduceusError::Other(format!("events JSON parse page {page}: {err}")))?;
        for e in wire {
            all.push(IssueEvent {
                kind: e.kind.unwrap_or_default(),
                actor: e.actor.and_then(|u| u.login).unwrap_or_default(),
                created_at: e.created_at.unwrap_or_else(Utc::now),
                label_name: e.label.and_then(|l| l.name),
            });
        }
        url = next_page(&response, page as u32 + 1, EVENTS_PER_PAGE);
    }
    all.sort_by_key(|e| e.created_at);
    if all.len() > MAX_RETAINED_EVENTS {
        let drop = all.len() - MAX_RETAINED_EVENTS;
        all.drain(..drop);
    }
    Ok(all)
}

fn decode_issue_body(response: &Response) -> CaduceusResult<IssueWire> {
    serde_json::from_slice(&response.body)
        .map_err(|err| CaduceusError::Other(format!("issue JSON parse: {err}")))
}

fn partition_comments(
    mut comments: Vec<IssueComment>,
    trusted_authors: &[String],
) -> (Vec<IssueComment>, Vec<IssueComment>) {
    let trusted: Vec<IssueComment> = if trusted_authors.is_empty() {
        Vec::new()
    } else {
        comments
            .iter()
            .filter(|c| trusted_authors.iter().any(|a| a == &c.author))
            .cloned()
            .collect()
    };
    // Drain trusted from comments? We keep them in `comments`
    // too — the caller can decide whether to dedup at render
    // time. The contract says trusted_comments is a filter of
    // comments, not a partition.
    let _ = &mut comments;
    (comments, trusted)
}

fn join_issues_path(client: &Client, path: &str) -> Url {
    let base = client.base_url();
    let (path_only, query) = split_path_query(path);
    let mut url = base.clone();
    url.set_path(path_only);
    if !query.is_empty() {
        url.set_query(Some(query));
    }
    url
}

fn split_path_query(input: &str) -> (&str, &str) {
    match input.split_once('?') {
        Some((p, q)) => (p, q),
        None => (input, ""),
    }
}

fn next_page(response: &Response, next_page_number: u32, per_page: u32) -> Option<Url> {
    use reqwest::header::LINK;
    let header = response.headers.get(LINK)?.to_str().ok()?;
    let next_url = next_url_from_link_header(header)?;
    // Some Link headers don't carry a query; construct a synthetic
    // page=N+1 URL on the same path so the test fixtures don't
    // have to know the page index ahead of time.
    if next_url.contains("page=") {
        return Url::parse(&next_url).ok();
    }
    let (path, query) = split_path_query(&next_url);
    let extra = format!("page={next_page_number}&per_page={per_page}");
    let combined = if query.is_empty() {
        format!("{path}?{extra}")
    } else {
        format!("{path}?{query}&{extra}")
    };
    Url::parse(&combined).ok()
}

fn next_url_from_link_header(header: &str) -> Option<String> {
    for segment in header.split(',') {
        let segment = segment.trim();
        let mut parts = segment.split(';');
        let url_part = parts.next()?.trim();
        let url = url_part
            .strip_prefix('<')
            .and_then(|s| s.strip_suffix('>'))?;
        for rel in parts {
            let rel = rel.trim();
            if rel == "rel=\"next\"" {
                return Some(url.to_string());
            }
        }
    }
    None
}

pub(crate) fn validate_owner(owner: &str) -> CaduceusResult<()> {
    if owner.is_empty() || owner.len() > 39 {
        return Err(CaduceusError::Other(format!(
            "owner must be 1..=39 chars; got {}",
            owner.len()
        )));
    }
    if owner.starts_with('-') || owner.ends_with('-') {
        return Err(CaduceusError::Other(
            "owner cannot begin or end with '-'".to_string(),
        ));
    }
    if !owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(CaduceusError::Other(format!(
            "owner contains invalid character: {owner}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_repo(repo: &str) -> CaduceusResult<()> {
    if repo.is_empty() || repo.len() > 100 {
        return Err(CaduceusError::Other(format!(
            "repo must be 1..=100 chars; got {}",
            repo.len()
        )));
    }
    if repo == "." || repo == ".." {
        return Err(CaduceusError::Other(format!("repo cannot be {repo}")));
    }
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(CaduceusError::Other(format!(
            "repo contains invalid character: {repo}"
        )));
    }
    Ok(())
}
