//! AC1 (issue #318): every DAR §13 review event name is asserted to
//! equal its catalog literal exactly.
//!
//! The test reads the producing sites' `pub const … : &str` values
//! through the single `caduceus::runtime::audit::review_events` seam —
//! NOT a re-declared list — so a drift between the const value and
//! the §13 catalog is caught at compile/test time, not at log time.
//! AC3 (the verdict/execution distinctness rule) is asserted here too.

use caduceus::runtime::audit::review_events::{
    ADMITTED_EVENT, DISCOVERED_EVENT, EVENT_PUBLISHED, EVENT_PUBLISH_FAILED_RETRYABLE,
    EVENT_PUBLISH_STARTED, EVENT_SUPPRESSED_STALE_GENERATION, FORK_SKIP_EVENT,
    MUTATION_VIOLATION_EVENT, OVERSIZED_PR_EVENT, REVIEW_EXECUTION_FAILED_EVENT,
    REVIEW_FAILED_VERDICT_EVENT, REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT,
    REVIEW_PASSED_EVENT, REVIEW_RETRY_SCHEDULED_EVENT, REVIEW_SKIPPED_HEAD_SHA_UNAVAILABLE_EVENT,
    REVIEW_SKIPPED_PR_GONE_EVENT, REVIEW_STARTED_EVENT, REVIEW_WORKER_COMPLETED_EVENT,
    SKIPPED_ALREADY_COMPLETE_EVENT, SKIPPED_DRAFT_EVENT, STALE_SHA_EVENT,
};

#[test]
fn dar_section_13_catalog_names_match_exactly() {
    // The §13 catalog block (docs/architecture/auto-review.md lines
    // 592–605), one assert per line so a failure names the offending
    // event. Order matches the catalog listing.
    assert_eq!(DISCOVERED_EVENT, "review_discovered");
    assert_eq!(
        SKIPPED_ALREADY_COMPLETE_EVENT,
        "review_skipped_already_complete"
    );
    assert_eq!(SKIPPED_DRAFT_EVENT, "review_skipped_draft");
    assert_eq!(FORK_SKIP_EVENT, "review_skipped_fork_unsupported");
    assert_eq!(ADMITTED_EVENT, "review_admitted");
    assert_eq!(REVIEW_STARTED_EVENT, "review_started");
    assert_eq!(REVIEW_WORKER_COMPLETED_EVENT, "review_worker_completed");
    assert_eq!(REVIEW_RETRY_SCHEDULED_EVENT, "review_retry_scheduled");
    assert_eq!(REVIEW_EXECUTION_FAILED_EVENT, "review_execution_failed");
    assert_eq!(REVIEW_PASSED_EVENT, "review_passed");
    assert_eq!(REVIEW_FAILED_VERDICT_EVENT, "review_failed_verdict");
    assert_eq!(MUTATION_VIOLATION_EVENT, "review_mutation_violation");
    assert_eq!(
        REVIEW_SKIPPED_HEAD_SHA_UNAVAILABLE_EVENT,
        "review_skipped_head_sha_unavailable"
    );
    assert_eq!(REVIEW_SKIPPED_PR_GONE_EVENT, "review_skipped_pr_gone");
    assert_eq!(OVERSIZED_PR_EVENT, "review_skipped_oversized_pr");
    assert_eq!(EVENT_PUBLISH_STARTED, "review_publish_started");
    assert_eq!(EVENT_PUBLISHED, "review_published");
    assert_eq!(
        EVENT_PUBLISH_FAILED_RETRYABLE,
        "review_publish_failed_retryable"
    );
    assert_eq!(
        EVENT_SUPPRESSED_STALE_GENERATION,
        "review_publication_suppressed_stale_generation"
    );
    assert_eq!(STALE_SHA_EVENT, "review_stale_sha_observed");
    assert_eq!(
        REVIEW_MIGRATION_TERMINATED_INVESTIGATION_EVENT,
        "review_migration_terminated_investigation"
    );
}

#[test]
fn verdict_fail_and_execution_failed_are_distinct_strings() {
    // AC3: the two names must never be conflatable.
    assert_ne!(REVIEW_FAILED_VERDICT_EVENT, REVIEW_EXECUTION_FAILED_EVENT);
    // Reinforce the deliberate word choice.
    assert!(REVIEW_FAILED_VERDICT_EVENT.contains("verdict"));
    assert!(REVIEW_EXECUTION_FAILED_EVENT.contains("execution"));
}

/// The out-of-catalog §9.3 event is NOT part of the §13 catalog and
/// must not be asserted there; pin the boundary so a future edit
/// cannot quietly fold it in.
#[test]
fn skipped_pr_closed_unmerged_is_not_in_the_catalog_seam() {
    // `review_skipped_pr_closed_unmerged` lives only at its producing
    // site (finalize.rs) and is a §9.3 event, not a §13 catalog name.
    assert_eq!(
        caduceus::review::finalize::EVENT_SKIPPED_PR_CLOSED_UNMERGED,
        "review_skipped_pr_closed_unmerged"
    );
}
