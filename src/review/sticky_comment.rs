//! Sticky PR comment publication for Auto Review (DAR §9.2–9.3,
//! §10.3 — `docs/architecture/auto-review.md`; issue #308).
//!
//! One comment per PR carries the current review verdict. Ownership:
//! `ReviewState.sticky_comment_id` is authoritative; a marker search
//! over the PR's comment list is the recovery fallback (crash-heal and
//! gone-state A).
//!
//! The renderer ([`render_sticky_comment`]) is pure and deterministic:
//! the same [`RenderInput`] always produces byte-identical output. It
//! reserves the header (marker, verdict heading, reviewed SHA,
//! stale-revision notice, update banner (re-publications)) first and
//! NEVER front-truncates — only findings are dropped from the tail,
//! inside a hard byte budget.
//!
//! [`publish`] owns the four gone-states (DAR §9.3): a deleted comment
//! (A) is recovered via marker search; a vanished PR (B) and a
//! closed-unmerged PR (C) are quiet skips; a merged PR (D) publishes.
//! The stale-generation suppression decision itself belongs to the
//! #310 finalizer — this module takes the current [`ReviewState`] in
//! and returns a [`StickyOutcome`] out; it never touches the store, so
//! the `review_generation` CAS guard stays with the finalizer.

use serde::Deserialize;
use url::Url;

use crate::github::issue::COMMENTS_PER_PAGE;
use crate::github::link_header::next_url_from_link_header;
use crate::github::merge_detect::MergeStatus;
use crate::github::pr::{create_pr_comment, update_pr_comment};
use crate::github::{check_voice_or_error, Client, Response, VoiceChannel, ACCEPT_VALUE};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::review::{Finding, Review, ReviewState, Severity, Verdict};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The one-per-PR auto-review marker (DAR §9.2). Hidden HTML comment,
/// no run id — exactly one sticky comment per PR, not per run, so
/// marker adoption works across runs.
pub const REVIEW_MARKER: &str = "<!-- caduceus-auto-review -->";

/// Hard cap on the rendered sticky body. Matches the validator's
/// `DEFAULT_COMMENT_MAX_BYTES` (`src/finalize/voice.rs`) and GitHub's
/// documented 65,536-byte comment limit.
pub const STICKY_COMMENT_MAX_BYTES: usize = 65_536;

/// Marker-search pagination cap. Mirrors the comments cap at
/// `src/github/issue.rs` (`MAX_PAGES = 20`) so a PR with a very long
/// comment thread cannot exhaust the discovery budget.
pub const STICKY_MARKER_SEARCH_MAX_PAGES: usize = 20;

/// Truncation notice appended when at least one finding was dropped.
/// Reserved in every render's budget so it can always be appended.
const TRUNCATION_NOTICE: &str = "_Additional findings truncated to fit the comment size limit._\n";

// ---------------------------------------------------------------------------
// Renderer (pure)
// ---------------------------------------------------------------------------

/// Inputs to the sticky-comment renderer. All fields the renderer needs
/// to reserve header space and detect staleness; none of the gone-state
/// or HTTP machinery (those live on [`publish`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderInput<'a> {
    /// The validated review to publish. Failure results never reach the
    /// renderer — the caller finalizes `ExecutionStatus::Success`
    /// reviews only (DAR §8).
    pub review: &'a Review,
    /// Head SHA the review was run against (identity, frozen at
    /// discovery).
    pub reviewed_head_sha: &'a str,
    /// The PR's current head SHA at publication time. When this differs
    /// from `reviewed_head_sha`, the renderer emits the stale-revision
    /// notice (DAR §9.2: stale results still publish with the reviewed
    /// SHA noted).
    pub current_head_sha: Option<&'a str>,
    /// The completing run's `review_generation` — 1 on first
    /// publication. Any generation > 1 renders the update banner
    /// (issue #393): the sticky comment is edited in place, so the
    /// banner is the at-a-glance signal that the review was re-run
    /// for a new commit.
    pub review_generation: u64,
}

