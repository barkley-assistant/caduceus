//! These tests exercise the documented hierarchy:
//!
//! 1. Explicit `Config::github_token`
//! 2. `$CADUCEUS_GITHUB_TOKEN`
//! 3. `$GITHUB_TOKEN`
//! 4. `gh auth token` subprocess output
//!
//! All env-mutating tests are wrapped in [`serial_test::serial`] so
//! parallel test threads never observe each other's env vars. The
//! tests use a stub [`TokenEnv`] and a stub [`GhRunner`] so the real
//! process environment is never touched except by the env-scoping
//! helper (which restores every variable it touched).

use std::collections::HashMap;
use std::sync::Mutex;

use caduceus::config::{
    Config, GhRunner, GhRunnerOutput, OsEnv, ResolvedToken, TokenEnv, TokenSource,
};
use caduceus::error::CaduceusError;
use serial_test::serial;

// Global env-scoping guard. Only one scoped-env test runs at a time,
// and each scoped block records every variable it touched so they can
// be restored exactly.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// Test fixtures

#[derive(Default)]
struct MapEnv {
    map: HashMap<String, String>,
}

impl MapEnv {
    fn with(pairs: &[(&str, &str)]) -> Self {
        let mut env = Self::default();
        for (k, v) in pairs {
            env.map.insert((*k).to_string(), (*v).to_string());
        }
        env
    }
}

impl TokenEnv for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.map
            .get(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

struct StubGh {
    output: Result<GhRunnerOutput, ()>,
}

impl GhRunner for StubGh {
    fn run(&self) -> Result<GhRunnerOutput, CaduceusError> {
        match &self.output {
            Ok(out) => Ok(out.clone()),
            Err(()) => Err(CaduceusError::TokenResolution(
                "`gh` executable not found in PATH".to_string(),
            )),
        }
    }
}

/// Scope an environment-mutating test. Records every variable it
/// touches so they can be restored to their previous values (or
/// removed if they were absent before).
fn scoped_env<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let _guard = ENV_LOCK.lock().expect("env lock poisoned");
    let previous: Vec<(String, Option<std::ffi::OsString>)> = [
        "CADUCEUS_GITHUB_TOKEN",
        "GITHUB_TOKEN",
        "PATH",
        "HOME",
        "USER",
        "XDG_CONFIG_HOME",
    ]
    .into_iter()
    .map(|name| {
        let prior = std::env::var_os(name);
        std::env::remove_var(name);
        (name.to_string(), prior)
    })
    .collect();
    let result = f();
    for (name, prior) in previous.into_iter().rev() {
        match prior {
            Some(value) => std::env::set_var(&name, value),
            None => std::env::remove_var(&name),
        }
    }
    result
}

// Hierarchy precedence

#[test]
#[serial]
fn explicit_config_field_wins_over_env_vars_and_gh() {
    let cfg = Config {
        github_token: Some("explicit-token".to_string()),
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::with(&[
        ("CADUCEUS_GITHUB_TOKEN", "env-cad"),
        ("GITHUB_TOKEN", "env-gh"),
    ]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let resolved = resolve(cfg, &env, &runner);
    assert_eq!(resolved.token, "explicit-token");
    assert_eq!(resolved.source, TokenSource::ExplicitConfig);
}

#[test]
#[serial]
fn caduceus_env_wins_over_github_env_and_gh() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::with(&[
        ("CADUCEUS_GITHUB_TOKEN", "env-cad"),
        ("GITHUB_TOKEN", "env-gh"),
    ]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let resolved = resolve(cfg, &env, &runner);
    assert_eq!(resolved.token, "env-cad");
    assert_eq!(resolved.source, TokenSource::CaduceusEnv);
}

#[test]
#[serial]
fn github_env_wins_over_gh_when_caduceus_env_unset() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::with(&[("GITHUB_TOKEN", "env-gh")]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let resolved = resolve(cfg, &env, &runner);
    assert_eq!(resolved.token, "env-gh");
    assert_eq!(resolved.source, TokenSource::GithubEnv);
}

#[test]
#[serial]
fn gh_cli_used_when_no_config_or_env_set() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::default();
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let resolved = resolve(cfg, &env, &runner);
    assert_eq!(resolved.token, "gh-token");
    assert_eq!(resolved.source, TokenSource::GhCli);
}

#[test]
#[serial]
fn fallback_chain_skips_blank_levels() {
    // Empty config field, blank env vars, then a working gh. We
    // confirm that empty env vars do not block the chain.
    let cfg = Config {
        github_token: Some(String::new()),
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::with(&[("CADUCEUS_GITHUB_TOKEN", "   "), ("GITHUB_TOKEN", "")]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "real-token".to_string(),
            stderr: String::new(),
        }),
    };
    let resolved = resolve(cfg, &env, &runner);
    assert_eq!(resolved.token, "real-token");
    assert_eq!(resolved.source, TokenSource::GhCli);
}

#[test]
#[serial]
fn whitespace_only_explicit_field_is_treated_as_unset() {
    let cfg = Config {
        github_token: Some("   ".to_string()),
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::default();
    let runner = StubGh { output: Err(()) };
    let err = resolve_or_err(cfg, &env, &runner);
    let msg = format!("{err:?}");
    assert!(
        msg.contains("`gh` executable not found") || msg.contains("`gh auth token`"),
        "got: {msg}"
    );
}

// gh CLI failure modes

#[test]
#[serial]
fn missing_gh_is_a_token_resolution_error() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::default();
    let runner = StubGh { output: Err(()) };
    let err = resolve_or_err(cfg, &env, &runner);
    let msg = format!("{err:?}");
    assert!(msg.contains("`gh` executable not found"), "got: {msg}");
}

