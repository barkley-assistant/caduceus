//! In-tick review publication finalizer poll (issue #310, DAR §9.1).
//!
//! Step 5.6 of the tick pipeline (after PR discovery, before the issue
//! drain): scan the review store for due finalizations
//! ([`crate::state::review::ReviewStore::due_finalizations`]) and run
//! the publication FSM ([`crate::review::finalize_review`]) for each.
//!
//! Isolation mirrors #312's discovery step: per-entry failures are
//! logged + counted + skipped (one bad entry never aborts the poll);
//! the step itself never returns `Err` for a per-entry condition and
//! never aborts the tick. Dispatch routing / completion-driven wakeups
//! remain #339's — this module only polls.

use tracing::warn;

use crate::infra::error::CaduceusResult;
use crate::review::{finalize_review, FinalizeOutcome};
use crate::state::review::ReviewStore;

/// Tick-wide finalizer counters (logged at step end).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublicationStats {
    /// Entries the poll attempted to finalize.
    pub attempted: u32,
    /// Publications (created, updated, or idempotently confirmed).
    pub published: u32,
    /// Stale-generation suppressions (§9.4).
    pub suppressed: u32,
    /// Quiet skips (PR-404, closed-unmerged, non-success results) and
    /// pre-state guard passes (already-final, retry-not-due,
    /// concurrent-CAS loss, no state).
    pub skipped: u32,
    /// Retryable failures with persisted backoff.
    pub failed_retryable: u32,
    /// Per-entry errors (store/parse failures) — logged + counted.
    pub errors: u32,
}

fn fold_outcome(stats: &mut PublicationStats, outcome: FinalizeOutcome) {
    match outcome {
        FinalizeOutcome::Published
        | FinalizeOutcome::PublishedUnchanged
        | FinalizeOutcome::PublishedRecreated => stats.published += 1,
        FinalizeOutcome::SuppressedStaleGeneration => stats.suppressed += 1,
        FinalizeOutcome::FailedRetryable { .. } => stats.failed_retryable += 1,
        FinalizeOutcome::NoState
        | FinalizeOutcome::AlreadyFinal
        | FinalizeOutcome::SkipRetryNotDue
        | FinalizeOutcome::SkipConcurrent
        | FinalizeOutcome::PrGone
        | FinalizeOutcome::SkippedClosedUnmerged => stats.skipped += 1,
    }
}

/// Step 5.6: finalize due review publications. Per-entry isolation —
/// one bad entry is logged + counted; the poll continues.
pub(crate) async fn poll_publication_step(
    client: &crate::github::Client,
    cfg: &crate::infra::config::Config,
    review_store: &ReviewStore,
) -> CaduceusResult<PublicationStats> {
    let due = review_store.due_finalizations()?;
    let mut stats = PublicationStats::default();
    let now = chrono::Utc::now();
    for entry in &due {
        match finalize_review(client, cfg, review_store, entry, now).await {
            Ok(outcome) => {
                stats.attempted += 1;
                fold_outcome(&mut stats, outcome);
            }
            Err(err) => {
                warn!(
                    target: "caduceus",
                    error = %err,
                    repo = entry.repository.full_name(),
                    pr = entry.pull_request,
                    head_sha = entry.head_sha,
                    "review finalization failed; continuing"
                );
                stats.errors += 1;
            }
        }
    }
    Ok(stats)
}

/// Test seam: same loop as [`poll_publication_step`] against an
/// explicit config (mirrors #312's `poll_review_step` seam). Errors are
/// counted, never propagated, so one bad entry cannot fail a test's
/// surrounding assertions.
pub async fn poll_publication_step_for_tests(
    client: &crate::github::Client,
    cfg: &crate::infra::config::Config,
    review_store: &ReviewStore,
) -> CaduceusResult<PublicationStats> {
    let due = review_store.due_finalizations()?;
    let mut stats = PublicationStats::default();
    let now = chrono::Utc::now();
    for entry in &due {
        stats.attempted += 1;
        match finalize_review(client, cfg, review_store, entry, now).await {
            Ok(outcome) => fold_outcome(&mut stats, outcome),
            Err(_) => stats.errors += 1,
        }
    }
    Ok(stats)
}
