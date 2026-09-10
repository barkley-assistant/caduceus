//! In-tick Auto Review PR discovery + admission (issue #312, D3).
//!
//! Step 5.5 of the tick pipeline (between issue polling and the
//! queue drain, DAR SS5): for each watched repo, list pull
//! requests (`state=all`, client-side filtering per D4), classify
//! every row through the pure eligibility predicate (D5), and
//! admit genuinely new head revisions into the review queue.
//!
//! Isolation tiers (D9):
//! - per-ROW: malformed / ineligible / git errors (incl.
//!   `HeadShaUnavailable`, D8) log + count + continue;
//! - per-REPO: list errors (500s, JSON parse) log + count
//!   `failed_repos` + continue to the next repo - the issue
//!   loop's break shape (tick/mod.rs) is NOT inherited (AC2);
//! - step-level: rate limit + review-store write errors return
//!   `Err` so the call site can classify and fold the error into
//!   the tick's `last_error` - the tick NEVER returns early from
//!   the PR step (AC3).
//!
//! Admission (D11): lazy mirror bootstrap per repo (D7), SHA-
//! anchored fetch of head + base SHA (`fetch_sha`, #297),
//! merge-base computed through the mirror and persisted on
//! `ReviewTarget` (DAR SS2.1-2.2), then the atomic
//! `ReviewStore::enqueue_review` (generation + queue write, #295).
//! Nothing changed = no mirror work at all (AC7). The per-tick
//! admission budget `max_reviews_per_tick` bounds admissions
//! tick-wide and is checked BEFORE any git work (D10, AC6).
//!
//! Discovery NEVER touches the queue/state files directly and
//! never fetches diffs or context (rate-limit discipline, D4).

use std::future::Future;
use std::pin::Pin;

use tracing::{info, warn};

use crate::github::pr::list_pull_requests;
use crate::github::Client;
use crate::infra::config::{AutoReviewConfig, Config};
use crate::infra::error::{CaduceusError, CaduceusResult};

use crate::repo::fork_quarantine::ForkQuarantine;
use crate::repo::mirror::BareMirror;
use crate::review::{RepositoryId, ReviewTarget};
use crate::state::review::{ReviewEnqueueOutcome, ReviewStore};
use crate::worktree::git_runner::GitRunner;

/// Resolver seam that maps a fork PR's `head.repo.full_name` (e.g.
/// `forkuser/r`) to the fork's git clone URL for the quarantine
/// fetch (issue #337 Phase 2). The tick wires the GitHub REST
/// `repos/{owner}/{repo}` lookup; tests wire local fixtures.
pub type ForkRemoteResolver = dyn Fn(&RepositoryId, &str) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>
    + Send
    + Sync;

// ---------------------------------------------------------------------------
// Event constants + emission shape (DAR SS13, D2; fork-gate pattern)
// ---------------------------------------------------------------------------

/// DAR SS13 discovery event: a PR row passed eligibility (may be
/// admitted this tick, budget permitting).
pub const DISCOVERED_EVENT: &str = "review_discovered";
/// DAR SS13 discovery event: `enqueue_review` inserted the target.
pub const ADMITTED_EVENT: &str = "review_admitted";
/// DAR SS5.1 skip event: draft PR with `draft_pull_requests: false`.
pub const SKIPPED_DRAFT_EVENT: &str = "review_skipped_draft";
/// DAR SS5.1 skip event: the exact head SHA is already active in the
/// review queue or was already reviewed.
pub const SKIPPED_ALREADY_COMPLETE_EVENT: &str = "review_skipped_already_complete";
/// DAR SS5.1 poll event: the PR's head moved relative to a SHA the
/// daemon held (active entry or `last_reviewed_head_sha`).
pub const STALE_SHA_EVENT: &str = "review_stale_sha_observed";

/// Emit `review_discovered` (D5 step: a row passed eligibility).
fn emit_discovered(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = DISCOVERED_EVENT,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "PR revision discovered for review"
    );
}

/// Emit `review_admitted` (D2: DAR SS13 assigns this event to
/// discovery; fired only when `enqueue_review` returned `Inserted`).
fn emit_admitted(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = ADMITTED_EVENT,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "PR revision admitted into the review queue"
    );
}

