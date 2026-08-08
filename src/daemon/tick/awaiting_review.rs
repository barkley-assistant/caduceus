use super::Outcome304;

use chrono::{DateTime, Utc};
use tracing::info;

use crate::daemon::orchestration::{ActiveRunGuard, FailureClass};
use crate::github::poll::{merge_outcomes, poll_code, poll_investigation};
use crate::github::{Client, RateLimitInfo};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::state::meta::{CadenceGate, MetaStore, TickOutcome};
use crate::state::queue::{Phase, QueueEntry, StateStore};

// Awaiting-review poller — checks PR merge status for entries in
// AwaitingReview phase and applies transitions.

/// Scan the queue for entries in [`Phase::AwaitingReview`] and poll
/// each entry's PR merge status. Applies transitions:
///
/// * PR merged → `Done` (via `store.complete`)
/// * PR closed without merge → `NeedsAttention` (via `store.route_to_needs_attention`)
/// * PR still open → no-op
///
/// The function is best-effort: a single failed poll does not block
/// the rest of the scan. Per-entry errors are logged and collected.
pub(crate) async fn poll_awaiting_review_entries(
    store: &StateStore,
    client: &Client,
) -> CaduceusResult<()> {
    let snap = store.snapshot()?;
    let awaiting: Vec<QueueEntry> = snap
        .entries
        .values()
        .filter(|e| e.phase == Phase::AwaitingReview)
        .filter(|e| {
            // Only poll entries that have a finalization checkpoint
            // with a PR number.
            e.finalization.as_ref().and_then(|f| f.pr_number).is_some()
        })
        .cloned()
        .collect();

    for entry in &awaiting {
        let key = &entry.key;
        let pr_number = entry
            .finalization
            .as_ref()
            .and_then(|f| f.pr_number)
            .expect("filtered above");

        match crate::github::merge_detect::poll_pr_merge_status(
            client, &key.owner, &key.repo, pr_number,
        )
        .await
        {
            Ok(crate::github::merge_detect::MergeStatus::Merged { .. }) => {
                info!(
                issue = %key.display_key(),
                pr = %pr_number,
                "PR merged; transitioning to Done"
                );
                if let Err(err) = store.resolve_awaiting_review_as_done(key) {
                    tracing::warn!(
                    error = %err,
                    issue = %key.display_key(),
                    "failed to mark merged PR as Done"
                    );
                }
            }
            Ok(crate::github::merge_detect::MergeStatus::ClosedWithoutMerge) => {
                info!(
                issue = %key.display_key(),
                pr = %pr_number,
                "PR closed without merge; routing to NeedsAttention"
                );
                if let Err(err) = store.route_to_needs_attention(
                    key,
                    &format!("PR #{pr_number} was closed without merge — operator must inspect"),
                ) {
                    tracing::warn!(
                    error = %err,
                    issue = %key.display_key(),
                    "failed to route closed PR to NeedsAttention"
                    );
                }
            }
            Ok(
                crate::github::merge_detect::MergeStatus::StillOpen
                | crate::github::merge_detect::MergeStatus::NotFound,
            ) => {
                // Still waiting for human review, or PR not found yet.
                // No-op.
            }
            Err(err) => {
                tracing::warn!(
                error = %err,
                issue = %key.display_key(),
                "failed to poll PR merge status"
                );
            }
        }
    }

    Ok(())
}

pub(crate) async fn poll_repo(
    slug: &str,
    client: &Client,
    cfg: &Config,
    store: &StateStore,
) -> CaduceusResult<Outcome304> {
    let repos: Vec<String> = vec![slug.to_string()];
    let code = poll_code(client, cfg, &repos).await?;
    let inv = poll_investigation(client, cfg, &repos).await?;
    let merged = merge_outcomes(code, inv);
    enqueue_summaries(store, &merged.summaries, cfg.dry_run)?;
    Ok(Outcome304(merged.from_cache))
}

pub fn enqueue_summaries(
    store: &StateStore,
    summaries: &[crate::github::poll::IssueSummary],
    dry_run: bool,
) -> CaduceusResult<Option<DateTime<Utc>>> {
    let mut earliest: Option<DateTime<Utc>> = None;
    for summary in summaries {
        let _outcome = store.enqueue(&summary.key, summary.ticket_type, dry_run)?;
        // The enqueue outcome is a binary inserted/already/promoted
        // signal; the backoff window is whatever the entry's
        // existing `next_attempt_at` carries.
        if let Some(entry) = store
            .snapshot()
            .ok()
            .and_then(|s| s.entry(&summary.key).cloned())
        {
            if let Some(b) = entry.next_attempt_at {
                earliest = Some(match earliest {
                    Some(e) => e.min(b),
                    None => b,
                });
            }
        }
    }
    Ok(earliest)
}

pub(crate) async fn handle_infra_or_retry(
    cfg: Config,
    guard: &mut ActiveRunGuard,
    err: &CaduceusError,
    class: FailureClass,
) -> CaduceusResult<TickOutcome> {
    if class.counts_against_retry_budget() {
        let new_phase = guard
            .finish_retry(&err.to_string(), cfg.max_retries_per_issue)
            .await?;
        return Ok(map_phase_to_outcome(new_phase));
    }
    let now = Utc::now();
    let not_before = now + chrono::Duration::seconds(cfg.retry_backoff_seconds as i64);
    let _ = guard
        .finish_infrastructure(&err.to_string(), not_before)
        .await;
    Ok(outcome_for_class(class))
}

