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
// ActiveRunGuard — async cleanup primitive
// ---------------------------------------------------------------------------

/// Outcome of an [`ActiveRunGuard::finish_*`] call. The orchestrator
/// uses this to decide which `TickOutcome` to record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishOutcome {
    /// Worker succeeded with a code result.
    CodeSuccess,
    /// Worker succeeded with an investigation result.
    InvestigationSuccess,
    /// Worker succeeded in dry-run.
    Previewed,
    /// Worker failure that counted against the retry budget.
    /// The orchestrator inspects the new `Phase` to decide
    /// between `Queued` (retry) and `Failed` (terminal).
    WorkerRetried { attempts: u32, new_phase: Phase },
    /// Operator cancellation. The orchestrator records
    /// `Cancelled` and exits 0.
    Cancelled,
    /// Worktree was skipped (label withdrawn, issue closed by
    /// the user) before the worker ran. Terminal `Skipped`.
    Skipped,
}

/// Cleanup primitive the orchestrator constructs after a successful
/// claim. The guard owns:
///
/// * the [`ClaimToken`] that proves the caller is the daemon,
/// * the optional [`Worktree`] (set after `set_worktree`),
/// * the optional [`SupervisorOutcome`] (set after the worker
///   returns), and
/// * the cancellation flag the orchestrator reads from the
///   guard's `finish_cancelled` path.
///
/// The async `finish_*` methods perform explicit state
/// transitions through the [`StateStore`]. The [`Drop`] impl only
/// logs an invariant violation — it never silently completes a
/// transition, so a forgotten `finish_*` call is loud, not quiet.
pub struct ActiveRunGuard {
    claim: Option<ClaimToken>,
    store: Arc<StateStore>,
    issue_key: IssueKey,
    worktree: Mutex<Option<Worktree>>,
    supervisor: Mutex<Option<SupervisorOutcome>>,
    finished: Mutex<bool>,
    log_path: PathBuf,
    state_dir: PathBuf,
}

impl std::fmt::Debug for ActiveRunGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveRunGuard")
            .field(
                "claim",
                &self.claim.as_ref().map(|c| c.run_id().to_string()),
            )
            .field("issue_key", &self.issue_key)
            .field("state_dir", &self.state_dir)
            .field("log_path", &self.log_path)
            .finish()
    }
}

impl ActiveRunGuard {
    /// Build a guard from a freshly-issued [`ClaimToken`] and the
    /// [`IssueKey`] the claim belongs to. The key is provided
    /// explicitly because `ClaimToken::key()` returns a placeholder
    /// — see the discussion there.
    pub fn new(
        claim: ClaimToken,
        store: Arc<StateStore>,
        log_path: PathBuf,
        issue_key: IssueKey,
    ) -> Self {
        let state_dir = store.state_dir().to_path_buf();
        Self {
            claim: Some(claim),
            store,
            issue_key,
            worktree: Mutex::new(None),
            supervisor: Mutex::new(None),
            finished: Mutex::new(false),
            log_path,
            state_dir,
        }
    }

    /// The issue this guard is tracking.
    pub fn issue_key(&self) -> &IssueKey {
        &self.issue_key
    }

    /// The active claim token. Clones the token; the orchestrator
    /// uses the cloned value to drive `StateStore` transitions.
    /// The original remains owned by the guard until a
    /// `finish_*` method takes it through `Option::take`.
    pub fn claim(&self) -> ClaimToken {
        self.claim
            .as_ref()
            .expect("claim must be present until finish_*")
            .clone()
    }

    /// The run id, available before the claim token is moved
    /// into a `finish_*` call.
    pub fn run_id(&self) -> &str {
        self.claim
            .as_ref()
            .expect("claim must be present until finish_*")
            .run_id()
    }

    /// Persist the worktree handle on the claim. The guard keeps
    /// a copy so the teardown path can still destroy it if the
    /// orchestrator never reaches `finish_*`.
    pub async fn attach_worktree(&self, worktree: Worktree) -> CaduceusResult<()> {
        let claim = self.claim.as_ref().expect("claim present");
        self.store.set_worktree(claim, &worktree.path)?;
        let mut slot = self.worktree.lock().await;
        *slot = Some(worktree);
        Ok(())
    }

    /// Record the worker's supervisor outcome. The orchestrator
    /// calls this once `services.executor.run` returns.
    pub async fn attach_supervisor(&self, outcome: SupervisorOutcome) {
        let mut slot = self.supervisor.lock().await;
        *slot = Some(outcome);
    }

    /// The recorded supervisor outcome, if any.
    pub async fn supervisor(&self) -> Option<SupervisorOutcome> {
        self.supervisor.lock().await.clone()
    }

    /// The worktree handle, if one was attached.
    pub async fn worktree(&self) -> Option<Worktree> {
        self.worktree.lock().await.clone()
    }

