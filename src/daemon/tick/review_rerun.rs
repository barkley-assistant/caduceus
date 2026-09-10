//! In-tick trusted-comment re-review listener (issue #335, DAR §17).
//!
//! Step 5.55 of the tick pipeline: for each watched repo's open PRs,
//! scan the PR's issue comments for the configured `rerun_command`
//! (default `/caduceus review`). A comment whose normalized line equals
//! the command, authored by someone on `Config.feedback_author_allowlist`,
//! enqueues an explicit re-review of the CURRENT head SHA through
//! `ReviewStore::enqueue_review_with_reason(..., ExplicitUserRequest)` —
//! deliberately bypassing the auto-discovery dedup so the SAME SHA can
//! be re-reviewed on demand (AC1/AC3, #335).
//!
//! An untrusted author's trigger comment is ignored WITH a structured
//! `review_rerun_skipped_untrusted` event (AC2). Auto-discovery polling
//! NEVER triggers same-SHA re-review: it always enqueues with
//! `AutoDiscovery` and always hits the dedup (AC2).
//!
//! This module is also the integration seam for future GitHub App
//! re-run controls (DAR §17): an App webhook handler calls
//! `enqueue_review_with_reason(..., ExplicitUserRequest)` directly,
//! bypassing the comment-scan listener entirely.
//!
//! Isolation tiers (mirrors #312's D9): per-PR errors (comment-list
//! HTTP, current-PR fetch, git fetch/merge-base, trust check, matcher)
//! log + count + continue to the next PR; step-level errors (rate
//! limit while listing comments, review-store write errors) return
//! `Err` so the tick folds them into `last_error`.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use tracing::{info, warn};
use url::Url;

use crate::daemon::tick::review_discovery::prepare_review_target;
use crate::github::issue::COMMENTS_PER_PAGE;
use crate::github::link_header::next_url_from_link_header;
use crate::github::pr::{fetch_pull_request, list_pull_requests, PullRequestDetail};
use crate::github::{Client, Response, ACCEPT_VALUE};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::repo::BareMirror;
use crate::review::sticky_comment::STICKY_MARKER_SEARCH_MAX_PAGES;
use crate::review::RepositoryId;
use crate::state::review::{EnqueueReason, ReviewEnqueueOutcome, ReviewStore};
use crate::worktree::git_runner::GitRunner;

// ---------------------------------------------------------------------------
// Event constants + emission shape (DAR §13)
// ---------------------------------------------------------------------------

/// DAR §13 rerun event: a trusted-author trigger comment matched and
/// the explicit re-review enqueue was attempted.
pub const RERUN_REQUESTED_EVENT: &str = "review_rerun_requested";
/// DAR §13 rerun event: a trigger comment from an author NOT on the
/// `feedback_author_allowlist` was ignored (AC2).
pub const RERUN_SKIPPED_UNTRUSTED_EVENT: &str = "review_rerun_skipped_untrusted";

/// Emit `review_rerun_requested` (trusted trigger → enqueue attempted).
fn emit_rerun_requested(repo: &str, pr: u64, author: &str, head_sha: &str) {
    info!(
        target: "caduceus",
        event = RERUN_REQUESTED_EVENT,
        repo = repo,
        pr = pr,
        author = author,
        head_sha = head_sha,
        "trusted comment requested an explicit re-review"
    );
}

/// Emit `review_rerun_skipped_untrusted` (trigger ignored, AC2).
fn emit_rerun_skipped_untrusted(repo: &str, pr: u64, author: &str, head_sha: &str) {
    info!(
        target: "caduceus",
        event = RERUN_SKIPPED_UNTRUSTED_EVENT,
        repo = repo,
        pr = pr,
        author = author,
        head_sha = head_sha,
        "rerun trigger ignored: author not on the feedback allowlist"
    );
}

// ---------------------------------------------------------------------------
// Pure matcher + trust check (Task 2, DAR §17 — no I/O, no logging)
// ---------------------------------------------------------------------------