/// Render the sticky comment body. Pure and deterministic: the same
/// [`RenderInput`] always produces byte-identical output (DAR §9.2,
/// "re-publishing the same result is byte-identical").
///
/// Layout (fixed order — header reserved first, never front-truncated):
///
/// 1. [`REVIEW_MARKER`] (head marker).
/// 2. Blank line.
/// 3. Update banner (only when `review_generation > 1`, issue #393): a
///    `> [!IMPORTANT]` alert panel naming the reviewed short SHA and the
///    generation, so an in-place edit is visible at a glance.
/// 4. PASS/FAIL heading line (derived from `review.verdict`).
/// 5. Reviewed SHA line.
/// 6. Stale-revision notice line (only when `current_head_sha` is
///    `Some` and differs from `reviewed_head_sha`).
/// 7. Summary.
/// 8. Findings in severity order (Blocking → Warning → Suggestion,
///    stable within severity = persisted `findings` order). Consumption
///    stops when the next finding would overflow the remaining budget.
/// 9. Truncation notice (only when at least one finding was dropped).
/// 10. Blank line, then [`REVIEW_MARKER`] again (tail marker).
///
/// The total is bounded by [`STICKY_COMMENT_MAX_BYTES`]. Only findings
/// (and, in the pathological over-cap-summary case, the summary tail)
/// are dropped — never the header.
pub fn render_sticky_comment(input: &RenderInput<'_>) -> String {
    let review = input.review;
    let heading = match review.verdict {
        Verdict::Pass => "## Auto review: PASS",
        Verdict::Fail => "## Auto review: FAIL",
    };

    let mut head = String::new();
    head.push_str(REVIEW_MARKER);
    head.push_str("\n\n");
    // Update banner (issue #393): re-publications edit the comment in
    // place, so the banner is the at-a-glance signal. Pushed onto
    // `head` BEFORE the reserve computation below, which makes it
    // part of the never-front-truncated header with zero budget-math
    // changes.
    if input.review_generation > 1 {
        head.push_str(&format!(
            "> [!IMPORTANT] Updated for commit `{}` (review generation {})\n\n",
            short_sha(input.reviewed_head_sha),
            input.review_generation
        ));
    }
    head.push_str(heading);
    head.push('\n');
    head.push_str("Reviewed SHA: ");
    head.push_str(input.reviewed_head_sha);
    head.push('\n');
    if let Some(current) = input.current_head_sha {
        if current != input.reviewed_head_sha {
            head.push_str("> Stale revision: this review covers ");
            head.push_str(input.reviewed_head_sha);
            head.push_str(" but the pull request head is now ");
            head.push_str(current);
            head.push_str(".\n");
        }
    }

    // Reserve: head + truncation notice (even when unused, so the
    // notice can always be appended) + tail marker + surrounding
    // newlines.
    let reserved = head.len() + TRUNCATION_NOTICE.len() + REVIEW_MARKER.len() + 2;
    let mut remaining = STICKY_COMMENT_MAX_BYTES.saturating_sub(reserved);

    let mut out = String::with_capacity(head.len() + review.summary.len());
    out.push_str(&head);

    // Summary + separating blank line. Tail-truncated at a char
    // boundary if it alone would overflow; the header is never touched.
    let summary_len = review.summary.len() + 2;
    let mut summary_clipped = false;
    if summary_len > remaining {
        summary_clipped = true;
        let separator = remaining.min(2);
        push_char_bounded(&mut out, &review.summary, remaining - separator);
        for _ in 0..separator {
            out.push('\n');
        }
        remaining = 0;
    } else {
        out.push_str(&review.summary);
        out.push_str("\n\n");
        remaining -= summary_len;
    }

    let mut dropped = false;
    for finding in findings_in_severity_order(review) {
        let block = render_finding(finding);
        if block.len() > remaining {
            dropped = true;
            break;
        }
        out.push_str(&block);
        remaining -= block.len();
    }
    // The truncation notice fires when ANY content was dropped — a
    // clipped summary or an unconsumed finding (AC2's "truncation note
    // present" is a property of the render, not only of findings).
    if dropped || summary_clipped {
        out.push_str(TRUNCATION_NOTICE);
    }
    out.push('\n');
    out.push_str(REVIEW_MARKER);
    out.push('\n');
    out
}

