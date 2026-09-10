use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::infra::error::CaduceusResult;
use crate::repo::fork_quarantine::ForkQuarantine;
use crate::repo::review_worktree::ReviewWorktree;
use crate::review::ReviewTarget;
use crate::state::review::{ReviewClaimToken, ReviewPhase, ReviewStore};
use crate::worktree::GitRunner;

// ReviewRunGuard — async cleanup primitive for review claims (issue
// #339). Mirrors `ActiveRunGuard` but is ReviewStore/ReviewTarget
// bound: issue claims are IssueKey/ClaimToken bound and the two must
// never mix (claim files live in separate directories by contract).

/// Cleanup primitive the review drain constructs after a successful
/// review claim. The guard owns:
///
/// * the [`ReviewClaimToken`] that proves the caller is the daemon,
/// * the optional [`ReviewWorktree`] (set after `attach_worktree`),
///   torn down on every `finish_*` route EXCEPT the Terminal
///   NeedsAttention routes (the preserved worktree is forensic
///   evidence, DAR §8.1),
/// * the optional [`ForkQuarantine`] (set after
///   `attach_quarantine` on fork runs, #337 Phase 2), torn down on
///   the TERMINAL `finish_*` routes only — Done, Skipped,
///   NeedsAttention, mutation-violation, and the terminal `Failed`
///   arm of `finish_retry`. The requeue routes (retry→Queued,
///   infrastructure, cancellation) deliberately KEEP the
///   quarantine: removing it on a non-terminal route would make the
///   retried fork claim fall back to the production-mirror path
///   (DAR §11.2, plan §3.2 — the quarantine is removed at terminal
///   status; the production mirror is never consulted for fork
///   runs). The quarantine is a throwaway object store (#337 Phase
///   2) and must not linger past the run; per plan §3.2 its removal
///   also force-removes any worktree registered to it, so a fork
///   run that lands in NeedsAttention loses the worktree files with
///   the quarantine (the "preserved worktree" forensic property
///   above applies to same-repo runs, whose worktree is registered
///   to the production mirror) — and
/// * the target identity for event emission and store routing.
///
/// The async `finish_*` methods perform explicit state transitions
/// through the [`ReviewStore`]. The [`Drop`] impl only logs an
/// invariant violation — it never silently completes a transition, so
/// a forgotten `finish_*` call is loud, not quiet.
pub struct ReviewRunGuard {
    claim: Option<ReviewClaimToken>,
    store: Arc<ReviewStore>,
    target: ReviewTarget,
    runner: GitRunner,
    worktree: Mutex<Option<ReviewWorktree>>,
    quarantine: Mutex<Option<ForkQuarantine>>,
    finished: Mutex<bool>,
    log_path: PathBuf,
    state_dir: PathBuf,
}

impl std::fmt::Debug for ReviewRunGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewRunGuard")
            .field(
                "claim",
                &self.claim.as_ref().map(|c| c.run_id().to_string()),
            )
            .field(
                "target",
                &format!(
                    "{}#pr/{}",
                    self.target.repository.full_name(),
                    self.target.pull_request
                ),
            )
            .field("state_dir", &self.state_dir)
            .field("log_path", &self.log_path)
            .finish()
    }
}

impl ReviewRunGuard {
    /// Build a guard from a freshly-issued [`ReviewClaimToken`] and
    /// the [`ReviewTarget`] the claim belongs to. The [`GitRunner`]
    /// is used for worktree teardown on the non-Terminal routes.
    pub fn new(
        claim: ReviewClaimToken,
        store: Arc<ReviewStore>,
        log_path: PathBuf,
        target: ReviewTarget,
        runner: GitRunner,
    ) -> Self {
        let state_dir = store.state_dir().to_path_buf();
        Self {
            claim: Some(claim),
            store,
            target,
            runner,
            worktree: Mutex::new(None),
            quarantine: Mutex::new(None),
            finished: Mutex::new(false),
            log_path,
            state_dir,
        }
    }

    /// The review target this guard is tracking.
    pub fn target(&self) -> &ReviewTarget {
        &self.target
    }

    /// The active claim token. Clones the token; the original remains
    /// owned by the guard until a `finish_*` method takes it through
    /// `Option::take`.
    pub fn claim(&self) -> ReviewClaimToken {
        self.claim
            .as_ref()
            .expect("claim must be present until finish_*")
            .clone()
    }

    /// The run id, available before the claim token is moved into a
    /// `finish_*` call.
    pub fn run_id(&self) -> &str {
        self.claim
            .as_ref()
            .expect("claim must be present until finish_*")
            .run_id()
    }

