//! Configuration resolution tests for daemon-level knobs.

use std::path::PathBuf;

use caduceus::config::{Config, LoadContext, RawConfig};

#[test]
fn worktree_gc_defaults_are_one_day_and_enabled() {
    let cfg = Config::test_defaults(PathBuf::from("/tmp/caduceus-test").as_path());
    assert_eq!(cfg.worktree_gc_older_than_days, 1);
    assert!(!cfg.worktree_gc_disabled);
}

#[test]
fn worktree_gc_explicit_values_resolve() {
    let raw = RawConfig {
        worktree_gc_older_than_days: Some(7),
        worktree_gc_disabled: Some(true),
        worker_command: Some(vec!["/bin/true".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &LoadContext::default()).expect("config");
    assert_eq!(cfg.worktree_gc_older_than_days, 7);
    assert!(cfg.worktree_gc_disabled);
}

#[test]
fn worktree_gc_zero_threshold_is_rejected() {
    let raw = RawConfig {
        worktree_gc_older_than_days: Some(0),
        worker_command: Some(vec!["/bin/true".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let err = Config::from_raw(raw, &LoadContext::default()).expect_err("zero must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("worktree_gc_older_than_days must be > 0"),
        "unexpected error: {msg}"
    );
}

#[test]
fn worktree_gc_explicit_defaults_match_test_defaults() {
    let raw = RawConfig {
        worktree_gc_older_than_days: Some(1),
        worktree_gc_disabled: Some(false),
        worker_command: Some(vec!["/bin/true".to_string()]),
        reduced_containment_acknowledged: Some(true),
        ..Default::default()
    };
    let from_raw = Config::from_raw(raw, &LoadContext::default()).expect("config");
    let defaults = Config::test_defaults(PathBuf::from("/tmp/caduceus-test").as_path());
    assert_eq!(
        from_raw.worktree_gc_older_than_days,
        defaults.worktree_gc_older_than_days
    );
    assert_eq!(from_raw.worktree_gc_disabled, defaults.worktree_gc_disabled);
}
