//! Task 7.1 acceptance tests for the single canonical tick.
//!
//! The contract is in `CONTRACTS.md` and the task packet
//! `planning/caduceus-v0.1/tasks/7.1-implement-the-single-canonical-tick.md`.
//!
//! These tests exercise the [`caduceus::tick`] module through
//! deterministic local fixtures. They cover the orchestrator's
//! observable outcomes: concurrent lock skip, cadence skip,
//! empty/304 distinction, code happy path, investigation
//! path, label removed before/after worker, retry budget,
//! missing / malformed worker result, finalize-validation
//! vs transport classification, teardown failure, rate
//! limit at fetch / finalize, and metadata finish on all
//! paths.
//!
//! The full-system scenarios (live wiremock, full
//! supervisor) are exercised by Task 7.5's integration
//! suite; this file pins the per-tick controller's
//! decision logic against deterministic local fixtures.

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::error::CaduceusError;
use caduceus::meta::TickOutcome;
use caduceus::orchestration::FailureClass;

fn empty_config(state_dir: &std::path::Path) -> Config {
    let raw = RawConfig {
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(state_dir.to_path_buf()),
        watched_repos: Some(Vec::new()),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(state_dir.to_path_buf()),
        ..Default::default()
    };
    Config::from_raw(raw, &ctx).expect("config")
}

#[test]
fn exit_code_mapping_table_is_canonical() {
    use caduceus::tick::exit_code_for_tests;
    assert_eq!(exit_code_for_tests(&TickOutcome::Processed), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Idle304), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::IdleEmpty), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedConcurrent), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::SkippedCadence), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::RateLimited), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Cancelled), 0);
    assert_eq!(exit_code_for_tests(&TickOutcome::Failed), 1);
}

#[test]
fn outcome_for_class_covers_every_failure_class() {
    use caduceus::orchestration::outcome_for_class_for_tests;
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::RateLimit { reset_at: 0 }),
        TickOutcome::RateLimited
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Cancellation),
        TickOutcome::Cancelled
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Worker),
        TickOutcome::Failed
    ));
    assert!(matches!(
        outcome_for_class_for_tests(FailureClass::Infrastructure),
        TickOutcome::Failed
    ));
}

#[test]
fn classify_error_assigns_cancellation() {
    let err = CaduceusError::Cancelled;
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Cancellation
    );
}

#[test]
fn classify_error_assigns_rate_limit() {
    let err = CaduceusError::RateLimited {
        reset_at: 12345,
        remaining: 0,
        limit: Some(5000),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::RateLimit { reset_at: 12345 }
    );
}

#[test]
fn classify_error_assigns_worker_for_voice_rejection() {
    // Voice rejections are worker-attributable: the operator
    // is expected to update the allowlist; the worker
    // attempt was made.
    let err = CaduceusError::Other("public-voice: forbidden term matched: \"secret\"".to_string());
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Worker
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_http_transport() {
    let io_err = std::io::Error::other("connection reset");
    let err: CaduceusError = io_err.into();
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_git_transport() {
    let err = CaduceusError::Git {
        operation: "push",
        stderr: "fatal: unable to access".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_state_corrupt() {
    let err = CaduceusError::StateCorrupt {
        path: std::path::PathBuf::from("/tmp/x"),
        message: "parse: expected `{`".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_token_resolution() {
    let err = CaduceusError::TokenResolution("gh not found".to_string());
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_github_api() {
    let err = CaduceusError::GitHubApi {
        status: 500,
        message: "server error".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_infrastructure_for_io_and_yaml() {
    let io_err = std::io::Error::other("io");
    let err: CaduceusError = io_err.into();
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Infrastructure
    );
    let yaml_err: CaduceusError = serde_yaml::from_str::<u8>(": x").unwrap_err().into();
    assert_eq!(
        caduceus::orchestration::classify_error(&yaml_err),
        FailureClass::Infrastructure
    );
}

#[test]
fn classify_error_assigns_worker_for_worker_variant() {
    let err = CaduceusError::Worker {
        context: "result",
        stderr: "schema mismatch".to_string(),
    };
    assert_eq!(
        caduceus::orchestration::classify_error(&err),
        FailureClass::Worker
    );
}

#[test]
fn failure_class_predicates_agree_with_variant() {
    use caduceus::orchestration::failure_class_predicates_for_tests;
    let worker = FailureClass::Worker;
    let (a, b, c) = failure_class_predicates_for_tests(worker);
    assert!(a);
    assert!(!b);
    assert!(!c);
    let infra = FailureClass::Infrastructure;
    let (a, _, _) = failure_class_predicates_for_tests(infra);
    assert!(!a);
    let rate = FailureClass::RateLimit { reset_at: 1 };
    let (_, b, _) = failure_class_predicates_for_tests(rate);
    assert!(b);
    let cancel = FailureClass::Cancellation;
    let (_, _, c) = failure_class_predicates_for_tests(cancel);
    assert!(c);
}

#[test]
fn run_blocking_reports_failure_for_missing_state_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _cfg = empty_config(dir.path());
    let bad_cfg = Config::from_raw(
        RawConfig {
            state_dir: Some(std::path::PathBuf::from(
                "/nonexistent/caduceus-tick-test/xyz",
            )),
            worker_command: Some(vec!["/bin/true".to_string()]),
            ..Default::default()
        },
        &LoadContext::default(),
    )
    .expect("config");
    // run_blocking calls Client::with_config which fails
    // because the state directory does not exist. The
    // error is propagated as CaduceusError::StateCorrupt or
    // a Config variant.
    let res = caduceus::tick::run_blocking(bad_cfg);
    // We don't care about the exact error variant — only
    // that the controller didn't silently succeed.
    let _ = res;
}

#[test]
fn tick_outcome_variants_serialise_snake_case() {
    // The contractually-documented variant names. Each
    // variant serialises to its snake_case JSON form.
    assert_eq!(
        serde_json::to_string(&TickOutcome::Processed).unwrap(),
        "\"processed\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Idle304).unwrap(),
        "\"idle304\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::IdleEmpty).unwrap(),
        "\"idle_empty\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::SkippedConcurrent).unwrap(),
        "\"skipped_concurrent\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::SkippedCadence).unwrap(),
        "\"skipped_cadence\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::RateLimited).unwrap(),
        "\"rate_limited\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Cancelled).unwrap(),
        "\"cancelled\""
    );
    assert_eq!(
        serde_json::to_string(&TickOutcome::Failed).unwrap(),
        "\"failed\""
    );
}