#[test]
#[serial]
fn gh_exit_nonzero_surfaces_exit_code_but_not_stderr() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::default();
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 42,
            stdout: String::new(),
            stderr: "secret-leak-ghp_xyz".to_string(),
        }),
    };
    let err = resolve_or_err(cfg, &env, &runner);
    let msg = format!("{err:?}");
    assert!(msg.contains("exited 42"), "got: {msg}");
    // stderr is intentionally NOT in the error message.
    assert!(!msg.contains("secret-leak"), "stderr leaked: {msg}");
    assert!(!msg.contains("ghp_xyz"), "token leaked: {msg}");
}

#[test]
#[serial]
fn gh_empty_output_is_failure() {
    let cfg = Config {
        github_token: None,
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::default();
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "   \n  ".to_string(),
            stderr: String::new(),
        }),
    };
    let err = resolve_or_err(cfg, &env, &runner);
    let msg = format!("{err:?}");
    assert!(msg.contains("returned no usable token"), "got: {msg}");
}

#[test]
#[serial]
fn error_text_never_includes_token_value() {
    // Force every level that the resolver inspects to be blank so
    // the chain falls through to a failing ``gh`` invocation. The
    // runner's stdout and stderr carry token-shaped strings; the
    // error message must surface only the exit code, not the value.
    let cfg = Config {
        github_token: Some(String::new()),
        ..Config::test_defaults(&tempdir())
    };
    let env = MapEnv::with(&[("CADUCEUS_GITHUB_TOKEN", ""), ("GITHUB_TOKEN", "")]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 1,
            stdout: "ghp_SHOULDNOTLEAK_STDOUT".to_string(),
            stderr: "ghp_SHOULDNOTLEAK_STDERR".to_string(),
        }),
    };
    let err = resolve_or_err(cfg, &env, &runner);
    let msg = format!("{err:?}");
    for needle in ["SHOULDNOTLEAK_STDOUT", "SHOULDNOTLEAK_STDERR"] {
        assert!(
            !msg.contains(needle),
            "token leaked into error ({needle}): {msg}"
        );
    }
}

// OsEnv reads from the process environment

#[test]
#[serial]
fn os_env_reads_caduceus_env_var_when_present() {
    scoped_env(|| {
        std::env::set_var("CADUCEUS_GITHUB_TOKEN", "from-process-env");
        let env = OsEnv;
        let value = env.get("CADUCEUS_GITHUB_TOKEN");
        assert_eq!(value.as_deref(), Some("from-process-env"));
    });
}

#[test]
#[serial]
fn os_env_returns_none_for_unset_var() {
    scoped_env(|| {
        let env = OsEnv;
        assert!(env.get("CADUCEUS_GITHUB_TOKEN").is_none());
        assert!(env.get("GITHUB_TOKEN").is_none());
    });
}

#[test]
#[serial]
fn os_env_treats_whitespace_only_as_unset() {
    scoped_env(|| {
        std::env::set_var("CADUCEUS_GITHUB_TOKEN", "   ");
        let env = OsEnv;
        assert!(env.get("CADUCEUS_GITHUB_TOKEN").is_none());
    });
}

#[test]
#[serial]
fn os_env_treats_empty_string_as_unset() {
    scoped_env(|| {
        std::env::set_var("GITHUB_TOKEN", "");
        let env = OsEnv;
        assert!(env.get("GITHUB_TOKEN").is_none());
    });
}

#[test]
#[serial]
fn os_env_trims_surrounding_whitespace() {
    scoped_env(|| {
        std::env::set_var("CADUCEUS_GITHUB_TOKEN", "  ghp_abc  ");
        let env = OsEnv;
        assert_eq!(env.get("CADUCEUS_GITHUB_TOKEN").as_deref(), Some("ghp_abc"));
    });
}

// Real runner shell-out (smoke test)

#[test]
#[serial]
fn real_runner_surfaces_spawn_errors_without_leaking_command_args() {
    // Point PATH at an empty directory so ``which::which("gh")`` fails.
    scoped_env(|| {
        let empty = tempdir().join("empty-bin");
        std::fs::create_dir_all(&empty).unwrap();
        std::env::set_var("PATH", &empty);
        let runner = caduceus::config::RealGhRunner;
        let err = runner.run().expect_err("gh should not be on PATH");
        let msg = format!("{err:?}");
        assert!(msg.contains("`gh` executable not found"), "got: {msg}");
    });
}

// Helpers

fn resolve(cfg: Config, env: &dyn TokenEnv, runner: &dyn GhRunner) -> ResolvedToken {
    caduceus::config::resolve_token_chain(&cfg, env, runner).expect("token resolves")
}

fn resolve_or_err(cfg: Config, env: &dyn TokenEnv, runner: &dyn GhRunner) -> CaduceusError {
    match caduceus::config::resolve_token_chain(&cfg, env, runner) {
        Ok(token) => panic!("expected error, got token {token:?}"),
        Err(err) => err,
    }
}

fn tempdir() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-token-test-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}
