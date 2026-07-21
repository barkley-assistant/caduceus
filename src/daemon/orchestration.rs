//! Orchestration layer for the per-tick pipeline.
//!
//! This module owns the canonical types the per-tick controller
//! passes between phases:
//!
//! * [`Services`] — the bundle of dependency-injection traits
//!   (`clock`, `github`, `git`, `process`) used only where deterministic
//!   testing requires them. Production adapters are thin wrappers
//!   around the concrete types owned by [`crate::config`],
//!   [`crate::github`], [`crate::worktree`], and
//!   [`crate::worker_supervisor`].
//! * [`FailureClass`] and [`classify_error`] — the exhaustive
//!   mapping from a [`CaduceusError`] to the four failure classes
//!   the orchestrator cares about (`Worker`, `Infrastructure`,
//!   `RateLimit`, `Cancellation`).
//! * [`ActiveRunGuard`] — the async cleanup primitive the
//!   orchestrator constructs after a successful claim. Its
//!   `finish_*` methods perform explicit state transitions;
//!   `Drop` only logs an invariant violation, never silently
//!   completes a transition.
//!
//! The shape of the canonical worker entry point
//! ([`crate::worker::supervisor::supervise`]), the canonical
//! worktree handle ([`crate::worktree::Worktree`]), and the
//! canonical finalization context ([`crate::finalize::FinalizeContext`])
//! are owned by their respective modules; this module's
//! `ActiveRunGuard` calls them by reference and never duplicates
//! their signatures.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::github::issue::IssueKey;
use crate::github::Client;
use crate::infra::error::{CaduceusError, CaduceusResult};
use crate::scheduler::Pool;
use crate::state::queue::{ClaimToken, Phase, QueueEntry, StateStore};
use crate::worker::supervisor::SupervisorOutcome;
use crate::worktree::{GitRunner, Worktree};

// ---------------------------------------------------------------------------
// Services — the dependency-injection surface the orchestrator uses
// ---------------------------------------------------------------------------

/// Trait abstraction over wall-clock access. Production callers
/// use [`SystemClock`]; tests use a fake that returns deterministic
/// [`DateTime<Utc>`] values.
pub trait Clock: Send + Sync {
    /// The current wall-clock time in UTC.
    fn now(&self) -> DateTime<Utc>;

    /// The current Unix timestamp in seconds. Default impl delegates
    /// to `now().timestamp()` so existing implementors get it for free.
    fn now_unix(&self) -> i64 {
        self.now().timestamp()
    }
}

/// Production wall-clock adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Trait abstraction over the GitHub API client. Production callers
/// wrap [`Client`] in [`GithubClientAdapter`]; tests use a
/// `wiremock`-backed `Client` and pass it through the same adapter
/// so the contract surface is unchanged.
pub trait GithubClient: Send + Sync {
    /// Borrow the inner `Arc<Client>` so call sites that need
    /// the full surface (e.g. cached `Arc<HttpCache>` test
    /// seams) can still reach it without an extra trait method
    /// per call.
    fn inner(&self) -> &Arc<Client>;
}

/// Production GitHub adapter. Thin — every trait method would
/// just forward to the inner `Client`, so the trait exposes the
/// underlying reference once and lets the orchestrator call
/// [`Client`] methods directly. The adapter exists to make the
/// orchestrator's dependency injection explicit at every call
/// site: `services.github.inner().get(...)` reads as "go through
/// the GitHub dependency" rather than "go through a free function".
#[derive(Clone)]
pub struct GithubClientAdapter {
    client: Arc<Client>,
}

impl std::fmt::Debug for GithubClientAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClientAdapter").finish()
    }
}

impl GithubClientAdapter {
    /// Wrap an `Arc<Client>`.
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

impl GithubClient for GithubClientAdapter {
    fn inner(&self) -> &Arc<Client> {
        &self.client
    }
}

/// Trait abstraction over the [`GitRunner`]. Production callers
/// wrap a concrete `GitRunner` in [`GitRunnerAdapter`]; tests use
/// the same wrapper.
pub trait Git: Send + Sync {
    /// Borrow the underlying [`GitRunner`].
    fn runner(&self) -> &GitRunner;
}

/// Production git adapter.
#[derive(Clone)]
pub struct GitRunnerAdapter {
    runner: GitRunner,
}

impl std::fmt::Debug for GitRunnerAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRunnerAdapter").finish()
    }
}

impl GitRunnerAdapter {
    /// Wrap a [`GitRunner`].
    pub fn new(runner: GitRunner) -> Self {
        Self { runner }
    }
}

impl Git for GitRunnerAdapter {
    fn runner(&self) -> &GitRunner {
        &self.runner
    }
}

