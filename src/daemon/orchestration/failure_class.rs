#![allow(dead_code, unused_imports)]
use super::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::github::issue::IssueKey;
use crate::github::Client;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::scheduler::Pool;
use crate::state::queue::{ClaimToken, Phase, QueueEntry, StateStore};
use crate::worker::supervisor::SupervisorOutcome;
use crate::worktree::{GitRunner, Worktree};

// ---------------------------------------------------------------------------
// FailureClass and classify_error
// ---------------------------------------------------------------------------

/// How a single [`CaduceusError`] should be classified for the
/// orchestrator's retry / requeue / terminal decisions.
///
/// The classification is the single source of truth for what the
/// orchestrator does next:
///
/// * [`FailureClass::Worker`] — the worker's run failed in a way
///   that counts against its retry budget (`Worker { .. }`
///   variants, content/schema/result validation errors, voice
///   rejections, code-result with no changes).
/// * [`FailureClass::Infrastructure`] — the daemon encountered a
///   transport, filesystem, or operator configuration problem
///   that does not count against the budget. The orchestrator
///   requeues with `not_before = now + retry_backoff_seconds` (or
///   immediately for cancellation).
/// * [`FailureClass::RateLimit`] — a typed GitHub rate-limit
///   observation. The orchestrator persists the reset time before
///   returning so the next tick respects it.
/// * [`FailureClass::Cancellation`] — operator cancellation
///   (SIGINT, SIGTERM, timeout-driven drain). The orchestrator
///   requeues with `not_before = now` so the next tick is
///   immediately eligible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureClass {
    Worker,
    Infrastructure,
    RateLimit { reset_at: u64 },
    Cancellation,
}

impl FailureClass {
    /// True when the failure should increment the entry's
    /// `attempts` counter (worker-attributable failures only).
    pub fn counts_against_retry_budget(&self) -> bool {
        matches!(self, FailureClass::Worker)
    }

    /// The matching `TickOutcome` for the failure class when
    /// the orchestrator should *return* it (cron contract:
    /// rate-limit and cancellation outcomes are exit 0, not
    /// failures). Returns `None` for the classes that must
    /// surface as `Failed` (worker-attributable and
    /// unclassified infrastructure failures).
    pub fn non_fatal_outcome(&self) -> Option<crate::state::meta::TickOutcome> {
        match self {
            FailureClass::RateLimit { .. } => Some(crate::state::meta::TickOutcome::RateLimited),
            FailureClass::Cancellation => Some(crate::state::meta::TickOutcome::Cancelled),
            FailureClass::Worker | FailureClass::Infrastructure => None,
        }
    }

    /// True when the orchestrator must persist a rate-limit
    /// observation before returning.
    pub fn must_persist_rate_limit(&self) -> bool {
        matches!(self, FailureClass::RateLimit { .. })
    }

    /// True when the failure is a cancellation (operator or
    /// timeout-driven).
    pub fn is_cancellation(&self) -> bool {
        matches!(self, FailureClass::Cancellation)
    }
}

/// Test seam: predicate tuple for [`FailureClass`]. Returns
/// `(counts_against_retry_budget, must_persist_rate_limit,
/// is_cancellation)`. Tests that need to assert the
/// orchestrator's branching logic can use this without
/// importing the orchestrator's private methods.
pub fn failure_class_predicates_for_tests(class: FailureClass) -> (bool, bool, bool) {
    (
        class.counts_against_retry_budget(),
        class.must_persist_rate_limit(),
        class.is_cancellation(),
    )
}

/// Test seam: outcome mapping for [`FailureClass`]. Returns
/// the [`meta::TickOutcome`](crate::state::meta::TickOutcome) the
/// orchestrator surfaces for each failure class. Mirrors the
/// private [`outcome_for_class`] helper used by the tick.
pub fn outcome_for_class_for_tests(class: FailureClass) -> crate::state::meta::TickOutcome {
    match class {
        FailureClass::RateLimit { .. } => crate::state::meta::TickOutcome::RateLimited,
        FailureClass::Cancellation => crate::state::meta::TickOutcome::Cancelled,
        _ => crate::state::meta::TickOutcome::Failed,
    }
}

