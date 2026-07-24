#![allow(dead_code, unused_imports)]
use super::*;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::info;
use ulid::Ulid;

use crate::daemon::orchestration::{
    classify_error, ActiveRunGuard, FailureClass, Services, SystemClock,
};
use crate::finalize::{
    archive_worker_result, commit_code_and_finalize, dry_run_finalize,
    find_or_create_pr_and_finalize, post_completion_only, post_investigation_comment_and_finalize,
    push_and_finalize, FinalizeContext, FinalizeOutput, FinalizeRequest,
};
use crate::github::poll::{discover_watched_repos, merge_outcomes, poll_code, poll_investigation};
use crate::github::{Client, RateLimitInfo, Response};
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::logging;
use crate::scheduler::circuit::{AdmissionResult, CircuitConfig, CircuitStore};
use crate::scheduler::{DrainConfig, LeaderToken, Pool};
use crate::signals;
use crate::state::checkpoints::{last_checkpoint_for_run, persist_checkpoint};
use crate::state::meta::{CadenceDecision, CadenceGate, MetaStore, TickOutcome};
use crate::state::queue::{ClaimedEntry, Phase, QueueEntry, StateStore, TicketType};
use crate::state::store;
use crate::worker::context::{build_context, encode_context, BuildInputs};
use crate::worker::prompt::{build_prompt, write_prompt};
use crate::worker::WorkerResult;
use crate::worktree::{create as create_worktree, find_main_clone, GitRunner};

// ---------------------------------------------------------------------------
// Awaiting-review poller — checks PR merge status for entries in
// AwaitingReview phase and applies transitions.
// ---------------------------------------------------------------------------

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
    meta: &MetaStore,
) -> CaduceusResult<Outcome304> {
    let repos: Vec<String> = vec![slug.to_string()];
    let code = poll_code(client, cfg, &repos).await?;
    let inv = poll_investigation(client, cfg, &repos).await?;
    let merged = merge_outcomes(code, inv);
    let _ = Response {
        status: 200,
        final_url: format!("https://api.github.com/repos/{slug}/issues"),
        body: Vec::new(),
        from_cache: false,
        headers: reqwest::header::HeaderMap::new(),
    };
    let _ = meta; // The meta is queried through the gate above.
                  // Polling enqueue paths share the same store.
    let _ = enqueue_summaries(store, &merged.summaries, cfg.dry_run);
    Ok(Outcome304(false))
}

pub(crate) fn enqueue_summaries(
    store: &StateStore,
    summaries: &[crate::github::poll::IssueSummary],
    dry_run: bool,
) -> Option<DateTime<Utc>> {
    let mut earliest: Option<DateTime<Utc>> = None;
    for summary in summaries {
        if let Ok(_outcome) = store.enqueue(&summary.key, summary.ticket_type, dry_run) {
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
    }
    earliest
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

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    pub(crate) fn exit_code_for_outcome_table() {
        assert_eq!(exit_code_for(&TickOutcome::Processed), 0);
        assert_eq!(exit_code_for(&TickOutcome::Idle304), 0);
        assert_eq!(exit_code_for(&TickOutcome::IdleEmpty), 0);
        assert_eq!(exit_code_for(&TickOutcome::SkippedConcurrent), 0);
        assert_eq!(exit_code_for(&TickOutcome::SkippedCadence), 0);
        assert_eq!(exit_code_for(&TickOutcome::RateLimited), 0);
        assert_eq!(exit_code_for(&TickOutcome::Cancelled), 0);
        assert_eq!(exit_code_for(&TickOutcome::Failed), 1);
    }

    #[test]
    pub(crate) fn outcome_for_class_maps_each_failure_class() {
        assert!(matches!(
            outcome_for_class(FailureClass::RateLimit { reset_at: 0 }),
            TickOutcome::RateLimited
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Cancellation),
            TickOutcome::Cancelled
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Worker),
            TickOutcome::Failed
        ));
        assert!(matches!(
            outcome_for_class(FailureClass::Infrastructure),
            TickOutcome::Failed
        ));
    }

    #[test]
    pub(crate) fn map_phase_to_outcome_agrees_with_phase_taxonomy() {
        assert!(matches!(
            map_phase_to_outcome(Phase::Queued),
            TickOutcome::Processed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Failed),
            TickOutcome::Failed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Done),
            TickOutcome::Processed
        ));
        assert!(matches!(
            map_phase_to_outcome(Phase::Skipped),
            TickOutcome::Processed
        ));
    }

    #[test]
    pub(crate) fn extract_http_status_only_matches_github_api_variant() {
        let err = CaduceusError::GitHubApi {
            status: 422,
            message: "x".to_string(),
        };
        assert_eq!(extract_http_status(&err), Some(422));
        let err = CaduceusError::Worker {
            context: "result",
            stderr: "x".to_string(),
        };
        assert_eq!(extract_http_status(&err), None);
    }
}
