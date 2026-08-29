//! Tests for refuse-to-operate error classification.
//!
//! The three reserved `context` tags on `CaduceusError::Worktree` and
//! `CaduceusError::Queue` must classify as `FailureClass::Terminal`;
//! everything else stays `Infrastructure` (or its existing class).

use caduceus::orchestration::{classify_error, failure_class_predicates_for_tests, FailureClass};
use caduceus::CaduceusError;

#[test]
fn terminal_for_dirty_main() {
    let err = CaduceusError::Worktree {
        context: "discover-dirty-main",
        stderr: "main checkout is dirty".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);
    assert!(class.is_terminal());
    assert!(!class.counts_against_retry_budget());
}

#[test]
fn terminal_for_path_collision() {
    let err = CaduceusError::Worktree {
        context: "create-path-collision",
        stderr: "path collision".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);
    assert!(class.is_terminal());
    assert!(!class.counts_against_retry_budget());
}

#[test]
fn terminal_for_claim_mismatch() {
    let err = CaduceusError::Queue {
        context: "claim-terminal-mismatch",
        stderr: "claim mismatch".to_string(),
    };
    let class = classify_error(&err);
    assert_eq!(class, FailureClass::Terminal);
    assert!(class.is_terminal());
    assert!(!class.counts_against_retry_budget());
}

#[test]
fn infrastructure_still_infrastructure() {
    // Unrelated Worktree/Queue contexts remain Infrastructure.
    let worktree_other = CaduceusError::Worktree {
        context: "create",
        stderr: "generic worktree failure".to_string(),
    };
    assert_eq!(
        classify_error(&worktree_other),
        FailureClass::Infrastructure
    );

    let git_error = CaduceusError::Git {
        operation: "fetch",
        stderr: "network unreachable".to_string(),
    };
    assert_eq!(classify_error(&git_error), FailureClass::Infrastructure);
}

#[test]
fn oci_image_failures_use_their_distinct_failure_classes() {
    let digest_mismatch = CaduceusError::OciImageDigestMismatch {
        reference: "worker@sha256:expected".to_string(),
        expected: "sha256:expected".to_string(),
        found: vec![],
    };
    assert_eq!(
        classify_error(&digest_mismatch),
        FailureClass::ImageVerification
    );

    let architecture_mismatch = CaduceusError::OciImageArchitectureMismatch {
        reference: "worker@sha256:expected".to_string(),
        expected: "amd64".to_string(),
        found: "arm64".to_string(),
    };
    assert_eq!(
        classify_error(&architecture_mismatch),
        FailureClass::ImageVerification
    );

    for error in [
        CaduceusError::OciPullFailed {
            image: "worker@sha256:expected".to_string(),
            stderr: "registry unavailable".to_string(),
        },
        CaduceusError::OciImageInspectFailed {
            reference: "worker@sha256:expected".to_string(),
            detail: "inspect failed".to_string(),
        },
        CaduceusError::OciImageMissing {
            reference: "worker@sha256:expected".to_string(),
        },
    ] {
        assert_eq!(classify_error(&error), FailureClass::Worker);
    }
}

#[test]
fn terminal_predicates_match_existing_classes() {
    // Worker still counts against budget; RateLimit/Cancellation still have
    // their dedicated predicates; Infrastructure and Terminal do not.
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::Terminal),
        (false, false, false)
    );
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::Infrastructure),
        (false, false, false)
    );
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::ImageVerification),
        (false, false, false)
    );
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::Worker),
        (true, false, false)
    );
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::RateLimit { reset_at: 0 }),
        (false, true, false)
    );
    assert_eq!(
        failure_class_predicates_for_tests(FailureClass::Cancellation),
        (false, false, true)
    );
}
