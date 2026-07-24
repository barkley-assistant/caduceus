//! GitHub API surface — HTTP client, issue model, polling, and
//! second-pass label verification.
//!
//! Module shape after the v0.1 → v1.0 restructuring (issue #13):
//!
//! - [`client`] — the HTTP client, ETag cache, and rate-limit parsing.
//! - [`issue`] — `IssueKey`, `IssueDetail`, and the fetch helper.
//! - [`poll`] — repository discovery, label polling, merge outcomes.
//! - [`verify`] — second-pass label verification before claiming.

pub mod client;
pub mod issue;
pub mod link_header;
pub mod merge_detect;
pub mod poll;
pub mod verify;

// Explicit re-exports of every public symbol from each leaf module.
// Glob re-exports were rejected because two leaf modules can share a
// public symbol name (e.g. `IssueSummary` lives in both `client` and
// `poll`); the per-leaf module re-exports below are the table of
// contents for the directory.
pub use crate::github::client::{
    cache_key, check_voice_or_error, is_valid_etag, poll_interval_from_headers,
    rate_limit_from_headers, super_err_from_voice, CacheEntry, Client, HttpCache, HttpClient,
    IssueSummary, PullRequest, RateLimitInfo, Response, VoiceChannel, ACCEPT_VALUE,
    GITHUB_API_VERSION_HEADER, GITHUB_API_VERSION_VALUE, MAX_BODY_BYTES, MAX_REDIRECTS,
    USER_AGENT_PREFIX,
};
pub use crate::github::issue::{IssueComment, IssueDetail, IssueEvent, IssueKey};
pub use crate::github::merge_detect::{poll_pr_merge_status, MergeStatus};
pub use crate::github::poll::{
    discover_watched_repos, poll_code, IssuePollDiagnostic, IssuePollOutcome,
    IssueSummary as PollIssueSummary,
};
pub use crate::github::verify::{verify_trigger, SkipReason, VerifyOutcome};