/// Findings in renderer consumption order: Blocking → Warning →
/// Suggestion, stable within severity (stable sort preserves the
/// persisted `findings` order, which is load-bearing for byte-identical
/// re-publication — DAR §3, §9.2).
fn findings_in_severity_order(review: &Review) -> Vec<&Finding> {
    let mut ordered: Vec<&Finding> = review.findings.iter().collect();
    ordered.sort_by_key(|finding| match finding.severity {
        Severity::Blocking => 0u8,
        Severity::Warning => 1,
        Severity::Suggestion => 2,
    });
    ordered
}

/// One finding rendered to the fixed block template. The trailing
/// blank line keeps blocks self-contained and the render deterministic.
fn render_finding(finding: &Finding) -> String {
    let mut block = String::new();
    block.push_str("### ");
    block.push_str(&finding.title);
    block.push_str("\n\n");
    block.push_str(&finding.body);
    block.push_str("\n\n");
    if let Some(path) = &finding.path {
        match finding.line {
            Some(line) => {
                block.push_str(&format!("`{path}:{line}`\n\n"));
            }
            None => {
                block.push_str(&format!("`{path}`\n\n"));
            }
        }
    }
    if let Some(remediation) = &finding.remediation {
        block.push_str(&format!("**Remediation:** {remediation}\n\n"));
    }
    block
}

/// Push at most `max_bytes` of *text* onto *out*, cutting at a UTF-8
/// char boundary (deterministic for a given input).
fn push_char_bounded(out: &mut String, text: &str, max_bytes: usize) {
    if text.len() <= max_bytes {
        out.push_str(text);
        return;
    }
    let mut keep = max_bytes;
    while keep > 0 && !text.is_char_boundary(keep) {
        keep -= 1;
    }
    out.push_str(&text[..keep]);
}

/// First 12 characters of a SHA, or the whole string when shorter.
/// Char-boundary safe: head SHAs are bounded, non-empty strings at
/// the store layer, but a multi-byte value must never panic the
/// renderer. Mirrors the CLI's display convention
/// (`src/cli/review.rs::short_sha`); kept private so the renderer
/// stays self-contained.
fn short_sha(sha: &str) -> &str {
    if sha.len() > 12 && sha.is_char_boundary(12) {
        &sha[..12]
    } else {
        sha
    }
}

// ---------------------------------------------------------------------------
// Marker search (capped pagination)
// ---------------------------------------------------------------------------

/// One comment row from `/issues/{n}/comments` — only the fields the
/// marker scan needs.
#[derive(Debug, Deserialize)]
struct MarkerCommentWire {
    id: u64,
    body: Option<String>,
}