/// Emit `review_skipped_draft` (DAR SS5.1).
fn emit_skipped_draft(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = SKIPPED_DRAFT_EVENT,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "PR skipped: draft"
    );
}

/// Emit `review_skipped_already_complete` (DAR SS5.1: dedup skip).
fn emit_skipped_already_complete(repo: &str, pr: u64, head_sha: &str) {
    info!(
        target: "caduceus",
        event = SKIPPED_ALREADY_COMPLETE_EVENT,
        repo = repo,
        pr = pr,
        head_sha = head_sha,
        "PR skipped: head SHA already queued or reviewed"
    );
}

/// Emit `review_stale_sha_observed` (D6: the held SHA moved).
fn emit_stale_sha(repo: &str, pr: u64, previous_sha: &str, observed_sha: &str) {
    info!(
        target: "caduceus",
        event = STALE_SHA_EVENT,
        repo = repo,
        pr = pr,
        previous_sha = previous_sha,
        observed_sha = observed_sha,
        "PR head moved since the last held revision"
    );
}

// ---------------------------------------------------------------------------
// Core types (plan SS4.3 contract)
// ---------------------------------------------------------------------------

/// What discovery holds for one `(repo, pr)` (D6): the head SHAs of
/// ACTIVE queue entries and the completed-review pointer.
///
/// Public so integration tests can build the dedup fixtures
/// (`classify_discovery_row_for_tests`); no behaviour surface.
#[derive(Clone, Debug)]
pub struct HeldShas {
    /// Active (`Queued`/`InProgress`) queue entries' head SHAs.
    pub active: Vec<String>,
    /// `ReviewState.last_reviewed_head_sha` of the completed review.
    pub last_reviewed: Option<String>,
}

/// One row's verdict (D5). `Ineligible` and `Malformed` carry NO
/// event; `SkipDraft` / `SkipFork` / `SkipAlreadyComplete` carry
/// their SS13 events at the emit site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAction {
    /// Admit (subject to the tick-wide budget, D10).
    Admit,
    /// Draft + `draft_pull_requests: false` -> `review_skipped_draft`.
    SkipDraft,
    /// Fork gate failed -> existing `emit_fork_gate_skip`.
    SkipFork { head_repo: Option<String> },
    /// Fork gate failed BUT `fork_policy` allows the fork -> admit
    /// through the quarantine fetch path (#337, Phase 2).
    AdmitFork { head_repo: String },
    /// Dedup hit -> `review_skipped_already_complete`.
    SkipAlreadyComplete,
    /// Closed / merged - never admitted, NO event (DAR SS5.1).
    Ineligible,
    /// Unreadable wire row - log-only, NO event (DAR SS13 defines no
    /// malformed-row event; mirrors `IssuePollDiagnostic::Malformed`).
    Malformed { reason: String },
}

/// One row's decision (D5 + D6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowDecision {
    pub action: RowAction,
    /// `(previous_sha, observed_sha)` when the PR's held SHA moved (D6).
    pub stale: Option<(String, String)>,
    /// Observed head SHA when readable.
    pub head_sha: String,
    pub base_sha: String,
    pub base_ref: String,
}

/// Tick-wide discovery counters (plan SS4.3; log at step end).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReviewDiscoveryStats {
    pub repos_polled: u32,
    pub discovered: u32,
    pub admitted: u32,
    pub skipped_draft: u32,
    pub skipped_fork: u32,
    pub skipped_already_complete: u32,
    pub ineligible: u32,
    pub malformed: u32,
    pub stale_observed: u32,
    pub skipped_unavailable_sha: u32,
    pub failed_admissions: u32,
    pub failed_repos: u32,
    pub budget_exhausted: bool,
}

// ---------------------------------------------------------------------------
// Pure eligibility classifier (Task 4, D5 - no I/O, no logging, never
// panics on any wire shape; mirrors classify_fork's contract)
// ---------------------------------------------------------------------------

