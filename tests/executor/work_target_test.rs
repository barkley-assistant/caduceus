//! Unit tests for the target-neutral `WorkTarget` boundary type
//! (DAR §6.1, issue #346).
//!
//! The issue path is a lossless rename of the historical flat
//! `ExecutorSpec` fields; the PR path carries a `ReviewTarget` and
//! never requires an `IssueKey` or a branch name. These tests pin the
//! display identity and the per-variant payload shapes.

use caduceus::executor::{IssueWorkTarget, WorkTarget};
use caduceus::github::issue::IssueKey;
use caduceus::review::{RepositoryId, ReviewTarget};

/// The frozen PR identity used across the boundary tests — the same
/// shape the supervisor round-trips through `--pr-target-json`.
fn sample_review_target() -> ReviewTarget {
    ReviewTarget {
        repository: RepositoryId {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        },
        pull_request: 9,
        head_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        base_sha: "cafebabecafebabecafebabecafebabecafebabe".to_string(),
        base_ref: "main".to_string(),
        merge_base: "abcdef01abcdef01abcdef01abcdef01abcdef01".to_string(),
    }
}

/// The historical issue payload — byte-for-byte the former flat
/// `ExecutorSpec` fields.
fn sample_issue_target() -> IssueWorkTarget {
    IssueWorkTarget {
        key: IssueKey::parse("owner/repo#7").expect("valid issue key"),
        title: "Fix the flaky test".to_string(),
        body: "The integration suite flakes under load.".to_string(),
        labels: vec!["autofix".to_string()],
        branch_name: "owner/repo-7".to_string(),
    }
}

#[test]
fn issue_target_display_is_issue_key_display() {
    let target = WorkTarget::Issue(sample_issue_target());
    // Unchanged from the pre-#346 display (`IssueKey::display_key`).
    assert_eq!(target.display(), "owner/repo#7");
}

#[test]
fn pr_target_display_is_owner_repo_pr_number() {
    let target = WorkTarget::PullRequest(sample_review_target());
    assert_eq!(target.display(), "owner/repo#pr/9");
}

#[test]
fn issue_work_target_round_trips_historical_fields() {
    let payload = sample_issue_target();
    let WorkTarget::Issue(round_tripped) = WorkTarget::Issue(payload.clone()) else {
        panic!("WorkTarget::Issue must carry an IssueWorkTarget");
    };
    assert_eq!(round_tripped, payload);
    assert_eq!(round_tripped.key, IssueKey::parse("owner/repo#7").unwrap());
    assert_eq!(round_tripped.title, "Fix the flaky test");
    assert_eq!(
        round_tripped.body,
        "The integration suite flakes under load."
    );
    assert_eq!(round_tripped.labels, vec!["autofix"]);
    assert_eq!(round_tripped.branch_name, "owner/repo-7");
}

#[test]
fn pr_target_requires_no_issue_key_or_branch() {
    // Constructing a PR run needs only the frozen review identity — no
    // synthetic IssueKey and no branch name (the type-level contract).
    let target = WorkTarget::PullRequest(sample_review_target());
    let WorkTarget::PullRequest(pr) = target else {
        panic!("WorkTarget::PullRequest must carry a ReviewTarget");
    };
    assert_eq!(pr.repository.full_name(), "owner/repo");
    assert_eq!(pr.pull_request, 9);
    assert_eq!(pr.head_sha, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    assert_eq!(pr.base_sha, "cafebabecafebabecafebabecafebabecafebabe");
    assert_eq!(pr.base_ref, "main");
    assert_eq!(pr.merge_base, "abcdef01abcdef01abcdef01abcdef01abcdef01");
}

#[test]
fn work_target_is_eq_and_cloneable() {
    let a = WorkTarget::Issue(sample_issue_target());
    let b = a.clone();
    assert_eq!(a, b);

    let pr_a = WorkTarget::PullRequest(sample_review_target());
    let pr_b = pr_a.clone();
    assert_eq!(pr_a, pr_b);
}