    /// Persist the worktree handle on the guard. The guard keeps a
    /// copy so teardown paths can destroy it, and the Terminal
    /// mutation route can leave it in place for forensics.
    pub async fn attach_worktree(&self, worktree: ReviewWorktree) {
        let mut slot = self.worktree.lock().await;
        *slot = Some(worktree);
    }

    /// Persist the fork-quarantine handle on the guard (fork runs,
    /// #337 Phase 2). The TERMINAL `finish_*` routes tear it down
    /// via [`Self::teardown_quarantine_if_attached`]; the requeue
    /// routes keep it so a retried fork claim reuses the quarantine
    /// (DAR §11.2, plan §3.2).
    pub async fn attach_quarantine(&self, quarantine: ForkQuarantine) {
        let mut slot = self.quarantine.lock().await;
        *slot = Some(quarantine);
    }

    /// Path to the structured log file (test seam).
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// State directory the guard is rooted at (test seam).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Mark the guard finished so [`Drop`] does not log an invariant
    /// violation. Called by every successful `finish_*` method.
    async fn mark_finished(&self) {
        let mut flag = self.finished.lock().await;
        *flag = true;
    }

    /// Take the claim token out of the guard. The helper exists
    /// because [`ReviewRunGuard`] implements [`Drop`] and a direct
    /// field move is forbidden; the `Option::take` lets the
    /// `finish_*` methods move the value out of the guard.
    fn take_claim(&mut self) -> ReviewClaimToken {
        self.claim
            .take()
            .expect("claim must be present until finish_*")
    }

    /// Terminal transition for a successful review run: the entry
    /// moves to `Done`. The worktree is torn down (the result is
    /// durably in history; nothing to preserve).
    pub async fn finish_done(&mut self) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        self.teardown_quarantine_if_attached().await;
        let claim = self.take_claim();
        self.store.complete_review(claim)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Retry-or-fail terminal transition. Increments `attempts` and
    /// either returns to `Queued` (with backoff) or transitions to
    /// terminal `Failed` (DAR §8.1 Worker row). The new phase is
    /// returned so the orchestrator can log without re-reading state.
    pub async fn finish_retry(&mut self, error: &str, budget: u32) -> CaduceusResult<ReviewPhase> {
        self.teardown_worktree_if_attached().await;
        let claim = self.take_claim();
        let new_phase = self.store.retry_or_fail_review(claim, error, budget)?;
        // Terminal-only quarantine teardown (DAR §11.2, plan §3.2):
        // a requeued retry (`Queued`) must KEEP the quarantine clone
        // so the next claim reuses it instead of falling back to the
        // production-mirror path; only the terminal `Failed`
        // transition removes it.
        if new_phase == ReviewPhase::Failed {
            self.teardown_quarantine_if_attached().await;
        }
        self.mark_finished().await;
        Ok(new_phase)
    }

