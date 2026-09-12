//! Resume-safe review publication finalizer (DAR §9.1–9.4, §13 —
//! `docs/architecture/auto-review.md`; issue #310).
//!
//! The finalizer owns the publication FSM
//! `Pending → Publishing → Published | FailedRetryable` for the sticky
//! PR comment. The canonical [`crate::review::ReviewResult`] is durable
//! in review history (#295) **before any GitHub call**; publication is
//! presentation only. A restart mid-publish resumes from the persisted
//! `publication_state` — the model is NEVER re-run because publication
//! failed (DAR §9.1, AC1).
//!
//! # FSM step (`finalize_review`)
//!
//! 1. Generation guard (DAR §9.4): a completing run whose generation is
//!    older than the current `ReviewState.review_generation` is
//!    suppressed — recorded in history only, never published
//!    (`review_publication_suppressed_stale_generation`, AC2). The
//!    suppression is a non-transition: the current generation owns the
//!    presentation.
//! 2. Pre-state guards: `Published` → already final; `FailedRetryable`
//!    with a future `next_publish_at` → retry not yet due. No write, no
//!    GitHub call.
//! 3. Claim: `Pending|FailedRetryable|Publishing → Publishing` with
//!    `publication_attempt_count += 1`, persisted via
//!    `ReviewStore::save_review_state`. The store rejects
//!    generation-regressing writes, so a newer admission winning the
//!    race surfaces as `StateCorrupt` → `SkipConcurrent` (the CAS IS the
//!    generation guard).
//! 4. Publish via [`crate::review::sticky_comment::publish`] with the PR
//!    lifecycle from `poll_pr_merge_status` (DAR §9.3 gone-states A–D).
//! 5. Terminal save: success → `Published` (+ sticky comment id);
//!    retryable error → `FailedRetryable` with persisted exponential
//!    backoff (`next_publish_at`, `last_publish_error`); gone-states
//!    B/C and suppression → finalized-without-publication (see
//!    Terminal classification below).
//!
//! # Terminal classification (plan §6, Option 2)
//!
//! The publication enum has no `Skipped`/`FailedNonRetryable` variant
//! (deliberate — the issue's scope does not add states). Permanently
//! unpublishable outcomes (PR-404, closed-unmerged, stale-generation,
//! non-success result) finalize as `publication_state := Published` —
//! "finalization complete, nothing to show" — with the reason recorded
//! in `last_publish_error` (`pr_not_found`, `closed_unmerged`,
//! `suppressed_stale_generation`, `no_publishable_result`). Chosen over
//! leaving the row at `Publishing` because the due-scan is
//! history-driven: a `Publishing` row with no pending terminal save is
//! precisely the crash-recovery case, and the two must not be
//! confusable.
//!
//! # Crash recovery (AC5)
//!
//! `publication_state` IS the resume checkpoint: the poll re-enters any
//! non-terminal row whose retry debt elapsed (`Publishing` with no
//! backoff = a crashed claim). Re-publication is idempotent: the sticky
//! comment is byte-identical (#308 `Unchanged`), and a create whose id
//! was never persisted self-heals via marker adoption
//! ([`crate::review::sticky_comment::find_sticky_comment_by_marker`]).
//! A crash after publish but before the terminal save therefore never
//! duplicates the comment.
//!
//! # Boundaries
//!
//! - Dispatch routing / skip routing is #339's: the tick's poll step
//!   (`daemon::tick::review_finalize_step`) scans the store; #339 will
//!   own completion-driven wakeups.
//! - Single-daemon assumption: concurrent daemons are bounded by the
//!   CAS + #308 idempotency (worst case a wasted API call, never a
//!   duplicate comment), but scheduling is not coordinated.
//! - `enqueue_review` re-arms publication on a new admission: bumping
//!   `review_generation` resets `publication_state` to `Pending` (and
//!   clears the stale publication-retry debt) so the next generation's
//!   completion can publish. Without the reset, a `Published` row from
//!   generation N would permanently block generation N+1.

use chrono::{DateTime, Utc};
use tracing::info;

use crate::github::merge_detect::poll_pr_merge_status;
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::review::sticky_comment::{publish, RenderInput, StickyOutcome};
use crate::review::{
    parse_review_result, PublicationState, RepositoryId, ReviewState, MAX_PUBLISH_ERROR_BYTES,
};
use crate::state::review::ReviewStore;

