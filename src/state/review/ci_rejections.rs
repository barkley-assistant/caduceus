//! Durable CI-failure rejection set for Auto Review (issue #435).
//!
//! A head SHA that failed the CI gate is recorded once so later polls
//! hit the existing already-complete dedup without a further CI
//! request. This is a sibling of the three review-era files — never
//! written onto `ReviewState.last_reviewed_head_sha` (that field means
//! a review finished and would bump generation on the first real
//! review).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::review::RepositoryId;
use crate::state::review::review_state_key;

/// Cap on retained red head SHAs per `(owner, repo, pr)`.
/// Older entries are dropped on insert so the set cannot grow
/// without bound across force-push cycles.
pub const MAX_CI_REJECTIONS_PER_PR: usize = 32;

/// One rejected `(repo, pr, head_sha)` triple.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiRejection {
    pub repository: RepositoryId,
    pub pull_request: u64,
    pub head_sha: String,
}

/// Versioned `review_ci_rejections.json` envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiRejectionFile {
    pub version: u32,
    pub rejections: Vec<CiRejection>,
}

impl CiRejectionFile {
    pub fn empty() -> Self {
        Self {
            version: crate::state::review::REVIEW_CI_REJECTIONS_FILE_VERSION,
            rejections: Vec::new(),
        }
    }

    /// Head SHAs recorded as CI-failed for one `(repo, pr)`.
    pub fn head_shas_for(&self, repository: &RepositoryId, pull_request: u64) -> BTreeSet<String> {
        let want = review_state_key(repository, pull_request);
        self.rejections
            .iter()
            .filter(|r| review_state_key(&r.repository, r.pull_request) == want)
            .map(|r| r.head_sha.clone())
            .collect()
    }

    /// Insert if absent. Caps retained SHAs per `(repo, pr)` at
    /// [`MAX_CI_REJECTIONS_PER_PR`], dropping the oldest for that
    /// key when the cap would be exceeded.
    pub fn insert(&mut self, rejection: CiRejection) {
        if self.rejections.iter().any(|r| {
            r.pull_request == rejection.pull_request
                && r.head_sha == rejection.head_sha
                && r.repository
                    .owner
                    .eq_ignore_ascii_case(&rejection.repository.owner)
                && r.repository
                    .repo
                    .eq_ignore_ascii_case(&rejection.repository.repo)
        }) {
            return;
        }
        let want = review_state_key(&rejection.repository, rejection.pull_request);
        let mut for_pr = 0usize;
        for r in &self.rejections {
            if review_state_key(&r.repository, r.pull_request) == want {
                for_pr += 1;
            }
        }
        while for_pr >= MAX_CI_REJECTIONS_PER_PR {
            if let Some(idx) = self
                .rejections
                .iter()
                .position(|r| review_state_key(&r.repository, r.pull_request) == want)
            {
                self.rejections.remove(idx);
                for_pr -= 1;
            } else {
                break;
            }
        }
        self.rejections.push(rejection);
    }
}
