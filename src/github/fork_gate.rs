//! Phase-1 fork gate for PR discovery (issue #316).
//!
//! Fork pull requests are **unsupported** in Phase 1 (DAR §5.1, §11.2):
//! discovery skips them unconditionally because the daemon's
//! single-origin mirror cannot check out fork SHAs. This module owns
//! the eligibility predicate and the structured skip event; #312 wires
//! them into the discovery loop. There is deliberately **no config
//! knob** — dead config would advertise a posture the system cannot
//! deliver (DAR §11.2).
//!
//! The predicate is fail-closed by construction: only a *proven*
//! same-repo row passes the gate. Every other wire shape — fork,
//! missing head repo (deleted head branch), missing base repo —
//! yields a non-passing verdict and, at the emit site, the
//! [`FORK_SKIP_EVENT`] (DAR §13 registry). `classify_fork` is pure:
//! no I/O, no logging, no panics on any wire shape.

use crate::github::pr::PullRequestDetail;

/// DAR §13 event name for the Phase-1 fork-gate skip.
pub const FORK_SKIP_EVENT: &str = "review_skipped_fork_unsupported";

/// Verdict of the Phase-1 fork gate for one PR row (DAR §5.1, §11.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkStatus {
    /// `head.repo.full_name == base.repo.full_name`, both present —
    /// the ONLY verdict that passes the gate.
    SameRepo,
    /// Proven fork: both full names present and different. Carries the
    /// head repo's full name for the skip event.
    Fork { head_repo: String },
    /// Head side unidentifiable: `head` absent, `head.repo: null`
    /// (deleted head branch), or `head.repo.full_name` absent.
    HeadRepoMissing,
    /// Base side unidentifiable (never seen on real payloads;
    /// wire-model defense). Carries the head identity that was
    /// readable. Fails closed.
    BaseRepoMissing { head_repo: String },
}

impl ForkStatus {
    /// Does this verdict pass the Phase-1 fork gate?
    pub fn passes(&self) -> bool {
        matches!(self, ForkStatus::SameRepo)
    }

    /// Head-repo identity for the skip-event payload (DAR §5.1:
    /// "head-repo identity or null"). `Some(full_name)` whenever the
    /// head side was readable, `None` for `SameRepo` (never emitted)
    /// and `HeadRepoMissing`.
    pub fn head_repo_identity(&self) -> Option<&str> {
        match self {
            ForkStatus::SameRepo | ForkStatus::HeadRepoMissing => None,
            ForkStatus::Fork { head_repo } | ForkStatus::BaseRepoMissing { head_repo } => {
                Some(head_repo)
            }
        }
    }
}

/// Classify a PR row for the Phase-1 fork gate. Pure: no I/O, no
/// logging, never panics on any wire shape. Evaluation order: head
/// side, then base side, then exact string comparison.
pub fn classify_fork(pr: &PullRequestDetail) -> ForkStatus {
    let head_full = pr
        .head
        .as_ref()
        .and_then(|b| b.repo.as_ref())
        .and_then(|r| r.full_name.as_deref());
    let Some(head_full) = head_full else {
        return ForkStatus::HeadRepoMissing;
    };
    let base_full = pr
        .base
        .as_ref()
        .and_then(|b| b.repo.as_ref())
        .and_then(|r| r.full_name.as_deref());
    let Some(base_full) = base_full else {
        return ForkStatus::BaseRepoMissing {
            head_repo: head_full.to_string(),
        };
    };
    if head_full == base_full {
        ForkStatus::SameRepo
    } else {
        ForkStatus::Fork {
            head_repo: head_full.to_string(),
        }
    }
}

/// Emit the DAR §13 fork-gate skip event: repo, PR number, head-repo
/// identity (or `"null"`). `tracing::info!`, `target: "caduceus"`.
pub fn emit_fork_gate_skip(repo: &str, pr_number: u64, head_repo: Option<&str>) {
    tracing::info!(
        target: "caduceus",
        event = FORK_SKIP_EVENT,
        repo = repo,
        pr = pr_number,
        head_repo = head_repo.unwrap_or("null"),
        "PR skipped: fork review unsupported in Phase 1"
    );
}