// ---------------------------------------------------------------------------
// Events (DAR §13 — owned by #310)
// ---------------------------------------------------------------------------

/// DAR §13 finalizer event: the publication attempt began (after the
/// claim, before any GitHub call).
pub const EVENT_PUBLISH_STARTED: &str = "review_publish_started";
/// DAR §13 finalizer event: the sticky comment was published or
/// idempotently confirmed.
pub const EVENT_PUBLISHED: &str = "review_published";
/// DAR §13 finalizer event: publication failed with persisted backoff.
pub const EVENT_PUBLISH_FAILED_RETRYABLE: &str = "review_publish_failed_retryable";
/// DAR §13/§9.4 event: a stale-generation publication was suppressed.
pub const EVENT_SUPPRESSED_STALE_GENERATION: &str =
    "review_publication_suppressed_stale_generation";
/// Finalizer event: PR closed without merge at finalization time —
/// quiet skip + structured event (DAR §9.3 row C).
pub const EVENT_SKIPPED_PR_CLOSED_UNMERGED: &str = "review_skipped_pr_closed_unmerged";

fn emit_publish_started(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = EVENT_PUBLISH_STARTED,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "review publication started"
    );
}

fn emit_published(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = EVENT_PUBLISHED,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "review published to the sticky PR comment"
    );
}

fn emit_publish_failed_retryable(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = EVENT_PUBLISH_FAILED_RETRYABLE,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "review publication failed; retry scheduled with backoff"
    );
}

fn emit_suppressed_stale_generation(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = EVENT_SUPPRESSED_STALE_GENERATION,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "stale-generation publication suppressed; result remains in history"
    );
}

fn emit_skipped_pr_closed_unmerged(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = EVENT_SKIPPED_PR_CLOSED_UNMERGED,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "publication skipped: PR closed without merge"
    );
}

// ---------------------------------------------------------------------------
// Terminal sentinels (Option 2 classification — see module docs)
// ---------------------------------------------------------------------------

/// `last_publish_error` sentinel: PR lookup 404 (DAR §9.3 row B).
pub const PUBLISH_ERROR_PR_NOT_FOUND: &str = "pr_not_found";
/// `last_publish_error` sentinel: PR closed without merge (§9.3 row C).
pub const PUBLISH_ERROR_CLOSED_UNMERGED: &str = "closed_unmerged";
/// `last_publish_error` sentinel: stale-generation suppression (§9.4).
pub const PUBLISH_ERROR_SUPPRESSED_STALE: &str = "suppressed_stale_generation";
/// `last_publish_error` sentinel: the durable result carries no
/// publishable review (non-success execution result).
pub const PUBLISH_ERROR_NO_PUBLISHABLE_RESULT: &str = "no_publishable_result";

// ---------------------------------------------------------------------------
// Due-scan types
// ---------------------------------------------------------------------------

/// One `(repo, pr)` finalization the poll should run: the completing
/// run's identity and the generation to guard against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueFinalization {
    /// Repository the PR lives in.
    pub repository: RepositoryId,
    /// PR number.
    pub pull_request: u64,
    /// The completing run's `review_generation` — the value the guard
    /// compares against the current `ReviewState.review_generation`.
    pub run_generation: u64,
    /// Head SHA of the completing run (the history-row lookup key).
    pub head_sha: String,
}

/// What one [`finalize_review`] step decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// No state row / no durable result for the run — nothing to
    /// finalize. Quiet: no event, no write.
    NoState,
    /// `publication_state` was already `Published` (final or
    /// finalized-without-publication). No write, no GitHub call.
    AlreadyFinal,
    /// `FailedRetryable` whose `next_publish_at` is still in the
    /// future. No write, no GitHub call.
    SkipRetryNotDue,
    /// The claim's CAS save lost to a newer admission
    /// (generation regression). No GitHub call.
    SkipConcurrent,
    /// Stale generation (pre-publish guard or post-publish CAS loss):
    /// history only, presentation untouched by this run.
    SuppressedStaleGeneration,
    /// Comment created/updated; `sticky_comment_id` persisted.
    Published,
    /// Byte-identical re-publication confirmed (`Unchanged`); no PATCH.
    PublishedUnchanged,
    /// Gone-state A recovery: comment adopted/recreated via marker
    /// search; new id persisted.
    PublishedRecreated,
    /// Gone-state B: PR 404 — quiet skip, never recreate (no event).
    PrGone,
    /// Gone-state C: closed-unmerged — quiet skip + event.
    SkippedClosedUnmerged,
    /// Publication failed; retry scheduled with persisted backoff.
    FailedRetryable {
        /// When the next publication attempt may run.
        next_attempt_at: DateTime<Utc>,
    },
}