/// Normalize one text line: trim surrounding whitespace, collapse
/// internal runs of whitespace to a single space, and fold case.
/// `split_whitespace` already drops leading/trailing/empty runs, so
/// the join is exactly "trim + collapse".
fn normalize_line(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// True when `body` contains a line that, after whitespace
/// normalization + case folding, equals `command`.
///
/// Exact-line match ONLY (DAR §17 decision): `/caduceus review please`
/// does NOT match — the line must equal the command after
/// normalization. This prevents matching inside a longer sentence
/// (no substring classification) and inside fenced code blocks that
/// happen to contain the command on their own line is still a match
/// (the body is plain text to us; a spoof that requires GitHub to
/// render differently is out of scope — the trust gate is the author
/// allowlist, not the renderer).
pub fn comment_matches_rerun_command(body: &str, command: &str) -> bool {
    let needle = normalize_line(command);
    if needle.is_empty() {
        return false;
    }
    body.lines().any(|line| normalize_line(line) == needle)
}

/// True when `author` is on the allowlist. Fail-closed: an EMPTY
/// allowlist trusts nobody. Mirrors `partition_comments`' filter
/// (`src/github/issue.rs`) so the two trust checks cannot diverge.
pub fn is_trusted_author(author: &str, allowlist: &[String]) -> bool {
    !allowlist.is_empty() && allowlist.iter().any(|a| a == author)
}

// ---------------------------------------------------------------------------
// Stats + budget (Task 4 fills the step function)
// ---------------------------------------------------------------------------

/// Per-tick cap on explicit re-review admissions. Explicit requests do
/// NOT consume `max_reviews_per_tick` (the auto-discovery budget, DAR
/// §5); this small constant prevents a flood of trigger comments from
/// overwhelming the worker pool. Phase-2 constant, not a config knob
/// (defer the knob until POC evidence).
pub const RERUN_PER_TICK_BUDGET: usize = 8;

/// Tick-wide rerun listener counters (mirrors `ReviewDiscoveryStats`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReviewRerunStats {
    pub repos_scanned: u32,
    pub prs_scanned: u32,
    pub trigger_matched: u32,
    pub enqueued: u32,
    pub skipped_untrusted: u32,
    pub skipped_no_trigger: u32,
    pub failed_prs: u32,
    pub failed_repos: u32,
    pub budget_exhausted: bool,
}

// ---------------------------------------------------------------------------
// Public test seams (D12 pattern)
// ---------------------------------------------------------------------------

/// Public test seam (D12): the structured `review_rerun_requested`
/// emitter, for event-capture tests.
pub fn emit_rerun_requested_for_tests(repo: &str, pr: u64, author: &str, head_sha: &str) {
    emit_rerun_requested(repo, pr, author, head_sha)
}

/// Public test seam (D12): the structured
/// `review_rerun_skipped_untrusted` emitter, for event-capture tests.
pub fn emit_rerun_skipped_untrusted_for_tests(repo: &str, pr: u64, author: &str, head_sha: &str) {
    emit_rerun_skipped_untrusted(repo, pr, author, head_sha)
}

// ---------------------------------------------------------------------------
// Comment listing (bounded pagination, §3.1)
// ---------------------------------------------------------------------------

/// One comment row from `/issues/{n}/comments` — only the fields the
/// rerun listener needs. Deliberately NOT `MarkerCommentWire`
/// (sticky_comment.rs) which lacks `user.login`.
#[derive(Debug, Deserialize)]
pub(crate) struct PrCommentWire {
    pub user: Option<PrCommentUserWire>,
    pub body: Option<String>,
}

/// The `user` object inside a comment row; `login` is `None` for
/// bots or deleted accounts where GitHub omits/nullifies `user`.
#[derive(Debug, Deserialize)]
pub(crate) struct PrCommentUserWire {
    pub login: Option<String>,
}