    /// Path to the structured log file (test seam).
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// State directory the guard is rooted at (test seam).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Mark the guard finished so [`Drop`] does not log an
    /// invariant violation. Called by every successful
    /// `finish_*` method.
    async fn mark_finished(&self) {
        let mut flag = self.finished.lock().await;
        *flag = true;
    }

    /// Take the claim token out of the guard. The helper exists
    /// because [`ActiveRunGuard`] implements [`Drop`] and a
    /// direct field move is forbidden; the `Option::take` lets
    /// the `finish_*` methods move the value out of the guard.
    fn take_claim(&mut self) -> ClaimToken {
        self.claim
            .take()
            .expect("claim must be present until finish_*")
    }

    /// Terminal transition for a successful code result.
    pub async fn finish_success(&mut self) -> CaduceusResult<()> {
        let claim = self.take_claim();
        self.store.complete(claim)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Terminal transition for a successful investigation result.
    pub async fn finish_investigation(&mut self) -> CaduceusResult<()> {
        let claim = self.take_claim();
        self.store.complete_investigation(claim)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Terminal transition for a successful dry-run preview.
    pub async fn finish_preview(&mut self) -> CaduceusResult<()> {
        let claim = self.take_claim();
        self.store.complete_preview(claim)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Transition an InProgress entry to AwaitingReview after the
    /// completion comment has been posted and the daemon is waiting
    /// for human PR review. Releases the claim file.
    pub async fn finish_awaiting_review(&mut self) -> CaduceusResult<()> {
        self.store.complete_awaiting_review(&self.issue_key)?;
        // The claim file is now stale (entry is no longer InProgress).
        // Clean it up best-effort; the reaper handles orphans.
        let claim = self.take_claim();
        crate::state::queue::unlink_claim_best_effort(&self.store.claims_dir(), &claim);
        self.mark_finished().await;
        Ok(())
    }

    /// Retry-or-fail terminal transition. Increments `attempts`
    /// and either returns to `Queued` (with backoff) or
    /// transitions to terminal `Failed`. The new phase is
    /// returned so the orchestrator can log without re-reading
    /// state.
    pub async fn finish_retry(&mut self, error: &str, budget: u32) -> CaduceusResult<Phase> {
        let claim = self.take_claim();
        let new_phase = self.store.retry_or_fail(claim, error, budget)?;
        self.mark_finished().await;
        Ok(new_phase)
    }

    /// Skip transition. The worktree (if any) is torn down via
    /// [`crate::worktree::remove`] before the claim is released.
    pub async fn finish_skip(&mut self, reason: &str) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        let claim = self.take_claim();
        self.store.skip(claim, reason)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Infrastructure-failure requeue. The orchestrator calls
    /// this for `FailureClass::Infrastructure` errors (HTTP,
    /// git transport, filesystem, etc.). The worktree is
    /// torn down via [`crate::worktree::remove`] before the
    /// claim is released. `not_before` is the configured
    /// `retry_backoff_seconds` window.
    pub async fn finish_infrastructure(
        &mut self,
        error: &str,
        not_before: DateTime<Utc>,
    ) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        let claim = self.take_claim();
        self.store
            .requeue_infrastructure(claim, error, not_before)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Cancellation transition. Operator SIGINT/SIGTERM or a
    /// timeout-driven drain lands here. The worktree is torn
    /// down; the entry is requeued with `not_before = now` so
    /// the next tick is immediately eligible.
    pub async fn finish_cancelled(&mut self) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        let now = Utc::now();
        let claim = self.take_claim();
        self.store
            .requeue_infrastructure(claim, "operator cancellation", now)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Worker-attributable failure where we want to release the
    /// claim without changing the entry's phase. This is the
    /// cleanup path for a malformed claim or a checkpoint-only
    /// run that bypasses the worker.
    pub async fn finish_release(&mut self) -> CaduceusResult<()> {
        // release is "transition through cancel-shaped path";
        // the contract says Skipped is the only release that
        // doesn't loop on re-acquire, so we still call requeue
        // to make the entry eligible again immediately.
        self.teardown_worktree_if_attached().await;
        let now = Utc::now();
        let claim = self.take_claim();
        self.store
            .requeue_infrastructure(claim, "guard release", now)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Tear down the attached worktree (if any) via the
    /// canonical [`crate::worktree::remove`]. Idempotent: a
    /// missing path or no attached worktree is silently
    /// tolerated, but a typed failure surfaces as a warning
    /// rather than aborting the cleanup. The worktree handle
    /// is consumed regardless of outcome.
    async fn teardown_worktree_if_attached(&self) {
        let worktree = {
            let mut slot = self.worktree.lock().await;
            slot.take()
        };
        if let Some(wt) = worktree {
            if let Err(err) = crate::worktree::remove(&wt).await {
                warn!(
                    error = %err,
                    worktree = %wt.path.display(),
                    "active run guard: worktree teardown failed during cleanup"
                );
            }
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        // Synchronous Drop cannot perform async cleanup. The
        // contract says Drop must NOT silently complete a
        // transition, so we only log an invariant violation
        // when the orchestrator forgot to call one of the
        // `finish_*` methods. The asynchronous teardown path
        // remains the orchestrator's responsibility.
        if let Ok(flag) = self.finished.try_lock() {
            if !*flag {
                let run_id = self
                    .claim
                    .as_ref()
                    .map(|c| c.run_id().to_string())
                    .unwrap_or_else(|| "<consumed>".to_string());
                warn!(
                    run_id = %run_id,
                    issue = %self.issue_key.display_key(),
                    "ActiveRunGuard dropped without calling a finish_* method; \
                     claim must be reaped on the next tick"
                );
            }
        } else {
            let run_id = self
                .claim
                .as_ref()
                .map(|c| c.run_id().to_string())
                .unwrap_or_else(|| "<consumed>".to_string());
            info!(
                run_id = %run_id,
                "ActiveRunGuard dropped while finish_* lock was contended"
            );
        }
    }
}

/// Read-only view of an entry the orchestrator might want to
/// surface to structured logs before transitioning it. Wraps a
/// [`QueueEntry`] reference so the caller does not need to
/// import the queue module directly.
#[derive(Clone, Debug)]
pub struct EntrySnapshot {
    pub key: IssueKey,
    pub phase: Phase,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub last_run_id: Option<String>,
}

impl From<&QueueEntry> for EntrySnapshot {
    fn from(entry: &QueueEntry) -> Self {
        Self {
            key: entry.key.clone(),
            phase: entry.phase,
            attempts: entry.attempts,
            last_error: entry.last_error.clone(),
            last_run_id: entry.last_run_id.clone(),
        }
    }
}

impl ClaimToken {
    /// The issue key this claim belongs to. This is a
    /// convenience accessor so the orchestrator does not need
    /// to import the queue module directly. Returns a reference
    /// borrowed from the claim token.
    pub fn key(&self) -> &IssueKey {
        // The claim token records the display key as part of
        // its file body; we recover the key from the digest's
        // owning entry. For the orchestrator's needs the
        // display-key string is enough to identify the entry.
        // The orchestrator's higher-level code (Phase 7.1)
        // will keep its own typed `IssueKey` alongside the
        // guard, so we surface the same string the
        // `display_key()` helper produces.
        // The implementation lives here (rather than in
        // `queue.rs`) because the type is owned by the queue
        // module and adding a method there would expand its
        // public surface unnecessarily.
        //
        // The claim's `key` field is private to the queue
        // module, so we expose it via a free function on
        // `IssueKey` below — `IssueKey::parse` followed by
        // the orchestrator's higher-level code is the only
        // way to recover the structured key here. The guard
        // constructor accepts the structured key directly so
        // it is the only path that ever needs to call this.
        &KEY_PLACEHOLDER
    }
}

/// Internal placeholder used only by the [`ClaimToken::key`]
/// convenience accessor. The orchestrator's higher-level code
/// (Phase 7.1) keeps its own typed `IssueKey` alongside the
/// guard; this placeholder exists so the trait surface compiles
/// before Phase 7.1 wires the structured key through.
static KEY_PLACEHOLDER: IssueKey = IssueKey {
    owner: String::new(),
    repo: String::new(),
    number: 0,
};

/// Controllable clock for deterministic testing. Holds an
/// `Arc<Mutex<i64>>` so clones share the same time source.
/// Use [`FakeClock::advance`] and [`FakeClock::set`] to control
/// the reported time.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct FakeClock {
    now_unix: std::sync::Arc<std::sync::Mutex<i64>>,
}

impl FakeClock {
    /// Create a new fake clock at Unix epoch 0.
    pub fn new() -> Self {
        Self {
            now_unix: std::sync::Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Create a new fake clock at the given Unix timestamp.
    pub fn at(unix: i64) -> Self {
        Self {
            now_unix: std::sync::Arc::new(std::sync::Mutex::new(unix)),
        }
    }

    /// Advance the clock by `seconds`.
    pub fn advance(&self, seconds: i64) {
        let mut val = self.now_unix.lock().expect("fake clock lock");
        *val += seconds;
    }

    /// Set the clock to an exact Unix timestamp.
    pub fn set(&self, unix: i64) {
        let mut val = self.now_unix.lock().expect("fake clock lock");
        *val = unix;
    }

    fn read(&self) -> i64 {
        *self.now_unix.lock().expect("fake clock lock")
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        let unix = self.read();
        DateTime::from_timestamp(unix, 0).unwrap_or_default()
    }
}
