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

use tracing::info;

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