/// One decoded PR comment: author login + body.
#[derive(Debug, Clone)]
pub(crate) struct PrComment {
    pub author: String,
    pub body: String,
}

/// List a PR's issue comments (the same endpoint the sticky-marker
/// search uses), capped at [`STICKY_MARKER_SEARCH_MAX_PAGES`] pages.
/// Errors propagate to the per-PR tier of the listener.
pub(crate) async fn list_pr_comments(
    client: &Client,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> CaduceusResult<Vec<PrComment>> {
    let path =
        format!("/repos/{owner}/{repo}/issues/{pr_number}/comments?per_page={COMMENTS_PER_PAGE}");
    let mut page = 0usize;
    let mut url: Option<Url> = Some(join_api_path(client, &path));
    let mut all = Vec::new();
    while let Some(current) = url.take() {
        if page >= STICKY_MARKER_SEARCH_MAX_PAGES {
            return Err(CaduceusError::Other(format!(
                "rerun comment scan exceeded {STICKY_MARKER_SEARCH_MAX_PAGES} pages \
                 for {owner}/{repo}#{pr_number}"
            )));
        }
        page += 1;
        let response = client.get_url(&current, ACCEPT_VALUE).await?;
        let wire: Vec<PrCommentWire> = serde_json::from_slice(&response.body).map_err(|err| {
            CaduceusError::Other(format!("rerun comment scan page {page} JSON parse: {err}"))
        })?;
        for comment in wire {
            all.push(PrComment {
                author: comment.user.and_then(|u| u.login).unwrap_or_default(),
                body: comment.body.unwrap_or_default(),
            });
        }
        url = next_page_url(&response, page as u32 + 1);
    }
    Ok(all)
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
/// (mirrors `sticky_comment.rs`'s `next_page_url` so test fixtures
/// stay simple).
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

/// The row's advertised head SHA, or `""` when the wire row is
/// missing it (used for event fields on the untrusted-author path,
/// which does not fetch the current PR).
fn row_head_sha(row: &PullRequestDetail) -> &str {
    row.head
        .as_ref()
        .and_then(|h| h.sha.as_deref())
        .unwrap_or("")
}

// ---------------------------------------------------------------------------
// The listener step (Task 4, D9 isolation tiers)
// ---------------------------------------------------------------------------

/// Step 5.55 of the tick: trusted-comment re-review listener.
///
/// For each watched repo's open PRs (re-listed here — step 5.5 does
/// not expose its list), scan the PR's issue comments for the
/// configured `rerun_command`. A matching line from an allowlisted
/// author enqueues an explicit re-review of the CURRENT head SHA
/// (fetched via the single-PR endpoint at trigger time, DAR §17 — the
/// head may have moved since the comment was posted; the explicit
/// review is for the head at enqueue time, AC1). Untrusted authors
/// are ignored with `review_rerun_skipped_untrusted` (AC2).
///
/// Explicit requests do NOT consume `max_reviews_per_tick` (the
/// auto-discovery budget, DAR §5); they are capped at
/// [`RERUN_PER_TICK_BUDGET`] per tick (§3.5).
///
/// Returns `Err` ONLY for step-level failures (rate limit while
/// listing comments or pulls; review-store write errors). Everything
/// per-repo / per-PR is logged + counted (D9 isolation tiers).
pub(crate) async fn poll_rerun_step(
    repos: &[String],
    client: &Client,
    cfg: &Config,
    review_store: &ReviewStore,
    runner: &GitRunner,
    resolve_remote: &dyn Fn(&str, &str) -> CaduceusResult<String>,
) -> CaduceusResult<ReviewRerunStats> {
    let ar = match cfg.auto_review() {
        Some(ar) => ar,
        // Whole step gated on `auto_review.enabled` — trusted-comment
        // re-review is meaningless without the review engine running.
        None => return Ok(ReviewRerunStats::default()),
    };
    if !ar.enabled {
        return Ok(ReviewRerunStats::default());
    }

    let mut stats = ReviewRerunStats::default();
    // Lazy per-repo mirror cache (D7 pattern, same as discovery).
    let mut mirrors: BTreeMap<String, BareMirror> = BTreeMap::new();
    // Per-tick dedup: one enqueue per `(repo, pr)` per tick (§3.5).
    // The first-match-wins comment scan already limits one trigger per
    // PR; this guards the theoretical duplicate-row case and keeps the
    // explicit-path side effect (generation bump + publication re-arm)
    // to once per PR per tick. Not persisted — the next tick re-scans
    // by design (the PR may have a new head SHA by then).
    let mut enqueued_this_tick: HashSet<(String, u64)> = HashSet::new();

    for repo in repos {
        // Parse the `owner/repo` slug. Malformed slugs cannot happen
        // for validated config / API discovery, but a safety net keeps
        // the loop total.
        let Some((owner, name)) = repo.split_once('/') else {
            warn!(
                target: "caduceus",
                repo = repo,
                "review rerun: malformed repo slug; skipping"
            );
            stats.failed_repos += 1;
            continue;
        };

        let pulls = match list_pull_requests(client, owner, name).await {
            Ok(pulls) => pulls,
            // A global rate limit is a step-level condition — later
            // repos would fail identically, so surface it (D9).
            Err(err @ CaduceusError::RateLimited { .. }) => return Err(err),
            Err(err) => {
                warn!(
                    target: "caduceus",
                    error = %err,
                    repo = repo,
                    "review rerun: PR list failed; continuing to next repo"
                );
                stats.failed_repos += 1;
                continue;
            }
        };
        stats.repos_scanned += 1;

        for row in &pulls {
            // Budget check BEFORE any per-PR work so an exhausted
            // budget does no comment scanning (D10 pattern).
            if stats.enqueued >= RERUN_PER_TICK_BUDGET as u32 {
                stats.budget_exhausted = true;
                break;
            }
            let Some(pr_number) = row.number else {
                stats.failed_prs += 1;
                continue;
            };
            stats.prs_scanned += 1;

            // 1. Comment scan (bounded pagination, §3.1). Per-PR tier:
            //    HTTP errors log + count + continue.
            let comments = match list_pr_comments(client, owner, name, pr_number).await {
                Ok(comments) => comments,
                Err(err @ CaduceusError::RateLimited { .. }) => return Err(err),
                Err(err) => {
                    warn!(
                        target: "caduceus",
                        error = %err,
                        repo = repo,
                        pr = pr_number,
                        "review rerun: comment list failed; skipping PR"
                    );
                    stats.failed_prs += 1;
                    continue;
                }
            };

            // 2. First matching line wins (exact-line matcher, §3.2).
            let Some(trigger) = comments.iter().find_map(|c| {
                comment_matches_rerun_command(&c.body, &ar.rerun_command).then(|| c.author.clone())
            }) else {
                stats.skipped_no_trigger += 1;
                continue;
            };
            stats.trigger_matched += 1;

            // 3. Trust gate (§3.3): ONLY allowlisted authors; empty
            //    allowlist = no trusted triggers possible (fail-closed).
            if !is_trusted_author(&trigger, &cfg.feedback_author_allowlist) {
                emit_rerun_skipped_untrusted(repo, pr_number, &trigger, row_head_sha(row));
                stats.skipped_untrusted += 1;
                continue;
            }

            // 4. Resolve the CURRENT head/base at trigger time (AC1).
            //    A 404 (gone PR) surfaces as `Ok(None)` — log and skip.
            let Some(current) = fetch_pull_request(client, owner, name, pr_number).await? else {
                warn!(
                    target: "caduceus",
                    repo = repo,
                    pr = pr_number,
                    "review rerun: PR gone; skipping"
                );
                stats.failed_prs += 1;
                continue;
            };
            let (Some(head_sha), Some(base_sha), Some(base_ref)) = (
                current.head.as_ref().and_then(|h| h.sha.as_deref()),
                current.base.as_ref().and_then(|b| b.sha.as_deref()),
                current.base.as_ref().and_then(|b| b.ref_name.as_deref()),
            ) else {
                warn!(
                    target: "caduceus",
                    repo = repo,
                    pr = pr_number,
                    "review rerun: current PR row malformed; skipping"
                );
                stats.failed_prs += 1;
                continue;
            };

            // 5. Per-tick `(repo, pr)` dedup (§3.5).
            if !enqueued_this_tick.insert((repo.to_string(), pr_number)) {
                continue;
            }

            // 6. Lazy mirror bootstrap (D7): only when this repo has a
            //    trusted trigger that reached the enqueue stage.
            let mirror = match mirrors.get(repo) {
                Some(m) => m,
                None => {
                    let remote = match resolve_remote(owner, name) {
                        Ok(remote) => remote,
                        Err(err) => {
                            warn!(
                                target: "caduceus",
                                error = %err,
                                repo = repo,
                                "review rerun: remote resolve failed; skipping repo"
                            );
                            stats.failed_repos += 1;
                            break;
                        }
                    };
                    match BareMirror::ensure(runner, cfg, owner, name, &remote, base_ref).await {
                        Ok(m) => {
                            mirrors.insert(repo.to_string(), m);
                            mirrors.get(repo).expect("just inserted")
                        }
                        Err(err) => {
                            warn!(
                                target: "caduceus",
                                error = %err,
                                repo = repo,
                                "review rerun: mirror ensure failed; skipping repo"
                            );
                            stats.failed_repos += 1;
                            break;
                        }
                    }
                }
            };

            // 7. Prepare the target via the same mirror fetch +
            //    merge-base as auto-discovery (`prepare_review_target`)
            //    so the two admission paths cannot diverge. Git errors
            //    are per-PR: log + count + continue.
            let repository_id = RepositoryId {
                owner: owner.to_string(),
                repo: name.to_string(),
            };
            let target = match prepare_review_target(
                runner,
                mirror,
                &repository_id,
                pr_number,
                head_sha,
                base_sha,
                base_ref,
            )
            .await
            {
                Ok(target) => target,
                Err(err) => {
                    warn!(
                        target: "caduceus",
                        error = %err,
                        repo = repo,
                        pr = pr_number,
                        "review rerun: admission failed; skipping PR"
                    );
                    stats.failed_prs += 1;
                    continue;
                }
            };

            // 8. Explicit enqueue (dedup bypass, AC1/AC3). Store-write
            //    errors are step-level (D9).
            match review_store
                .enqueue_review_with_reason(&target, EnqueueReason::ExplicitUserRequest)
            {
                Ok(ReviewEnqueueOutcome::Inserted) => {
                    emit_rerun_requested(repo, pr_number, &trigger, head_sha);
                    stats.enqueued += 1;
                }
                Ok(ReviewEnqueueOutcome::AlreadyPresent) => {
                    // Unreachable on the explicit path (the bypass
                    // replaces the active entry), but keep the match
                    // total.
                    warn!(
                        target: "caduceus",
                        repo = repo,
                        pr = pr_number,
                        "review rerun: explicit enqueue reported already present"
                    );
                    stats.failed_prs += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
    Ok(stats)
}

/// Public test seam (D12): the listener step. Same body as
/// `poll_rerun_step`.
pub async fn poll_rerun_step_for_tests(
    repos: &[String],
    client: &Client,
    cfg: &Config,
    review_store: &ReviewStore,
    runner: &GitRunner,
    resolve_remote: &dyn Fn(&str, &str) -> CaduceusResult<String>,
) -> CaduceusResult<ReviewRerunStats> {
    poll_rerun_step(repos, client, cfg, review_store, runner, resolve_remote).await
}