// ---------------------------------------------------------------------------
// Backoff (plan §5: min(2^min(n,12) * 30s, 1h))
// ---------------------------------------------------------------------------

/// Publication backoff for the n-th attempt: `min(2^min(n,12) * 30s,
/// 3600s)` — 60s, 2m, 4m, 8m, 16m, 32m, 1h cap. `n` is
/// `publication_attempt_count` (publication retries — never the worker
/// attempt counter, DAR §9.1).
pub fn backoff_delay(publication_attempt_count: u32) -> chrono::Duration {
    let shift = publication_attempt_count.min(12);
    let secs = (1u64 << shift) * 30;
    chrono::Duration::seconds((secs.min(3600)) as i64)
}

/// Char-boundary-safe truncation for `last_publish_error` (cap is the
/// validator's `MAX_PUBLISH_ERROR_BYTES`).
fn truncate_error(msg: &str) -> String {
    if msg.len() <= MAX_PUBLISH_ERROR_BYTES {
        return msg.to_string();
    }
    let mut end = MAX_PUBLISH_ERROR_BYTES;
    while end > 0 && !msg.is_char_boundary(end) {
        end -= 1;
    }
    msg[..end].to_string()
}

// ---------------------------------------------------------------------------
// Claim (Pending|FailedRetryable|Publishing → Publishing, CAS-guarded)
// ---------------------------------------------------------------------------