/// Find the existing sticky comment's id by scanning the PR's comment
/// list for the [`REVIEW_MARKER`] string. Capped at
/// [`STICKY_MARKER_SEARCH_MAX_PAGES`] pages (Link-header pagination,
/// mirroring `fetch_comments`). Returns the first matching comment id;
/// `None` if no marker-bearing comment exists.
///
/// This is the ownership fallback (DAR §9.2): `sticky_comment_id` is
/// authoritative; this is the recovery path when the id is stale
/// (gone-state A) or when a crash happened between create and
/// id-persist (crash-heal, AC3).
pub async fn find_sticky_comment_by_marker(
    client: &Client,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> CaduceusResult<Option<u64>> {
    let path =
        format!("/repos/{owner}/{repo}/issues/{pr_number}/comments?per_page={COMMENTS_PER_PAGE}");
    let mut page = 0usize;
    let mut url: Option<Url> = Some(join_api_path(client, &path));
    while let Some(current) = url.take() {
        if page >= STICKY_MARKER_SEARCH_MAX_PAGES {
            return Err(CaduceusError::Other(format!(
                "sticky marker search exceeded {STICKY_MARKER_SEARCH_MAX_PAGES} pages \
                 for {owner}/{repo}#{pr_number}"
            )));
        }
        page += 1;
        let response = client.get_url(&current, ACCEPT_VALUE).await?;
        let wire: Vec<MarkerCommentWire> =
            serde_json::from_slice(&response.body).map_err(|err| {
                CaduceusError::Other(format!(
                    "sticky marker search page {page} JSON parse: {err}"
                ))
            })?;
        for comment in wire {
            if comment
                .body
                .as_deref()
                .unwrap_or_default()
                .contains(REVIEW_MARKER)
            {
                return Ok(Some(comment.id));
            }
        }
        url = next_page_url(&response, page as u32 + 1);
    }
    Ok(None)
}

/// Join an API path (with optional query) onto the client's base URL.
fn join_api_path(client: &Client, path: &str) -> Url {
    let (path_only, query) = path.split_once('?').unwrap_or((path, ""));
    let mut url = client.base_url().clone();
    url.set_path(path_only);
    if !query.is_empty() {
        url.set_query(Some(query));
    }
    url
}

/// Parse the `Link: rel="next"` header and build the next page URL,
/// synthesizing `page=N&per_page=…` when the header omits a query
/// (mirrors `fetch_comments` so test fixtures stay simple).
fn next_page_url(response: &Response, next_page_number: u32) -> Option<Url> {
    use reqwest::header::LINK;
    let header = response.headers.get(LINK)?.to_str().ok()?;
    let next_url = next_url_from_link_header(header)?;
    if next_url.contains("page=") {
        return Url::parse(&next_url).ok();
    }
    let (path, query) = next_url.split_once('?').unwrap_or((&next_url, ""));
    let extra = format!("page={next_page_number}&per_page={COMMENTS_PER_PAGE}");
    let combined = if query.is_empty() {
        format!("{path}?{extra}")
    } else {
        format!("{path}?{query}&{extra}")
    };
    Url::parse(&combined).ok()
}

// ---------------------------------------------------------------------------
// Publish (four gone-states, DAR §9.3)
// ---------------------------------------------------------------------------

/// Why [`publish`] did or did not touch GitHub. Drives the #310
/// finalizer's FSM transitions and event emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StickyOutcome {
    /// First-time create OR update by id. Carries the comment id the
    /// finalizer must persist into `ReviewState.sticky_comment_id`.
    Published { comment_id: u64 },
    /// The rendered body was byte-identical to the existing comment;
    /// no PATCH was sent (idempotent re-finalization, AC1). The finalizer
    /// still persists `comment_id` — the crash-heal adoption path can
    /// return `Unchanged` for an id that was never persisted.
    Unchanged { comment_id: u64 },
    /// Gone-state A: comment PATCH 404 (deleted by a human) → marker
    /// search (capped) → adopt-or-create-new. Carries the NEW comment
    /// id; the finalizer persists it, replacing the stale id. A crash
    /// between create and id-persist self-heals because the next
    /// `publish` run finds the marker via
    /// [`find_sticky_comment_by_marker`] (AC3).
    CommentGoneRecreated { new_comment_id: u64 },
    /// Gone-state B: PR lookup 404. Quiet skip; NEVER recreate (AC4).
    PrNotFound,
    /// Gone-state C: PR closed without merge. Quiet skip + event
    /// (emitted by #310); the historical result remains persisted (AC4).
    PrClosedUnmerged,
    /// Reserved for the #310 finalizer: the merge was detected after
    /// the stale-generation guard suppressed the publication. `publish`
    /// itself always publishes merged PRs (`Published` / `Unchanged`).
    PrMergedSuppressed,
    /// Reserved for the #310 finalizer: the completing run's generation
    /// is older than the current `ReviewState.review_generation` (DAR
    /// §9.4). `publish` does not own the suppression decision; it only
    /// surfaces the state the finalizer needs.
    SuppressedStaleGeneration,
}