/// Classify a [`CaduceusError`] into a [`FailureClass`].
///
/// The match is exhaustive: every `CaduceusError` variant maps
/// to exactly one failure class. New `CaduceusError` variants
/// make this match fail to compile until they are classified —
/// that is the intended compile-time guard. Returns by value so
/// callers can switch on the result without copying the original
/// error.
pub fn classify_error(err: &CaduceusError) -> FailureClass {
    match err {
        // Operator cancellation — orchestrator requeues with
        // not_before = now.
        CaduceusError::Cancelled => FailureClass::Cancellation,

        // Typed rate-limit observation — orchestrator persists
        // reset_at and returns.
        CaduceusError::RateLimited { reset_at, .. } => FailureClass::RateLimit {
            reset_at: *reset_at,
        },

        // Worker-attributable failures. The Worker variant
        // carries a `context` label that already discriminates
        // schema / result / content / no-code-change /
        // worker-exit / timeout — they all count against the
        // retry budget. The remaining Worker-class cases
        // (Other-shaped voice errors, etc.) collapse into the
        // same class.
        CaduceusError::Worker { .. } => FailureClass::Worker,

        // Transport / server / filesystem / teardown failures —
        // the contract pins HTTP transport/server, git transport,
        // filesystem, and teardown as infrastructure. They do
        // not count against the retry budget.
        CaduceusError::Http(_) => FailureClass::Infrastructure,
        CaduceusError::GitHubApi { .. } => FailureClass::Infrastructure,
        CaduceusError::Git { .. } => FailureClass::Infrastructure,
        CaduceusError::Push { .. } => FailureClass::Infrastructure,
        CaduceusError::PushCollision { .. } => FailureClass::Infrastructure,
        CaduceusError::Worktree { .. } => FailureClass::Infrastructure,
        CaduceusError::Queue { .. } => FailureClass::Infrastructure,
        CaduceusError::StateCorrupt { .. } => FailureClass::Infrastructure,
        CaduceusError::ReconciliationFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::ConflictingMarker { .. } => FailureClass::Worker,
        CaduceusError::Config(_) => FailureClass::Infrastructure,
        CaduceusError::TokenResolution(_) => FailureClass::Infrastructure,
        CaduceusError::LeadershipContended { .. } => FailureClass::Infrastructure,
        CaduceusError::LeaseStale { .. } => FailureClass::Infrastructure,
        CaduceusError::FencingTokenRegression { .. } => FailureClass::Infrastructure,
        CaduceusError::PoolSaturated { .. } => FailureClass::Infrastructure,
        CaduceusError::RepositoryExclusionHeld { .. } => FailureClass::Infrastructure,
        CaduceusError::DrainTimeout { .. } => FailureClass::Cancellation,
        CaduceusError::CircuitOpen { .. } => FailureClass::Infrastructure,
        CaduceusError::MaxDegradedAgeExceeded { .. } => FailureClass::Infrastructure,

        // New variants for daemon-owned repo storage (Task 5.4).
        // See REPO-ERROR-001: SymlinkedStorageRoot and
        // ModeNotPreserved are Infrastructure; WorktreeReuseAfterFailure
        // is Worker because the worker left the worktree in an
        // unsafe state.
        CaduceusError::SymlinkedStorageRoot { .. } => FailureClass::Infrastructure,
        CaduceusError::WorktreeReuseAfterFailure { .. } => FailureClass::Worker,
        CaduceusError::ModeNotPreserved { .. } => FailureClass::Infrastructure,

        // Executor-mode errors (Task 6.1). Both fire from
        // config-validation or unsupported-mode selection; the
        // orchestrator treats them as infrastructure so they do
        // not count against the worker retry budget.
        CaduceusError::OciCliNotFound { .. } => FailureClass::Infrastructure,
        CaduceusError::OciEngineUnavailable { .. } => FailureClass::Infrastructure,
        CaduceusError::OciMismatchedCliVersion { .. } => FailureClass::Infrastructure,
        CaduceusError::OciPullFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciCreateFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciStartFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciWaitFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciStopFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciRemoveFailed { .. } => FailureClass::Infrastructure,
        CaduceusError::OciUndeclaredMount { .. } => FailureClass::Worker,
        CaduceusError::OciSecretLeakSuspected { .. } => FailureClass::Infrastructure,
        CaduceusError::OciSecretLeakDetected { .. } => FailureClass::Infrastructure,
        CaduceusError::ReducedContainmentNotAcknowledged => FailureClass::Infrastructure,
        CaduceusError::OciNetworkNotInProfile { .. } => FailureClass::Worker,
        CaduceusError::OciSecretNotGranted { .. } => FailureClass::Worker,
        CaduceusError::OciImageNotDigestPinned { .. } => FailureClass::Worker,
        CaduceusError::OciResourceLimitRequired { .. } => FailureClass::Worker,
        CaduceusError::OciBaselineViolation { .. } => FailureClass::Worker,
        CaduceusError::OciUpgradeChoiceRequired => FailureClass::Worker,
        CaduceusError::OciPullPolicyIncompatible { .. } => FailureClass::Worker,

        // Generic Other — content / schema / public-voice / worker
        // result validation land here. Voice rejections and
        // content-shape failures are worker-attributable.
        CaduceusError::Other(_) => FailureClass::Worker,

        // IO and JSON errors during state mutation are
        // infrastructure.
        CaduceusError::Io(_) => FailureClass::Infrastructure,
        CaduceusError::Json(_) => FailureClass::Infrastructure,
        CaduceusError::Yaml(_) => FailureClass::Infrastructure,
    }
}
