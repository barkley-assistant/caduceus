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

    /// Insert if absent.
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
        self.rejections.push(rejection);
    }
}
