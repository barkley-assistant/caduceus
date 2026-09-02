//! Regression tests for the token-resolution chain wired into
//! Config::load. All four cases call `Config::load_from_with_env` with
//! a stub `TokenEnv` (so tests do not mutate the process environment)
//! and a stub `GhRunner` where needed.

use caduceus::config::{Config, GhRunner, GhRunnerOutput, TokenEnv};
use serial_test::serial;
#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;
use std::collections::HashMap;

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
    fn run(&self) -> Result<GhRunnerOutput, caduceus::error::CaduceusError> {
        match &self.output {
            Ok(out) => Ok(out.clone()),
            Err(()) => Err(caduceus::error::CaduceusError::TokenResolution(
                "`gh` executable not found in PATH".to_string(),
            )),
        }
    }
}

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    std::fs::write(path, body).expect("write config");
}

fn minimal_null_yaml() -> String {
    String::from(
        r#"
worker_command: ["python3", "/test/worker.py"]
reduced_containment_acknowledged: true
github_token: null
"#,
    )
}

#[test]
#[serial]
fn config_load_resolves_env_token() {
    let root = tempdir("env-token");
    let path = root.join("config.yaml");
    write(&path, &minimal_null_yaml());

    let env = MapEnv::with(&[("CADUCEUS_GITHUB_TOKEN", "env-token-value")]);
    let runner = StubGh { output: Err(()) };
    let cfg = Config::load_from_with_env(&path, &env, &runner).expect("config loads from env");
    assert_eq!(cfg.github_token.as_deref(), Some("env-token-value"));
}

#[test]
#[serial]
fn config_load_yaml_wins_over_env() {
    let root = tempdir("yaml-wins");
    let path = root.join("config.yaml");
    write(
        &path,
        r#"
worker_command: ["python3", "/test/worker.py"]
reduced_containment_acknowledged: true
github_token: yaml-token-value
"#,
    );

    let env = MapEnv::with(&[("CADUCEUS_GITHUB_TOKEN", "env-token-value")]);
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let cfg = Config::load_from_with_env(&path, &env, &runner).expect("config loads from yaml");
    assert_eq!(cfg.github_token.as_deref(), Some("yaml-token-value"));
}

#[test]
#[serial]
fn config_load_resolves_gh_cli_fallback() {
    let root = tempdir("gh-fallback");
    let path = root.join("config.yaml");
    write(&path, &minimal_null_yaml());

    let env = MapEnv::default();
    let runner = StubGh {
        output: Ok(GhRunnerOutput {
            exit_status: 0,
            stdout: "gh-token".to_string(),
            stderr: String::new(),
        }),
    };
    let cfg = Config::load_from_with_env(&path, &env, &runner).expect("config loads from gh");
    assert_eq!(cfg.github_token.as_deref(), Some("gh-token"));
}

#[test]
#[serial]
fn config_load_strict_error_when_all_sources_empty() {
    let root = tempdir("strict-error");
    let path = root.join("config.yaml");
    write(&path, &minimal_null_yaml());

    let env = MapEnv::default();
    let runner = StubGh { output: Err(()) };
    let err = Config::load_from_with_env(&path, &env, &runner)
        .expect_err("config load must fail when no token source resolves");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("TokenResolution"),
        "expected a TokenResolution error, got: {msg}"
    );
}