pub(crate) fn outcome_for_class(class: FailureClass) -> TickOutcome {
    match class {
        FailureClass::RateLimit { .. } => TickOutcome::RateLimited,
        FailureClass::Cancellation => TickOutcome::Cancelled,
        _ => TickOutcome::Failed,
    }
}

pub(crate) fn map_phase_to_outcome(phase: Phase) -> TickOutcome {
    match phase {
        Phase::Queued
        | Phase::InProgress
        | Phase::Previewed
        | Phase::AwaitingReview
        | Phase::Done
        | Phase::Skipped => TickOutcome::Processed,
        Phase::Failed => TickOutcome::Failed,
        Phase::NeedsAttention => TickOutcome::Failed,
    }
}

pub(crate) fn extract_http_status(err: &CaduceusError) -> Option<u16> {
    match err {
        CaduceusError::GitHubApi { status, .. } => Some(*status),
        _ => None,
    }
}

pub(crate) fn finish_tick_outcome(
    gate: &CadenceGate,
    _meta: &MetaStore,
    now: DateTime<Utc>,
    outcome: TickOutcome,
    http_status: Option<u16>,
    last_error: Option<&CaduceusError>,
) -> CaduceusResult<()> {
    let _ = _meta;
    gate.record_tick_finished(
        now,
        outcome,
        http_status,
        0,
        None,
        last_error.map(|e| format!("{e}")),
    )
}

pub(crate) fn finish_tick_failure(
    gate: &CadenceGate,
    now: DateTime<Utc>,
    cfg: &Config,
    meta: &MetaStore,
    class: FailureClass,
    last_error: Option<&CaduceusError>,
) -> CaduceusResult<()> {
    let _ = meta;
    let outcome = match class {
        FailureClass::RateLimit { .. } => TickOutcome::RateLimited,
        FailureClass::Cancellation => TickOutcome::Cancelled,
        _ => TickOutcome::Failed,
    };
    let _ = cfg;
    // The rate-limit observation is the input to the
    // next tick's `CadenceGate::precheck` and must be
    // persisted *before* the tick returns. The
    // orchestrator's `tick` body itself does not always
    // pass the rate-limit info to `record_tick_finished`;
    // we extract the observation from the last error here
    // when the failure class is `RateLimit` so the gate
    // can record it via `record_tick_finished`.
    let rate_limit_info: Option<RateLimitInfo> = match (class, last_error) {
        (
            FailureClass::RateLimit { .. },
            Some(CaduceusError::RateLimited {
                reset_at,
                remaining,
                limit,
            }),
        ) => Some(RateLimitInfo {
            remaining: *remaining,
            limit: *limit,
            observed_at: now,
            reset_at_unix: now.timestamp().saturating_add(*reset_at as i64),
        }),
        _ => None,
    };
    gate.record_tick_finished(
        now,
        outcome,
        None,
        cfg.poll_interval_seconds,
        rate_limit_info.as_ref(),
        last_error.map(|e| format!("{e}")),
    )
}

pub(crate) fn dummy_rate_limit_info(
    obs: &crate::state::meta::RateLimitObservation,
) -> RateLimitInfo {
    RateLimitInfo {
        remaining: obs.remaining,
        limit: obs.limit,
        observed_at: obs.observed_at,
        reset_at_unix: obs.reset_at.timestamp(),
    }
}

pub(crate) fn exit_code_for(outcome: &TickOutcome) -> u8 {
    match outcome {
        TickOutcome::Processed => 0,
        TickOutcome::Idle304 | TickOutcome::IdleEmpty => 0,
        TickOutcome::SkippedConcurrent => 0,
        TickOutcome::SkippedCadence => 0,
        TickOutcome::RateLimited => 0,
        TickOutcome::Cancelled => 0,
        TickOutcome::Failed => 1,
    }
}

/// Test seam: re-export the exit-code mapping so integration
/// tests can assert the cron-tick contract without owning a
/// runtime. Identical to the private [`exit_code_for`].
pub fn exit_code_for_tests(outcome: &TickOutcome) -> u8 {
    exit_code_for(outcome)
}

/// Test seam: re-export the failure-class→outcome mapping so
/// integration tests can assert the mapping without owning a
/// runtime. Identical to the private [`outcome_for_class`].
pub fn outcome_for_class_for_tests(class: FailureClass) -> TickOutcome {
    outcome_for_class(class)
}

/// Test seam: re-export the phase→outcome mapping so integration
/// tests can assert the mapping without owning a runtime.
/// Identical to the private [`map_phase_to_outcome`].
pub fn map_phase_to_outcome_for_tests(phase: Phase) -> TickOutcome {
    map_phase_to_outcome(phase)
}

/// Test seam: re-export the HTTP-status extraction so integration
/// tests can assert the mapping without owning a runtime.
/// Identical to the private [`extract_http_status`].
pub fn extract_http_status_for_tests(err: &CaduceusError) -> Option<u16> {
    extract_http_status(err)
}