/// One comment body from `GET /issues/comments/{id}` — the idempotency
/// compare input.
#[derive(Debug, Deserialize)]
struct CommentBodyWire {
    body: Option<String>,
}

/// Fetch a comment's current body. A 404 surfaces as
/// [`CaduceusError::GitHubApi { status: 404, .. }`] (gone-state A).
async fn fetch_comment_body(
    client: &Client,
    owner: &str,
    repo: &str,
    comment_id: u64,
) -> CaduceusResult<String> {
    let path = format!("/repos/{owner}/{repo}/issues/comments/{comment_id}");
    let response = client.get(&path, ACCEPT_VALUE).await?;
    let wire: CommentBodyWire = serde_json::from_slice(&response.body).map_err(|err| {
        CaduceusError::Other(format!("comment {comment_id} body JSON parse: {err}"))
    })?;
    Ok(wire.body.unwrap_or_default())
}

/// Publish (or re-publish) the sticky comment for one review (AC1–AC4).
///
/// * `state` is the current [`ReviewState`] (authoritative
///   `sticky_comment_id`, if any). The store is NOT written here — the
///   returned [`StickyOutcome`] carries the id the #310 finalizer must
///   persist, keeping the `review_generation` CAS guard at the
///   finalizer.
/// * `input` carries the review + SHAs (consumed by the pure renderer).
/// * `pr_state` is the PR's current lifecycle (from
///   `poll_pr_merge_status`) — the caller resolves it once before
///   calling, so this function owns no merge-detection retry loop.
///
/// Flow: gone-states B/C are classified first (`PrNotFound` /
/// `PrClosedUnmerged`, no HTTP); D (merged) and still-open proceed.
/// With an authoritative id: fetch the current body — byte-identical →
/// `Unchanged` (no PATCH); different → PATCH. A 404 on the GET or PATCH
/// is gone-state A: marker search → adopt via PATCH, else create-new →
/// `CommentGoneRecreated`. With no authoritative id: marker search
/// first (crash-heal adoption for a create whose id was never
/// persisted), then the same compare/PATCH path; no marker → create.
#[allow(clippy::too_many_arguments)] // plan §3.4 surface: fixed 8-arg #310 contract
pub async fn publish(
    client: &Client,
    cfg: &Config,
    owner: &str,
    repo: &str,
    pr_number: u64,
    state: &ReviewState,
    input: &RenderInput<'_>,
    pr_state: MergeStatus,
) -> CaduceusResult<StickyOutcome> {
    // Gone-states B/C/D classification (DAR §9.3). B and C are quiet
    // skips with zero HTTP; D (merged) and still-open publish.
    match pr_state {
        MergeStatus::NotFound => return Ok(StickyOutcome::PrNotFound),
        MergeStatus::ClosedWithoutMerge => return Ok(StickyOutcome::PrClosedUnmerged),
        MergeStatus::Merged { .. } | MergeStatus::StillOpen => {}
    }

    let body = render_sticky_comment(input);
    // Public-voice gate BEFORE any HTTP (DAR §9.2; the wrappers gate
    // again internally, but the marker search and body compare below
    // are GETs, so the explicit gate here guarantees zero network
    // traffic for a rejected body).
    check_voice_or_error(&body, cfg, VoiceChannel::Comment)?;
    match state.sticky_comment_id {
        Some(id) => match fetch_comment_body(client, owner, repo, id).await {
            Ok(existing) if existing == body => Ok(StickyOutcome::Unchanged { comment_id: id }),
            Ok(_) => apply_update(client, cfg, owner, repo, pr_number, id, &body).await,
            Err(CaduceusError::GitHubApi { status: 404, .. }) => {
                recreate_after_comment_gone(client, cfg, owner, repo, pr_number, &body, Some(id))
                    .await
            }
            Err(err) => Err(err),
        },
        None => match find_sticky_comment_by_marker(client, owner, repo, pr_number).await? {
            Some(id) => {
                // Crash-heal adoption (AC3): a prior create succeeded but
                // the id was never persisted. PATCH the adopted id — no
                // duplicate is created.
                match fetch_comment_body(client, owner, repo, id).await {
                    Ok(existing) if existing == body => {
                        Ok(StickyOutcome::Unchanged { comment_id: id })
                    }
                    Ok(_) => apply_update(client, cfg, owner, repo, pr_number, id, &body).await,
                    Err(CaduceusError::GitHubApi { status: 404, .. }) => {
                        recreate_after_comment_gone(
                            client,
                            cfg,
                            owner,
                            repo,
                            pr_number,
                            &body,
                            Some(id),
                        )
                        .await
                    }
                    Err(err) => Err(err),
                }
            }
            None => {
                let new_id = create_pr_comment(client, cfg, owner, repo, pr_number, &body).await?;
                Ok(StickyOutcome::Published { comment_id: new_id })
            }
        },
    }
}