/// Persist the `Publishing` claim for `state`:
/// `publication_state := Publishing`, `publication_attempt_count += 1`,
/// retry debt cleared. The equal-generation write is allowed by
/// `save_review_state`; a generation-regression rejection (a newer
/// admission won the race) returns `Ok(false)` — the caller must not
/// publish. Any other store error propagates.
pub fn claim_for_publication(store: &ReviewStore, state: &ReviewState) -> CaduceusResult<bool> {
    let mut claimed = state.clone();
    claimed.publication_state = PublicationState::Publishing;
    claimed.publication_attempt_count = claimed.publication_attempt_count.saturating_add(1);
    claimed.next_publish_at = None;
    match store.save_review_state(&claimed) {
        Ok(()) => Ok(true),
        Err(CaduceusError::StateCorrupt { .. }) => Ok(false),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// The FSM step
// ---------------------------------------------------------------------------

/// Run one publication-finalization step for a due finalization (the
/// FSM core, plan §5). Guards run before any write; no GitHub call
/// happens before the claim succeeds.
pub async fn finalize_review(
    client: &Client,
    cfg: &Config,
    store: &ReviewStore,
    due: &DueFinalization,
    now: DateTime<Utc>,
) -> CaduceusResult<FinalizeOutcome> {
    let repo_full = due.repository.full_name();
    let owner = due.repository.owner.as_str();
    let repo_name = due.repository.repo.as_str();

    let Some(mut state) = store.review_state(&due.repository, due.pull_request)? else {
        return Ok(FinalizeOutcome::NoState);
    };

    // Generation guard (DAR §9.4, AC2). `run_generation >
    // review_generation` is impossible under the monotonic admission
    // CAS — treat as NoState (corrupt) rather than publishing.
    if due.run_generation > state.review_generation {
        return Ok(FinalizeOutcome::NoState);
    }
    if due.run_generation < state.review_generation {
        emit_suppressed_stale_generation(&repo_full, due.pull_request, &due.head_sha);
        return Ok(FinalizeOutcome::SuppressedStaleGeneration);
    }

    // Pre-state guards: no write, no GitHub call.
    match state.publication_state {
        PublicationState::Published => return Ok(FinalizeOutcome::AlreadyFinal),
        PublicationState::FailedRetryable
            if state.next_publish_at.map(|t| t > now).unwrap_or(false) =>
        {
            return Ok(FinalizeOutcome::SkipRetryNotDue);
        }
        _ => {}
    }

    // Durable-result-before-publication invariant: the canonical result
    // must already be in history. Nothing here re-runs the model.
    let rows = store.history_for_head_sha(&due.repository, due.pull_request, &due.head_sha)?;
    let Some(row) = rows.last() else {
        return Ok(FinalizeOutcome::NoState);
    };
    let result = parse_review_result(&row.result_json)?;
    let Some(review) = result.review else {
        // Non-success execution result: nothing to publish. Finalize
        // quietly (terminal sentinel) — retrying could not help; the
        // result stays in history.
        finalize_without_publication(
            store,
            &mut state,
            &row.head_sha,
            &row.review_run_id,
            PUBLISH_ERROR_NO_PUBLISHABLE_RESULT,
            now,
        )?;
        return Ok(FinalizeOutcome::NoState);
    };

    // Claim (CAS vehicle for the generation guard under races).
    if !claim_for_publication(store, &state)? {
        return Ok(FinalizeOutcome::SkipConcurrent);
    }
    state.publication_state = PublicationState::Publishing;
    state.publication_attempt_count = state.publication_attempt_count.saturating_add(1);
    state.next_publish_at = None;

    emit_publish_started(&repo_full, due.pull_request, &due.head_sha);

    // PR lifecycle classification (DAR §9.3) — first GitHub call.
    let pr_state = match poll_pr_merge_status(client, owner, repo_name, due.pull_request).await {
        Ok(pr_state) => pr_state,
        Err(err) => {
            return save_retryable(store, &mut state, due, &err, now);
        }
    };

    let input = RenderInput {
        review: &review,
        reviewed_head_sha: &due.head_sha,
        current_head_sha: None,
        // The §9.4 guard above already guarantees
        // `due.run_generation == state.review_generation` here, so this
        // reads the completing run's generation (issue #393).
        review_generation: due.run_generation,
    };
    let sticky = match publish(
        client,
        cfg,
        owner,
        repo_name,
        due.pull_request,
        &state,
        &input,
        pr_state,
    )
    .await
    {
        Ok(sticky) => sticky,
        Err(err) => {
            return save_retryable(store, &mut state, due, &err, now);
        }
    };

    match sticky {
        StickyOutcome::Published { comment_id } => {
            finalize_published(
                store,
                &mut state,
                &review,
                &row.head_sha,
                &row.review_run_id,
                comment_id,
                now,
            )?;
            emit_published(&repo_full, due.pull_request, &due.head_sha);
            Ok(FinalizeOutcome::Published)
        }
        StickyOutcome::Unchanged { comment_id } => {
            finalize_published(
                store,
                &mut state,
                &review,
                &row.head_sha,
                &row.review_run_id,
                comment_id,
                now,
            )?;
            emit_published(&repo_full, due.pull_request, &due.head_sha);
            Ok(FinalizeOutcome::PublishedUnchanged)
        }
        StickyOutcome::CommentGoneRecreated { new_comment_id } => {
            finalize_published(
                store,
                &mut state,
                &review,
                &row.head_sha,
                &row.review_run_id,
                new_comment_id,
                now,
            )?;
            emit_published(&repo_full, due.pull_request, &due.head_sha);
            Ok(FinalizeOutcome::PublishedRecreated)
        }
        StickyOutcome::PrNotFound => {
            // Gone-state B: quiet skip, no event, never recreate.
            finalize_without_publication(
                store,
                &mut state,
                &row.head_sha,
                &row.review_run_id,
                PUBLISH_ERROR_PR_NOT_FOUND,
                now,
            )?;
            Ok(FinalizeOutcome::PrGone)
        }
        StickyOutcome::PrClosedUnmerged => {
            // Gone-state C: quiet skip + event; history remains.
            emit_skipped_pr_closed_unmerged(&repo_full, due.pull_request, &due.head_sha);
            finalize_without_publication(
                store,
                &mut state,
                &row.head_sha,
                &row.review_run_id,
                PUBLISH_ERROR_CLOSED_UNMERGED,
                now,
            )?;
            Ok(FinalizeOutcome::SkippedClosedUnmerged)
        }
        StickyOutcome::PrMergedSuppressed | StickyOutcome::SuppressedStaleGeneration => {
            // Defensive mapping (publish itself never returns these —
            // see sticky_comment.rs); treated as the §9.4 suppression.
            emit_suppressed_stale_generation(&repo_full, due.pull_request, &due.head_sha);
            finalize_without_publication(
                store,
                &mut state,
                &row.head_sha,
                &row.review_run_id,
                PUBLISH_ERROR_SUPPRESSED_STALE,
                now,
            )?;
            Ok(FinalizeOutcome::SuppressedStaleGeneration)
        }
    }
}

/// Terminal save for a published comment: `Published` + sticky id,
/// retry debt cleared, review observations recorded, event emitted by
/// the caller. A CAS regression on the save (a newer admission reset
/// the row mid-publish) leaves the state to the newer generation and
/// reports the suppression — the GitHub call already happened; #308
/// byte-identical idempotency keeps any later re-publish duplicate-free.
fn finalize_published(
    store: &ReviewStore,
    state: &mut ReviewState,
    review: &crate::review::Review,
    head_sha: &str,
    run_id: &str,
    comment_id: u64,
    now: DateTime<Utc>,
) -> CaduceusResult<()> {
    state.publication_state = PublicationState::Published;
    state.sticky_comment_id = Some(comment_id);
    state.next_publish_at = None;
    state.last_publish_error = None;
    state.last_reviewed_head_sha = Some(head_sha.to_string());
    state.last_verdict = Some(review.verdict);
    state.last_reviewed_at = Some(now);
    state.last_run_id = Some(run_id.to_string());
    match store.save_review_state(state) {
        Ok(()) => Ok(()),
        Err(err @ CaduceusError::StateCorrupt { .. }) => {
            tracing::warn!(
                error = %err,
                repo = state.repository.full_name(),
                pr = state.pull_request,
                "terminal publication save lost the generation race; \
                 the newer admission owns the presentation"
            );
            // The comment WAS published; report the race without
            // failing the step.
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Terminal save for a finalization that completes WITHOUT publishing
/// (Option 2 sentinel — see module docs): `Published` state marker +
/// the reason in `last_publish_error`. Review observations are still
/// recorded: the review completed; only its presentation is absent.
fn finalize_without_publication(
    store: &ReviewStore,
    state: &mut ReviewState,
    head_sha: &str,
    run_id: &str,
    sentinel: &str,
    now: DateTime<Utc>,
) -> CaduceusResult<()> {
    state.publication_state = PublicationState::Published;
    state.next_publish_at = None;
    state.last_publish_error = Some(sentinel.to_string());
    state.last_reviewed_head_sha = Some(head_sha.to_string());
    state.last_reviewed_at = Some(now);
    state.last_run_id = Some(run_id.to_string());
    match store.save_review_state(state) {
        Ok(()) => Ok(()),
        Err(err @ CaduceusError::StateCorrupt { .. }) => {
            tracing::warn!(
                error = %err,
                repo = state.repository.full_name(),
                pr = state.pull_request,
                "finalization save lost the generation race; \
                 the newer admission owns the state"
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Retryable-failure save (AC1, AC4): `FailedRetryable` + persisted
/// exponential backoff + truncated error. The model is never re-run.
fn save_retryable(
    store: &ReviewStore,
    state: &mut ReviewState,
    due: &DueFinalization,
    err: &CaduceusError,
    now: DateTime<Utc>,
) -> CaduceusResult<FinalizeOutcome> {
    let next_attempt_at = now + backoff_delay(state.publication_attempt_count);
    state.publication_state = PublicationState::FailedRetryable;
    state.next_publish_at = Some(next_attempt_at);
    state.last_publish_error = Some(truncate_error(&err.to_string()));
    match store.save_review_state(state) {
        Ok(()) => {}
        Err(err @ CaduceusError::StateCorrupt { .. }) => {
            tracing::warn!(
                error = %err,
                repo = due.repository.full_name(),
                pr = due.pull_request,
                "retry save lost the generation race; the newer \
                 admission owns the publication state"
            );
            return Ok(FinalizeOutcome::SkipConcurrent);
        }
        Err(err) => return Err(err),
    }
    emit_publish_failed_retryable(&due.repository.full_name(), due.pull_request, &due.head_sha);
    Ok(FinalizeOutcome::FailedRetryable { next_attempt_at })
}
