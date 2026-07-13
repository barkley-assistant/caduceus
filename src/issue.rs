//! Typed issue identity and detail schema. The shape is normative and is
//! re-exported from `lib.rs`. Validation rules live in this module per
//! `CONTRACTS.md` "Issue identity and queue schema".

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CaduceusError, CaduceusResult};

/// GitHub-canonical issue identifier: `(owner, repo, number)`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct IssueKey {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl IssueKey {
    /// Lowercased `owner/repo#number` form used as a queue-key and to
    /// derive the on-disk claim filename via SHA-256.
    pub fn display_key(&self) -> String {
        format!(
            "{}/{}{}{}",
            self.owner.to_ascii_lowercase(),
            self.repo.to_ascii_lowercase(),
            '#',
            self.number
        )
    }

    /// Parse an `owner/repo#number` reference. The input may have
    /// any casing for owner/repo; validation normalises the casing
    /// rules but preserves the original case in the returned
    /// struct (so API paths use GitHub's canonical case). Returns
    /// a [`CaduceusError::Config`] for malformed input — never
    /// panics.
    pub fn parse(input: &str) -> CaduceusResult<Self> {
        let (head, number_text) = input
            .split_once('#')
            .ok_or_else(|| CaduceusError::Config(format!("issue ref missing '#': {input}")))?;
        let (owner, repo) = head
            .split_once('/')
            .ok_or_else(|| CaduceusError::Config(format!("issue ref missing '/': {input}")))?;
        if owner.is_empty() || repo.is_empty() {
            return Err(CaduceusError::Config(format!(
                "issue ref has empty owner or repo: {input}"
            )));
        }
        if repo.contains('/') {
            return Err(CaduceusError::Config(format!(
                "issue ref has extra '/': {input}"
            )));
        }
        let number = number_text.parse::<u64>().map_err(|err| {
            CaduceusError::Config(format!("issue ref number parse: {input} ({err})"))
        })?;
        if number == 0 {
            return Err(CaduceusError::Config(format!(
                "issue number must be positive: {input}"
            )));
        }
        let key = Self {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        };
        key.validate()?;
        Ok(key)
    }

    /// Validate identifier components per `CONTRACTS.md`.
    pub fn validate(&self) -> CaduceusResult<()> {
        validate_owner(&self.owner)?;
        validate_repo(&self.repo)?;
        if self.number == 0 {
            return Err(CaduceusError::Other(
                "issue number must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

impl fmt::Display for IssueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// Full issue detail assembled from the GitHub API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueDetail {
    pub key: IssueKey,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub comments: Vec<IssueComment>,
    pub trusted_comments: Vec<IssueComment>,
}

/// One GitHub comment attached to an issue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IssueComment {
    pub author: String,
    pub body: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Convenience: where the worker should drop its `worker-result.json` for
/// a given run ID on the daemon side. The actual implementation lands in
/// Task 4.x; the stub keeps the symbol reachable.
pub fn worker_result_path(_state_dir: &PathBuf, _run_id: &str) -> PathBuf {
    PathBuf::new()
}

pub(crate) fn validate_owner(owner: &str) -> CaduceusResult<()> {
    if owner.is_empty() || owner.len() > 39 {
        return Err(CaduceusError::Other(format!(
            "owner must be 1..=39 chars; got {}",
            owner.len()
        )));
    }
    if owner.starts_with('-') || owner.ends_with('-') {
        return Err(CaduceusError::Other(
            "owner cannot begin or end with '-'".to_string(),
        ));
    }
    if !owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(CaduceusError::Other(format!(
            "owner contains invalid character: {owner}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_repo(repo: &str) -> CaduceusResult<()> {
    if repo.is_empty() || repo.len() > 100 {
        return Err(CaduceusError::Other(format!(
            "repo must be 1..=100 chars; got {}",
            repo.len()
        )));
    }
    if repo == "." || repo == ".." {
        return Err(CaduceusError::Other(format!("repo cannot be {repo}")));
    }
    if !repo
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(CaduceusError::Other(format!(
            "repo contains invalid character: {repo}"
        )));
    }
    Ok(())
}