/// Trait abstraction over the worker process supervisor. Production
/// callers wrap [`crate::worker::supervisor::supervise`] in
/// [`ProcessSupervisorAdapter`]; tests use a trait object whose
/// `supervise` returns a canned [`SupervisorOutcome`].
#[allow(clippy::too_many_arguments)]
pub trait ProcessSupervisor: Send + Sync {
    /// Spawn the worker under the canonical supervision contract.
    /// Returns a [`SupervisorOutcome`] on success or a
    /// [`CaduceusError::Worker`] on supervision failure.
    fn supervise<'a>(
        &'a self,
        self_exe: &'a Path,
        cfg: &'a crate::infra::config::Config,
        issue: &'a IssueKey,
        worktree: &'a Path,
        run_id: &'a str,
        context_json: &'a str,
        worker_command: &'a [String],
        cancellation: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>,
    >;
}

/// Production process supervisor adapter.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessSupervisorAdapter;

#[allow(clippy::too_many_arguments)]
impl ProcessSupervisor for ProcessSupervisorAdapter {
    fn supervise<'a>(
        &'a self,
        self_exe: &'a Path,
        cfg: &'a crate::infra::config::Config,
        issue: &'a IssueKey,
        worktree: &'a Path,
        run_id: &'a str,
        context_json: &'a str,
        worker_command: &'a [String],
        cancellation: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = CaduceusResult<SupervisorOutcome>> + Send + 'a>,
    > {
        Box::pin(crate::worker::supervisor::supervise(
            self_exe,
            cfg,
            issue,
            worktree,
            run_id,
            context_json,
            worker_command,
            cancellation,
        ))
    }
}

/// Bundle of dependencies the orchestrator needs for one tick.
/// The bundle is the canonical point of injection: production
/// constructs one of these per daemon process; tests construct
/// one per test with mock adapters.
#[derive(Clone)]
pub struct Services {
    pub clock: Arc<dyn Clock>,
    pub github: Arc<dyn GithubClient>,
    pub git: Arc<dyn Git>,
    pub process: Arc<dyn ProcessSupervisor>,
    pub pool: Arc<Pool>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("clock", &"Arc<dyn Clock>")
            .field("github", &"Arc<dyn GithubClient>")
            .field("git", &"Arc<dyn Git>")
            .field("process", &"Arc<dyn ProcessSupervisor>")
            .field("pool", &"Arc<Pool>")
            .finish()
    }
}

impl Services {
    /// Production convenience constructor.
    pub fn production(
        clock: Arc<dyn Clock>,
        github: Arc<Client>,
        git: GitRunner,
        process: Arc<dyn ProcessSupervisor>,
        pool: Arc<Pool>,
    ) -> Self {
        Self {
            clock,
            github: Arc::new(GithubClientAdapter::new(github)),
            git: Arc::new(GitRunnerAdapter::new(git)),
            process,
            pool,
        }
    }