/// Classify one `/pulls` row through the D5 eligibility order:
/// malformed -> closed/merged -> draft -> fork -> dedup ->
/// stale-observe -> admit. Pure: no I/O, no logging, never panics.
///
/// Public via [`classify_discovery_row_for_tests`] (the integration
/// seam, D12); production callers use this directly.
pub(crate) fn classify_discovery_row(
    row: &crate::github::pr::PullRequestDetail,
    ar: &AutoReviewConfig,
    held: &HeldShas,
) -> RowDecision {
    // 2. Malformed rows (D5 step 2): missing identity or SHA context.
    let Some(_) = row.number else {
        return malformed("missing number");
    };
    let Some(base) = row.base.as_ref() else {
        return malformed("missing base");
    };
    let Some(base_sha) = base.sha.as_deref() else {
        return malformed("missing base.sha");
    };
    let Some(base_ref) = base.ref_name.as_deref() else {
        return malformed("missing base.ref");
    };
    let Some(head) = row.head.as_ref() else {
        return malformed("missing head");
    };
    let Some(head_sha) = head.sha.as_deref() else {
        return malformed("missing head.sha");
    };
    if head_sha.is_empty() || base_sha.is_empty() {
        return malformed("empty SHA");
    }

    // 3. Closed / merged rows are never admitted - NO event (DAR SS5.1).
    if row.state.as_deref() != Some("open") || row.merged == Some(true) {
        return RowDecision {
            action: RowAction::Ineligible,
            stale: None,
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            base_ref: base_ref.to_string(),
        };
    }

    // 4. Draft gate (DAR SS5.1): `review_skipped_draft` unless the
    //    operator opted in.
    if row.draft && !ar.draft_pull_requests {
        return RowDecision {
            action: RowAction::SkipDraft,
            stale: None,
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            base_ref: base_ref.to_string(),
        };
    }

    // 5. Fork gate (Phase-1 contract, #316): every non-SameRepo
    //    verdict skips with the existing SS13 event; `head.repo: null`
    //    (deleted head branch) lands here as `HeadRepoMissing`. Phase 2
    //    (#337): when the gate DENIES but the BASE (watched) repo is
    //    in `fork_policy.allow_fork_prs`, the row admits through the
    //    quarantine path instead (`RowAction::AdmitFork`). The
    //    allow-list names WATCHED BASE repo slugs — the fork's head
    //    repo identity is attacker-controlled and never consulted for
    //    the policy decision, only for the `AdmitFork` payload. The
    //    gate itself stays pure and unchanged.
    let fork = crate::github::fork_gate::classify_fork(row);
    if !fork.passes() {
        let head_repo = fork.head_repo_identity().map(str::to_string);
        let base_identity = row
            .base
            .as_ref()
            .and_then(|b| b.repo.as_ref())
            .and_then(|r| r.full_name.as_deref());
        let base_allowed = base_identity.is_some_and(|identity| {
            ar.fork_policy
                .as_ref()
                .is_some_and(|p| p.is_allowed(identity))
        });
        if let Some(head_repo) = head_repo.as_deref() {
            if base_allowed {
                return RowDecision {
                    action: RowAction::AdmitFork {
                        head_repo: head_repo.to_string(),
                    },
                    stale: None,
                    head_sha: head_sha.to_string(),
                    base_sha: base_sha.to_string(),
                    base_ref: base_ref.to_string(),
                };
            }
        }
        return RowDecision {
            action: RowAction::SkipFork { head_repo },
            stale: None,
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            base_ref: base_ref.to_string(),
        };
    }

    // 6. Dedup: the exact `(repo, pr, head_sha)` is already active in
    //    the queue or already reviewed -> `review_skipped_already_
    //    complete` (DAR SS4.3 pointer + active-only dedup; history is
    //    never consulted).
    let already_held = held.active.iter().any(|sha| sha == head_sha)
        || held.last_reviewed.as_deref() == Some(head_sha);
    if already_held {
        return RowDecision {
            action: RowAction::SkipAlreadyComplete,
            stale: None,
            head_sha: head_sha.to_string(),
            base_sha: base_sha.to_string(),
            base_ref: base_ref.to_string(),
        };
    }

    // 7. Stale observation (D6): ANY other held SHA differs from the
    //    observed head -> emit once at the emit site and CONTINUE -
    //    the observed head is itself genuinely new and admits now.
    let stale = held
        .active
        .first()
        .cloned()
        .or_else(|| held.last_reviewed.clone())
        .filter(|previous| previous != head_sha)
        .map(|previous| (previous, head_sha.to_string()));

    // 8. Admit (budget permitting at the caller, D10).
    RowDecision {
        action: RowAction::Admit,
        stale,
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
        base_ref: base_ref.to_string(),
    }
}

