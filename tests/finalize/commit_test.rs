//! Tests for the configurable git author identity cascade.

use std::fs;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use caduceus::config::Config;
use caduceus::finalize::commit::{
    resolve_git_author, DEFAULT_GIT_USER_EMAIL, DEFAULT_GIT_USER_NAME,
};

static TEST_HOST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

fn with_host_config<T>(gitconfig: Option<&str>, test: impl FnOnce() -> T) -> T {
    let _guard = TEST_HOST_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("host config test mutex");
    let home = tempfile::tempdir().expect("temporary HOME");
    if let Some(contents) = gitconfig {
        fs::write(home.path().join(".gitconfig"), contents).expect("write gitconfig");
    }

    let original_home = std::env::var_os("HOME");
    let original_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("HOME", home.path());
    std::env::set_var("XDG_CONFIG_HOME", home.path());

    let result = catch_unwind(AssertUnwindSafe(test));

    match original_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match original_xdg {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    match result {
        Ok(value) => value,
        Err(payload) => resume_unwind(payload),
    }
}

fn test_config() -> Config {
    Config::test_defaults(Path::new("/tmp"))
}

#[test]
fn explicit_identity_wins() {
    with_host_config(None, || {
        let mut cfg = test_config();
        cfg.git_author_name = Some("Ops Bot".to_string());
        cfg.git_author_email = Some("ops@example.com".to_string());

        assert_eq!(
            resolve_git_author(&cfg),
            ("Ops Bot".to_string(), "ops@example.com".to_string())
        );
    });
}

#[test]
fn host_identity_wins_when_config_is_absent() {
    with_host_config(
        Some("[user]\n\tname = Host Bot\n\temail = host@example.com\n"),
        || {
            let cfg = test_config();
            assert_eq!(
                resolve_git_author(&cfg),
                ("Host Bot".to_string(), "host@example.com".to_string())
            );
        },
    );
}

#[test]
fn last_resort_identity_is_used_without_config() {
    with_host_config(None, || {
        let cfg = test_config();
        assert_eq!(
            resolve_git_author(&cfg),
            (
                DEFAULT_GIT_USER_NAME.to_string(),
                DEFAULT_GIT_USER_EMAIL.to_string()
            )
        );
    });
}

#[test]
fn config_name_and_host_email_merge() {
    with_host_config(Some("[user]\n\temail = host@example.com\n"), || {
        let mut cfg = test_config();
        cfg.git_author_name = Some("Ops Bot".to_string());

        assert_eq!(
            resolve_git_author(&cfg),
            ("Ops Bot".to_string(), "host@example.com".to_string())
        );
    });
}

#[test]
fn empty_host_identity_falls_back_without_error() {
    with_host_config(Some("[user]\n\tname =\n\temail =\n"), || {
        let cfg = test_config();
        assert_eq!(
            resolve_git_author(&cfg),
            (
                DEFAULT_GIT_USER_NAME.to_string(),
                DEFAULT_GIT_USER_EMAIL.to_string()
            )
        );
    });
}

#[test]
fn config_name_merges_with_last_resort_email() {
    with_host_config(None, || {
        let mut cfg = test_config();
        cfg.git_author_name = Some("Ops Bot".to_string());

        assert_eq!(
            resolve_git_author(&cfg),
            ("Ops Bot".to_string(), DEFAULT_GIT_USER_EMAIL.to_string())
        );
    });
}

#[test]
fn default_identity_constants_are_documented_values() {
    assert_eq!(DEFAULT_GIT_USER_NAME, "Caduceus Daemon");
    assert_eq!(DEFAULT_GIT_USER_EMAIL, "caduceus@daemon.local");
}