    /// Quiet-skip transition (DAR §8.1 skip rows: unavailable head
    /// SHA, gone PR, oversized diff). The worktree is torn down
    /// before the claim is released.
    pub async fn finish_skip(&mut self, reason: &str) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        self.teardown_quarantine_if_attached().await;
        let claim = self.take_claim();
        self.store.skip_review(claim, reason)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Infrastructure-failure requeue. The orchestrator calls this
    /// for `FailureClass::Infrastructure` errors (HTTP, git
    /// transport, filesystem, etc.). The worktree is torn down and
    /// the claim is released. `not_before` is the configured
    /// `retry_backoff_seconds` window; `attempts` is NOT incremented.
    /// The fork quarantine is deliberately KEPT on this requeue
    /// route (DAR §11.2, plan §3.2): a retried fork claim must reuse
    /// the clone, never the production mirror.
    pub async fn finish_infrastructure(
        &mut self,
        error: &str,
        not_before: chrono::DateTime<chrono::Utc>,
    ) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        let claim = self.take_claim();
        self.store
            .requeue_infrastructure_review(claim, error, not_before)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Terminal NeedsAttention transition. The worktree is
    /// deliberately NOT torn down: `route_review_to_needs_attention`
    /// documents the preserved worktree as forensic evidence (DAR
    /// §8.1 Terminal row) — the review GC reclaims it by age. The
    /// claim is released after the route so a later `queue reset` is
    /// not blocked by a stale claim file.
    pub async fn finish_needs_attention(
        &mut self,
        error: &str,
        source: &str,
        recovery_hint: &str,
    ) -> CaduceusResult<()> {
        self.teardown_quarantine_if_attached().await;
        let claim = self.take_claim();
        self.store
            .route_review_to_needs_attention(claim, error, source, recovery_hint)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Terminal mutation-violation route (DAR §8.1, §10; #306): emits
    /// the `review_mutation_violation` event and routes to
    /// NeedsAttention with the preserved worktree as the hint. Same
    /// no-teardown contract as [`finish_needs_attention`]. Only valid
    /// for a [`CaduceusError::ReviewSourceMutation`]; any other error
    /// type propagates.
    pub async fn finish_mutation_violation(
        &mut self,
        err: &crate::infra::error::CaduceusError,
    ) -> CaduceusResult<()> {
        self.teardown_quarantine_if_attached().await;
        let claim = self.take_claim();
        crate::repo::review_integrity::finish_mutation_violation(
            self.store.as_ref(),
            claim,
            &self.target,
            err,
        )?;
        self.mark_finished().await;
        Ok(())
    }

    /// Cancellation transition. Operator SIGINT/SIGTERM or a
    /// timeout-driven drain lands here. The worktree is torn down;
    /// the entry is requeued with `not_before = now` so the next tick
    /// is immediately eligible. The fork quarantine is deliberately
    /// KEPT (DAR §11.2, plan §3.2): the requeue is non-terminal, so
    /// the retried fork claim must still find and reuse the clone.
    pub async fn finish_cancelled(&mut self) -> CaduceusResult<()> {
        self.teardown_worktree_if_attached().await;
        let now = chrono::Utc::now();
        let claim = self.take_claim();
        self.store
            .requeue_infrastructure_review(claim, "operator cancellation", now)?;
        self.mark_finished().await;
        Ok(())
    }

    /// Tear down the attached fork quarantine (if any) via
    /// [`ForkQuarantine::remove`]. Runs ONLY on the terminal
    /// `finish_*` routes — Done, Skipped, NeedsAttention,
    /// mutation-violation, and the terminal `Failed` arm of
    /// `finish_retry`. The requeue routes (retry→Queued,
    /// infrastructure, cancellation) deliberately do NOT call this:
    /// removing the quarantine on a non-terminal route would make
    /// the retried fork claim fall back to the production-mirror
    /// path (DAR §11.2, plan §3.2). On the terminal routes the
    /// worktree itself is preserved for forensics (NeedsAttention)
    /// while the quarantine clone — a throwaway object store, not
    /// evidence — is removed. Idempotent: a missing clone or no
    /// attached quarantine is silently tolerated; a typed failure
    /// surfaces as a warning.
    async fn teardown_quarantine_if_attached(&self) {
        let quarantine = {
            let mut slot = self.quarantine.lock().await;
            slot.take()
        };
        if let Some(quarantine) = quarantine {
            if let Err(err) = ForkQuarantine::remove(&quarantine, &self.runner).await {
                warn!(
                    error = %err,
                    quarantine = %quarantine.path.display(),
                    "review run guard: fork quarantine teardown failed during cleanup"
                );
            }
        }
    }

    /// Tear down the attached review worktree (if any) via
    /// [`ReviewWorktree::remove`]. Idempotent: a missing path or no
    /// attached worktree is silently tolerated, but a typed failure
    /// surfaces as a warning rather than aborting the cleanup. The
    /// worktree handle is consumed regardless of outcome.
    async fn teardown_worktree_if_attached(&self) {
        let worktree = {
            let mut slot = self.worktree.lock().await;
            slot.take()
        };
        if let Some(wt) = worktree {
            if let Err(err) = ReviewWorktree::remove(&self.runner, &wt).await {
                warn!(
                    error = %err,
                    worktree = %wt.path.display(),
                    "review run guard: worktree teardown failed during cleanup"
                );
            }
        }
    }
}

impl Drop for ReviewRunGuard {
    fn drop(&mut self) {
        // Synchronous Drop cannot perform async cleanup. The contract
        // says Drop must NOT silently complete a transition, so we
        // only log an invariant violation when the orchestrator
        // forgot to call one of the `finish_*` methods.
        if let Ok(flag) = self.finished.try_lock() {
            if !*flag {
                let run_id = self
                    .claim
                    .as_ref()
                    .map(|c| c.run_id().to_string())
                    .unwrap_or_else(|| "<consumed>".to_string());
                warn!(
                    run_id = %run_id,
                    target = %format!(
                        "{}#pr/{}",
                        self.target.repository.full_name(),
                        self.target.pull_request
                    ),
                    "ReviewRunGuard dropped without calling a finish_* method; \
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
                "ReviewRunGuard dropped while finish_* lock was contended"
            );
        }
    }
}