fn malformed(reason: &str) -> RowDecision {
    RowDecision {
        action: RowAction::Malformed {
            reason: reason.to_string(),
        },
        stale: None,
        head_sha: String::new(),
        base_sha: String::new(),
        base_ref: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Admission (Task 5, D11)
// ---------------------------------------------------------------------------

/// Prepare a full [`ReviewTarget`] for one PR revision: SHA-anchored
/// fetch of head + base into the mirror, then merge-base computation
/// (DAR §2.1). Git errors propagate as per-target (caller logs +
/// continues). Shared by auto-discovery (`admit_target`) and the
/// trusted-comment re-review listener (#335, DAR §17) so the mirror
/// fetch + merge-base logic cannot diverge between the two paths.
#[allow(clippy::too_many_arguments)] // fixed 7-arg preparation contract
pub(crate) async fn prepare_review_target(
    runner: &GitRunner,
    mirror: &BareMirror,
    repository: &RepositoryId,
    pull_request: u64,
    head_sha: &str,
    base_sha: &str,
    base_ref: &str,
) -> CaduceusResult<ReviewTarget> {
    // 2. SHA-anchored head fetch (D8: unavailable -> skip, no event;
    //    #339 owns the skip routing).
    mirror.fetch_sha(runner, head_sha).await?;
    // 3. Base fetch - both objects are then guaranteed present
    //    locally (D11: do NOT rely on the base-branch fetch having
    //    landed the wire's base.sha).
    mirror.fetch_sha(runner, base_sha).await?;
    // 4. Merge base (DAR §2.1). Unrelated histories fail as
    //    `CaduceusError::Git` -> per-target log-and-skip (caller).
    let merge_base = mirror.merge_base(runner, base_sha, head_sha).await?;
    // 5. Build the full ReviewTarget (merge_base populated; validation
    //    happens inside enqueue_review).
    Ok(ReviewTarget {
        repository: repository.clone(),
        pull_request,
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
        base_ref: base_ref.to_string(),
        merge_base,
    })
}

/// One admission: prepare the target, enqueue atomically (D11). Git
/// errors are per-target (caller logs + continues); store-write
/// errors propagate as step-level (D9). Returns `Ok(true)` when the
/// target was inserted, `Ok(false)` when a concurrent admission
/// already held it or the SHAs were unavailable (D8).
#[allow(clippy::too_many_arguments)] // plan D11 surface: fixed 8-arg admission contract
pub(crate) async fn admit_target(
    runner: &GitRunner,
    mirror: &BareMirror,
    review_store: &ReviewStore,
    repository: &RepositoryId,
    pull_request: u64,
    head_sha: &str,
    base_sha: &str,
    base_ref: &str,
) -> CaduceusResult<bool> {
    let target = prepare_review_target(
        runner,
        mirror,
        repository,
        pull_request,
        head_sha,
        base_sha,
        base_ref,
    )
    .await?;
    // 6. Atomic generation + queue write (#295).
    match review_store.enqueue_review(&target)? {
        ReviewEnqueueOutcome::Inserted => Ok(true),
        ReviewEnqueueOutcome::AlreadyPresent => Ok(false),
    }
}

/// One quarantine-path admission for an ALLOWED fork PR (#337,
/// Phase 2). Analogue of [`admit_target`] with the fork fetch story
/// swapped in: the base objects come from `git clone --bare
/// --no-tags <base_url>` (the TRUSTED origin), the fork head SHA is
/// fetched SHA-anchored from the fork URL into the quarantine clone,
/// and the merge base is computed INSIDE the quarantine — the
/// production mirror is never consulted for fork runs (§11.2). The
/// enqueued `ReviewTarget` is the same shape, so downstream
/// review-worktree creation reuses the existing code with the
/// quarantine clone as its `BareMirror`.
///
/// Cleanup: on ANY failure here (fetch, merge-base, enqueue), the
/// quarantine clone is removed immediately — no attacker-triggerable
/// accumulation. The per-tick orphan sweep is the crash backstop.
#[allow(clippy::too_many_arguments)] // fixed fork admission contract (#337 §3.2)
pub(crate) async fn admit_fork_target(
    runner: &GitRunner,
    review_store: &ReviewStore,
    repository: &RepositoryId,
    pull_request: u64,
    head_sha: &str,
    base_sha: &str,
    base_ref: &str,
    head_repo: &str,
    fork_url: &str,
    base_url: &str,
) -> CaduceusResult<bool> {
    let state_dir = review_store.state_dir().to_path_buf();
    let quarantine = ForkQuarantine::create(
        runner,
        &state_dir,
        &repository.owner,
        &repository.repo,
        pull_request,
        head_sha,
        base_sha,
        base_url,
        head_repo,
    )
    .await?;

    if let Err(err) = quarantine.fetch_fork_sha(runner, fork_url, head_sha).await {
        let _ = ForkQuarantine::remove(&quarantine, runner).await;
        return Err(err);
    }

    let merge_base = match quarantine.merge_base(runner, base_sha, head_sha).await {
        Ok(mb) => mb,
        Err(err) => {
            let _ = ForkQuarantine::remove(&quarantine, runner).await;
            return Err(err);
        }
    };

    let target = ReviewTarget {
        repository: repository.clone(),
        pull_request,
        head_sha: head_sha.to_string(),
        base_sha: base_sha.to_string(),
        base_ref: base_ref.to_string(),
        merge_base,
    };

    match review_store.enqueue_review(&target) {
        Ok(ReviewEnqueueOutcome::Inserted) => Ok(true),
        Ok(ReviewEnqueueOutcome::AlreadyPresent) => {
            // The run may already be claimed/in-flight and still
            // needs the quarantine mirror for its worktree; leave the
            // clone in place (the guard tears it down at terminal
            // status, the orphan sweep is the crash backstop).
            Ok(false)
        }
        Err(err) => {
            let _ = ForkQuarantine::remove(&quarantine, runner).await;
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// The discovery loop (Task 6, D5/D9/D10/D11/D12)
// ---------------------------------------------------------------------------

/// Step 5.5 of the tick: per-repo PR discovery + admission.
///
/// Returns `Err` ONLY for step-level failures (rate limit while
/// listing; review-store write errors). Everything per-repo /
/// per-row is logged + counted (D9).
pub(crate) async fn poll_review_step(
    repos: &[String],
    client: &Client,
    cfg: &Config,
    review_store: &ReviewStore,
    runner: &GitRunner,
    resolve_remote: &dyn Fn(&str, &str) -> CaduceusResult<String>,
    resolve_fork_remote: &ForkRemoteResolver,
) -> CaduceusResult<ReviewDiscoveryStats> {
    let ar = match cfg.auto_review() {
        Some(ar) => ar,
        // Whole step gated on `auto_review.enabled` (D5 step 1).
        None => return Ok(ReviewDiscoveryStats::default()),
    };
    if !ar.enabled {
        return Ok(ReviewDiscoveryStats::default());
    }

    let budget = cfg.max_reviews_per_tick;
    let mut stats = ReviewDiscoveryStats::default();
    // Held SHAs read once per repo via one snapshot (D11/D9: a read
    // path over the store, no direct file access).
    let mut mirrors: std::collections::BTreeMap<String, BareMirror> =
        std::collections::BTreeMap::new();

    for repo in repos {
        // Parse the `owner/repo` slug. Malformed slugs cannot happen
        // for validated config / API discovery, but a safety net
        // keeps the row classifier total.
        let Some((owner, name)) = repo.split_once('/') else {
            warn!(
                target: "caduceus",
                repo = repo,
                "review discovery: malformed repo slug; skipping"
            );
            stats.failed_repos += 1;
            continue;
        };

        let pulls = match list_pull_requests(client, owner, name).await {
            Ok(pulls) => pulls,
            // Per-REPO tier (D9): list errors log + count + continue.
            // A global rate limit is a step-level condition - later
            // repos would fail identically, so surface it (D9).
            Err(err @ CaduceusError::RateLimited { .. }) => return Err(err),
            Err(err) => {
                warn!(
                    target: "caduceus",
                    error = %err,
                    repo = repo,
                    "review discovery: PR list failed; continuing to next repo"
                );
                stats.failed_repos += 1;
                continue;
            }
        };
        stats.repos_polled += 1;

        for row in &pulls {
            // Budget check BEFORE any per-row work so an exhausted
            // budget does no git work (D10). Deferred candidates are
            // re-discovered next tick - no `review_discovered` event
            // for them.
            if budget != 0 && stats.admitted >= budget {
                stats.budget_exhausted = true;
                break;
            }

            let Some(pr_number) = row.number else {
                stats.malformed += 1;
                continue;
            };

            // Held SHAs for this (repo, pr): active queue entries +
            // the completed-review pointer (D6). Read per row through
            // the store's public read API.
            let held = read_held_shas(review_store, owner, name, pr_number)?;

            let decision = classify_discovery_row(row, ar, &held);

            // Emit the stale observation BEFORE the action event so
            // the log reads chronologically (D6).
            if let Some((previous, observed)) = &decision.stale {
                emit_stale_sha(repo, pr_number, previous, observed);
                stats.stale_observed += 1;
            }

            match decision.action {
                RowAction::Admit => {
                    emit_discovered(repo, pr_number, &decision.head_sha);
                    stats.discovered += 1;

                    // Lazy mirror bootstrap (D7): only when this repo
                    // has at least one candidate that passed
                    // eligibility + budget (AC7).
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
                                        "review discovery: remote resolve failed; skipping repo"
                                    );
                                    stats.failed_repos += 1;
                                    break;
                                }
                            };
                            match BareMirror::ensure(
                                runner,
                                cfg,
                                owner,
                                name,
                                &remote,
                                &decision.base_ref,
                            )
                            .await
                            {
                                Ok(m) => {
                                    mirrors.insert(repo.to_string(), m);
                                    mirrors.get(repo).expect("just inserted")
                                }
                                Err(err) => {
                                    warn!(
                                        target: "caduceus",
                                        error = %err,
                                        repo = repo,
                                        "review discovery: mirror ensure failed; skipping repo"
                                    );
                                    stats.failed_repos += 1;
                                    break;
                                }
                            }
                        }
                    };

                    let repository_id = RepositoryId {
                        owner: owner.to_string(),
                        repo: name.to_string(),
                    };
                    match admit_target(
                        runner,
                        mirror,
                        review_store,
                        &repository_id,
                        pr_number,
                        &decision.head_sha,
                        &decision.base_sha,
                        &decision.base_ref,
                    )
                    .await
                    {
                        Ok(true) => {
                            emit_admitted(repo, pr_number, &decision.head_sha);
                            stats.admitted += 1;
                        }
                        Ok(false) => {
                            // A concurrent admission won the race or
                            // the SHAs were unavailable (D8). Debug
                            // log, no event.
                            info!(
                                target: "caduceus",
                                repo = repo,
                                pr = pr_number,
                                "review discovery: target already present; skipping"
                            );
                        }
                        Err(err) => {
                            // D9 per-row isolation: git errors from the
                            // admission itself (mirror fetch, merge
                            // base - including `HeadShaUnavailable`,
                            // D8) log + count + continue. Only
                            // store/state write errors (from
                            // `enqueue_review`) and cancellation
                            // propagate as step-level so the call
                            // site can classify them.
                            match err {
                                CaduceusError::HeadShaUnavailable { .. } => {
                                    stats.skipped_unavailable_sha += 1;
                                    info!(
                                        target: "caduceus",
                                        repo = repo,
                                        pr = pr_number,
                                        "review discovery: head SHA unavailable; skipping (next poll retries)"
                                    );
                                }
                                // Git transport / merge-base failure
                                // for this target only (e.g. unrelated
                                // histories): the next poll retries.
                                CaduceusError::Git { .. } => {
                                    warn!(
                                        target: "caduceus",
                                        error = %err,
                                        repo = repo,
                                        pr = pr_number,
                                        "review discovery: admission failed; skipping target"
                                    );
                                    stats.failed_admissions += 1;
                                }
                                // Store/state write errors and
                                // cancellation are step-level (D9).
                                other => return Err(other),
                            }
                        }
                    }
                }
                RowAction::SkipDraft => {
                    emit_skipped_draft(repo, pr_number, &decision.head_sha);
                    stats.skipped_draft += 1;
                }
                RowAction::SkipFork { head_repo } => {
                    crate::github::fork_gate::emit_fork_gate_skip(
                        repo,
                        pr_number,
                        head_repo.as_deref(),
                    );
                    stats.skipped_fork += 1;
                }
                RowAction::AdmitFork { head_repo } => {
                    emit_discovered(repo, pr_number, &decision.head_sha);
                    stats.discovered += 1;

                    let repository_id = RepositoryId {
                        owner: owner.to_string(),
                        repo: name.to_string(),
                    };

                    // The fork URL comes from the resolver seam (the
                    // daemon wires the GitHub REST `repos/{owner}/
                    // {repo}` lookup; tests wire local fixtures). An
                    // unresolvable fork is a per-row skip with the
                    // existing unavailable-SHA event — the next poll
                    // retries.
                    let Some(fork_url) = resolve_fork_remote(&repository_id, &head_repo).await
                    else {
                        stats.skipped_unavailable_sha += 1;
                        info!(
                            target: "caduceus",
                            repo = repo,
                            pr = pr_number,
                            fork_repo = head_repo,
                            "review discovery: fork remote unresolvable; skipping (next poll retries)"
                        );
                        continue;
                    };

                    // Base URL for the trusted-origin clone. Same
                    // repo-level failure contract as mirror ensure.
                    let base_url = match resolve_remote(owner, name) {
                        Ok(remote) => remote,
                        Err(err) => {
                            warn!(
                                target: "caduceus",
                                error = %err,
                                repo = repo,
                                "review discovery: remote resolve failed; skipping repo"
                            );
                            stats.failed_repos += 1;
                            break;
                        }
                    };

                    match admit_fork_target(
                        runner,
                        review_store,
                        &repository_id,
                        pr_number,
                        &decision.head_sha,
                        &decision.base_sha,
                        &decision.base_ref,
                        &head_repo,
                        &fork_url,
                        &base_url,
                    )
                    .await
                    {
                        Ok(true) => {
                            emit_admitted(repo, pr_number, &decision.head_sha);
                            stats.admitted += 1;
                        }
                        Ok(false) => {
                            info!(
                                target: "caduceus",
                                repo = repo,
                                pr = pr_number,
                                "review discovery: fork target already present; skipping"
                            );
                        }
                        Err(err) => match err {
                            CaduceusError::HeadShaUnavailable { .. } => {
                                stats.skipped_unavailable_sha += 1;
                                info!(
                                    target: "caduceus",
                                    repo = repo,
                                    pr = pr_number,
                                    "review discovery: fork head SHA unavailable; skipping (next poll retries)"
                                );
                            }
                            CaduceusError::Git { .. } => {
                                warn!(
                                    target: "caduceus",
                                    error = %err,
                                    repo = repo,
                                    pr = pr_number,
                                    "review discovery: fork admission failed; skipping target"
                                );
                                stats.failed_admissions += 1;
                            }
                            other => return Err(other),
                        },
                    }
                }
                RowAction::SkipAlreadyComplete => {
                    emit_skipped_already_complete(repo, pr_number, &decision.head_sha);
                    stats.skipped_already_complete += 1;
                }
                RowAction::Ineligible => {
                    stats.ineligible += 1;
                }
                RowAction::Malformed { reason } => {
                    warn!(
                        target: "caduceus",
                        repo = repo,
                        reason = reason,
                        "review discovery: malformed PR row; skipping"
                    );
                    stats.malformed += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Read the held SHAs for one `(repo, pr)` (D6): active queue
/// entries' head SHAs + `ReviewState.last_reviewed_head_sha`.
/// Store-read errors propagate as step-level (D9).
fn read_held_shas(
    review_store: &ReviewStore,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> CaduceusResult<HeldShas> {
    let repository = RepositoryId {
        owner: owner.to_string(),
        repo: name.to_string(),
    };
    let snapshot = review_store.review_queue_snapshot()?;
    let active: Vec<String> = snapshot
        .entries
        .values()
        .filter(|entry| {
            entry.phase.is_active()
                && entry.target.repository.owner == repository.owner
                && entry.target.repository.repo == repository.repo
                && entry.target.pull_request == pr_number
        })
        .map(|entry| entry.target.head_sha.clone())
        .collect();
    let state = review_store.review_state(&repository, pr_number)?;
    let last_reviewed = state.and_then(|s| s.last_reviewed_head_sha);
    Ok(HeldShas {
        active,
        last_reviewed,
    })
}

/// Public test seam (plan SS4.3, D12): mirrors
/// `poll_awaiting_review_entries_for_tests`. Same body as
/// `poll_review_step`.
pub async fn poll_review_step_for_tests(
    repos: &[String],
    client: &Client,
    cfg: &Config,
    review_store: &ReviewStore,
    runner: &GitRunner,
    resolve_remote: &dyn Fn(&str, &str) -> CaduceusResult<String>,
    resolve_fork_remote: &ForkRemoteResolver,
) -> CaduceusResult<ReviewDiscoveryStats> {
    poll_review_step(
        repos,
        client,
        cfg,
        review_store,
        runner,
        resolve_remote,
        resolve_fork_remote,
    )
    .await
}

/// Public test seam (D12): the pure eligibility classifier. Same body
/// as `classify_discovery_row`.
pub fn classify_discovery_row_for_tests(
    row: &crate::github::pr::PullRequestDetail,
    ar: &AutoReviewConfig,
    held: &HeldShas,
) -> RowDecision {
    classify_discovery_row(row, ar, held)
}

/// Public test seam (D12): the structured `review_discovered`
/// emitter, for event-capture tests.
pub fn emit_discovered_for_tests(repo: &str, pr: u64, head_sha: &str) {
    emit_discovered(repo, pr, head_sha)
}

/// Public test seam (D12): the structured `review_admitted` emitter,
/// for event-capture tests.
pub fn emit_admitted_for_tests(repo: &str, pr: u64, head_sha: &str) {
    emit_admitted(repo, pr, head_sha)
}

/// Public test seam (D12): the structured `review_skipped_draft`
/// emitter, for event-capture tests.
pub fn emit_skipped_draft_for_tests(repo: &str, pr: u64, head_sha: &str) {
    emit_skipped_draft(repo, pr, head_sha)
}

/// Public test seam (D12): the structured
/// `review_skipped_already_complete` emitter, for event-capture
/// tests.
pub fn emit_skipped_already_complete_for_tests(repo: &str, pr: u64, head_sha: &str) {
    emit_skipped_already_complete(repo, pr, head_sha)
}

/// Public test seam (D12): the structured `review_stale_sha_observed`
/// emitter, for event-capture tests.
pub fn emit_stale_sha_for_tests(repo: &str, pr: u64, previous_sha: &str, observed_sha: &str) {
    emit_stale_sha(repo, pr, previous_sha, observed_sha)
}

/// Public test seam (D12): one admission (fetch head SHA, fetch base
/// SHA, merge base, atomic enqueue). Same body as `admit_target`.
#[allow(clippy::too_many_arguments)]
pub async fn admit_target_for_tests(
    runner: &GitRunner,
    mirror: &BareMirror,
    review_store: &ReviewStore,
    repository: &RepositoryId,
    pull_request: u64,
    head_sha: &str,
    base_sha: &str,
    base_ref: &str,
) -> CaduceusResult<bool> {
    admit_target(
        runner,
        mirror,
        review_store,
        repository,
        pull_request,
        head_sha,
        base_sha,
        base_ref,
    )
    .await
}