/// PATCH an existing (authoritative or adopted) comment id. A 404 here
/// is gone-state A and routes to recovery; everything else propagates.
async fn apply_update(
    client: &Client,
    cfg: &Config,
    owner: &str,
    repo: &str,
    pr_number: u64,
    comment_id: u64,
    body: &str,
) -> CaduceusResult<StickyOutcome> {
    match update_pr_comment(client, cfg, owner, repo, comment_id, body).await {
        Ok(()) => Ok(StickyOutcome::Published { comment_id }),
        Err(CaduceusError::GitHubApi { status: 404, .. }) => {
            recreate_after_comment_gone(client, cfg, owner, repo, pr_number, body, Some(comment_id))
                .await
        }
        Err(err) => Err(err),
    }
}

/// Gone-state A recovery (DAR §9.3 row A): the comment id is stale
/// (deleted by a human). Search the marker (capped pages); adopt the
/// found comment via PATCH — never create a duplicate when adoption is
/// possible — otherwise create a new one. Either way the caller gets
/// `CommentGoneRecreated` with the id to persist. A single recovery
/// pass: if the adopted PATCH 404s too (deleted in the race window),
/// fall through to create-new; if the create fails, the error
/// propagates for #310's retryable-failure handling.
async fn recreate_after_comment_gone(
    client: &Client,
    cfg: &Config,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
    failed_id: Option<u64>,
) -> CaduceusResult<StickyOutcome> {
    let found = find_sticky_comment_by_marker(client, owner, repo, pr_number).await?;
    match found {
        Some(id) if Some(id) != failed_id => {
            match update_pr_comment(client, cfg, owner, repo, id, body).await {
                Ok(()) => Ok(StickyOutcome::CommentGoneRecreated { new_comment_id: id }),
                Err(CaduceusError::GitHubApi { status: 404, .. }) => {
                    let new_id =
                        create_pr_comment(client, cfg, owner, repo, pr_number, body).await?;
                    Ok(StickyOutcome::CommentGoneRecreated {
                        new_comment_id: new_id,
                    })
                }
                Err(err) => Err(err),
            }
        }
        _ => {
            let new_id = create_pr_comment(client, cfg, owner, repo, pr_number, body).await?;
            Ok(StickyOutcome::CommentGoneRecreated {
                new_comment_id: new_id,
            })
        }
    }
}