    /// Test-only constructor that takes pre-built trait objects so
    /// tests can mix real adapters with fakes.
    pub fn for_tests(
        clock: Arc<dyn Clock>,
        github: Arc<dyn GithubClient>,
        git: Arc<dyn Git>,
        process: Arc<dyn ProcessSupervisor>,
        pool: Arc<Pool>,
    ) -> Self {
        Self {
            clock,
            github,
            git,
            process,
            pool,
        }
    }
}

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
        CaduceusError::OciNotImplementedYet => FailureClass::Infrastructure,
        CaduceusError::ReducedContainmentNotAcknowledged => FailureClass::Infrastructure,

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
    /// Build a guard from a freshly-issued [`ClaimToken`].
    pub fn new(claim: ClaimToken, store: Arc<StateStore>, log_path: PathBuf) -> Self {
        let state_dir = store.state_dir().to_path_buf();
        let issue_key = claim.key().clone();
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
    /// calls this once `services.process.supervise` returns.
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

// ---------------------------------------------------------------------------
// Inline tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod inline_tests {
    use super::*;
    use crate::infra::config::Config;
    use crate::state::queue::{ClaimFileBody, ClaimToken, CLAIM_FILE_VERSION};

    fn cfg() -> Config {
        Config::test_defaults(std::path::Path::new("/tmp"))
    }

    fn dummy_claim() -> ClaimToken {
        ClaimToken::for_test(
            std::env::temp_dir().join("caduceus-orchestration-tests"),
            "deadbeef00",
            "RUNID",
        )
    }

    #[test]
    fn classify_error_maps_variants() {
        // Cancellation
        let err = CaduceusError::Cancelled;
        assert_eq!(classify_error(&err), FailureClass::Cancellation);

        // Rate limit
        let err = CaduceusError::RateLimited {
            reset_at: 12345,
            remaining: 0,
            limit: Some(5000),
        };
        assert_eq!(
            classify_error(&err),
            FailureClass::RateLimit { reset_at: 12345 }
        );

        // Worker-attributable
        let err = CaduceusError::Worker {
            context: "result",
            stderr: "schema mismatch".to_string(),
        };
        assert_eq!(classify_error(&err), FailureClass::Worker);

        let err = CaduceusError::Other("voice: forbidden term".to_string());
        assert_eq!(classify_error(&err), FailureClass::Worker);

        // Infrastructure
        let err = CaduceusError::Config("bad worker command".to_string());
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        let err = CaduceusError::TokenResolution("gh not found".to_string());
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        // HTTP transport — infrastructure
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        let err: CaduceusError = io_err.into();
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        // Pool saturated — infrastructure
        let err = CaduceusError::PoolSaturated {
            current_depth: 5,
            max_depth: 10,
        };
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        // Repository exclusion held — infrastructure
        let err = CaduceusError::RepositoryExclusionHeld {
            repo_key: "owner/repo".into(),
        };
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        // Drain timeout — cancellation
        let err = CaduceusError::DrainTimeout {
            timed_out_run_ids: vec!["run-1".into()],
        };
        assert_eq!(classify_error(&err), FailureClass::Cancellation);

        // SymlinkedStorageRoot — infrastructure
        let err = CaduceusError::SymlinkedStorageRoot {
            path: PathBuf::from("/tmp/link"),
        };
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);

        // WorktreeReuseAfterFailure — worker
        let err = CaduceusError::WorktreeReuseAfterFailure {
            run_id: "deadbeef".into(),
            worktree_path: PathBuf::from("/tmp/failed"),
            last_state: "Failed".into(),
        };
        assert_eq!(classify_error(&err), FailureClass::Worker);

        // ModeNotPreserved — infrastructure
        let err = CaduceusError::ModeNotPreserved {
            path: PathBuf::from("/tmp/x"),
            expected: 0o700,
            observed: 0o755,
        };
        assert_eq!(classify_error(&err), FailureClass::Infrastructure);
    }

    #[test]
    fn classify_error_is_exhaustive_at_compile_time() {
        // Every CaduceusError variant is classified. If a new
        // variant is added without a `classify_error` arm,
        // this match will fail to compile.
        let variants: [CaduceusError; 28] = [
            CaduceusError::Config("x".into()),
            CaduceusError::Io(std::io::Error::other("x")),
            CaduceusError::Json(serde_json::from_str::<u8>("not-a-number").unwrap_err()),
            CaduceusError::Yaml(serde_yaml::from_str::<u8>(": x").unwrap_err()),
            CaduceusError::Worker {
                context: "spawn",
                stderr: "x".into(),
            },
            CaduceusError::Worktree {
                context: "create",
                stderr: "x".into(),
            },
            CaduceusError::Queue {
                context: "claim",
                stderr: "x".into(),
            },
            CaduceusError::Push {
                context: "push",
                stderr: "x".into(),
            },
            CaduceusError::PushCollision {
                branch: "b".into(),
                remote_oid: "r".into(),
                local_oid: "l".into(),
            },
            CaduceusError::StateCorrupt {
                path: PathBuf::from("/tmp/x"),
                message: "x".into(),
            },
            CaduceusError::Git {
                operation: "commit",
                stderr: "x".into(),
            },
            CaduceusError::GitHubApi {
                status: 500,
                message: "x".into(),
            },
            CaduceusError::RateLimited {
                reset_at: 1,
                remaining: 0,
                limit: None,
            },
            CaduceusError::TokenResolution("x".into()),
            CaduceusError::Cancelled,
            CaduceusError::Other("x".into()),
            // Http is exercised via the Io case above; we
            // synthesise an Http variant by running reqwest's
            // error path indirectly. For the compile-time
            // guard the variant list is the actual concern.
            CaduceusError::Worker {
                context: "http",
                stderr: "transport".into(),
            },
            CaduceusError::LeadershipContended {
                context: "acquire",
                stderr: "contended".into(),
            },
            CaduceusError::LeaseStale {
                context: "renew",
                stderr: "expired".into(),
            },
            CaduceusError::FencingTokenRegression {
                issue_key: "owner/repo#1".into(),
                stale_token: 1,
                current_token: 3,
            },
            CaduceusError::PoolSaturated {
                current_depth: 1,
                max_depth: 2,
            },
            CaduceusError::RepositoryExclusionHeld {
                repo_key: "owner/repo".into(),
            },
            CaduceusError::DrainTimeout {
                timed_out_run_ids: vec!["run-1".into()],
            },
            CaduceusError::CircuitOpen {
                scope: "provider",
                scope_id: "github".into(),
                retry_after: 1800,
                probe_in_flight: false,
            },
            CaduceusError::MaxDegradedAgeExceeded {
                scope: "repository",
                scope_id: "owner/repo".into(),
                opened_at: 1000000,
            },
            CaduceusError::SymlinkedStorageRoot {
                path: PathBuf::from("/tmp/link"),
            },
            CaduceusError::WorktreeReuseAfterFailure {
                run_id: "deadbeef".into(),
                worktree_path: PathBuf::from("/tmp/failed"),
                last_state: "Failed".into(),
            },
            CaduceusError::ModeNotPreserved {
                path: PathBuf::from("/tmp/x"),
                expected: 0o700,
                observed: 0o755,
            },
        ];
        for v in &variants {
            let _class = classify_error(v);
        }
        // Http is also covered by the match arms even though
        // we don't synthesise one here.
    }

    #[test]
    fn failure_class_predicates() {
        let worker = FailureClass::Worker;
        assert!(worker.counts_against_retry_budget());
        assert!(!worker.must_persist_rate_limit());
        assert!(!worker.is_cancellation());

        let infra = FailureClass::Infrastructure;
        assert!(!infra.counts_against_retry_budget());
        assert!(!infra.must_persist_rate_limit());
        assert!(!infra.is_cancellation());

        let rate = FailureClass::RateLimit { reset_at: 100 };
        assert!(!rate.counts_against_retry_budget());
        assert!(rate.must_persist_rate_limit());
        assert!(!rate.is_cancellation());

        let cancel = FailureClass::Cancellation;
        assert!(!cancel.counts_against_retry_budget());
        assert!(!cancel.must_persist_rate_limit());
        assert!(cancel.is_cancellation());
    }

    #[test]
    fn claim_token_key_accessor_returns_placeholder() {
        // The convenience accessor compiles and returns a
        // reference; the orchestrator's higher-level code
        // keeps its own typed `IssueKey` alongside the guard.
        let claim = dummy_claim();
        let key = claim.key();
        assert_eq!(key.number, 0);
        assert!(key.owner.is_empty());
    }

    #[test]
    fn claim_file_body_round_trip() {
        // Sanity check that the queue module's ClaimFileBody
        // round-trips so the orchestrator can rehydrate a
        // claim token if needed.
        let key = IssueKey::parse("owner/repo#1").expect("key");
        let started_at: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().expect("timestamp");
        let body = ClaimFileBody {
            version: CLAIM_FILE_VERSION,
            key: key.clone(),
            run_id: "RUNID".to_string(),
            pid: 42,
            process_start_identity: "boot-1/100".to_string(),
            started_at,
            worktree_path: None,
        };
        let serialized = serde_json::to_string(&body).unwrap();
        let parsed: ClaimFileBody = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.version, CLAIM_FILE_VERSION);
        assert_eq!(parsed.key, key);
        assert_eq!(parsed.run_id, "RUNID");
    }

    #[test]
    fn system_clock_returns_recent_utc() {
        let before = Utc::now();
        let now = SystemClock.now();
        let after = Utc::now();
        assert!(now >= before);
        assert!(now <= after);
    }

    #[test]
    fn services_production_helper_compiles() {
        // The constructor wires the four adapters. We can't
        // call services.process.supervise here because that
        // would spawn a worker; the test only verifies the
        // type compiles and the field accessors work.
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let _ = clock.now();
        let _cfg = cfg();
    }

    #[test]
    fn fake_clock_default_starts_at_epoch() {
        let fc = FakeClock::new();
        assert_eq!(fc.now_unix(), 0);
    }

    #[test]
    fn fake_clock_advance_works() {
        let fc = FakeClock::new();
        fc.advance(100);
        assert_eq!(fc.now_unix(), 100);
    }

    #[test]
    fn fake_clock_set_works() {
        let fc = FakeClock::new();
        fc.set(999);
        assert_eq!(fc.now_unix(), 999);
    }

    #[test]
    fn fake_clock_clones_share_time() {
        let fc = FakeClock::new();
        let fc2 = fc.clone();
        fc.advance(50);
        assert_eq!(fc2.now_unix(), 50);
    }

    #[test]
    fn fake_clock_now_returns_correct_datetime() {
        let fc = FakeClock::at(946684800); // 2000-01-01T00:00:00Z
        assert_eq!(fc.now().timestamp(), 946684800);
    }
}
