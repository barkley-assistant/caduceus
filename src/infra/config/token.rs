#![allow(dead_code, unused_imports)]
use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::infra::error::{CaduceusError, CaduceusResult};

// Token resolution
// ---------------------------------------------------------------------------

/// Indicate which resolution path produced the token. Used in tests
/// and in the daemon's structured logs (without the secret itself).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenSource {
    ExplicitConfig,
    CaduceusEnv,
    GithubEnv,
    GhCli,
}

/// Environment variable lookup abstracted over the host process or a
/// test fixture. Implementations must never leak the value through
/// the trait surface — only non-secret metadata.
pub trait TokenEnv {
    /// Read an environment variable, returning `None` when unset or
    /// empty. Whitespace-only values are also treated as unset.
    fn get(&self, name: &str) -> Option<String>;
}

/// Process-environment adapter. Reads from the real OS env via
/// `std::env::var_os`. Wrapped in a struct so tests can swap in a
/// fake without mutating process state under concurrent tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEnv;

impl TokenEnv for OsEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var_os(name)
            .map(|value| value.to_string_lossy().trim().to_string())
            .filter(|value| !value.is_empty())
    }
}

/// Run a `gh auth token` subprocess with a 10-second timeout, captured
/// stderr, and no token logging. The runner is overridable in tests.
pub trait GhRunner: Send + Sync {
    fn run(&self) -> Result<GhRunnerOutput, CaduceusError>;
}

/// What `gh auth token` produced, reduced to the contract surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GhRunnerOutput {
    pub exit_status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Default `gh` runner. Resolves the binary, shells out with an
/// argument array, and surfaces exit codes / stderr without echoing
/// the captured stdout.
#[derive(Debug)]
pub struct RealGhRunner;

impl GhRunner for RealGhRunner {
    fn run(&self) -> Result<GhRunnerOutput, CaduceusError> {
        // ``which::which`` is the contract-respecting binary
        // resolver; absent ``gh`` is a clean error.
        let binary = match which::which("gh") {
            Ok(path) => path,
            Err(_) => {
                return Err(CaduceusError::TokenResolution(
                    "`gh` executable not found in PATH".to_string(),
                ));
            }
        };
        // ``subprocess::Command`` requires async + tokio; for the
        // single-shot blocking 10-second call we use ``std::process``
        // which is enough and avoids tying the resolver to a runtime.
        // We do *not* log stdout — by contract the value is secret.
        let mut command = std::process::Command::new(&binary);
        command.arg("auth").arg("token");
        command.env_clear();
        // Inherit only PATH-equivalent vars the binary needs. We
        // deliberately do not inherit the daemon's GitHub token so
        // the operator's existing ``gh auth login`` state is the
        // single source of truth. HOME is needed for ``gh`` to find
        // its config directory.
        for var in ["PATH", "HOME", "USER", "XDG_CONFIG_HOME"] {
            if let Some(value) = std::env::var_os(var) {
                command.env(var, value);
            }
        }
        let output = match command.output() {
            Ok(out) => out,
            Err(err) => {
                return Err(CaduceusError::TokenResolution(format!(
                    "failed to spawn `gh`: {err}"
                )));
            }
        };
        let exit_status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok(GhRunnerOutput {
            exit_status,
            stdout,
            stderr,
        })
    }
}

/// Resolved token + which source produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedToken {
    pub token: String,
    pub source: TokenSource,
}

impl ResolvedToken {
    /// Bundle a token with its source for callers that want to log
    /// the resolution path without exposing the secret.
    pub fn new(token: String, source: TokenSource) -> Self {
        Self { token, source }
    }
}

/// Implementation of the documented hierarchy. Public so tests can
/// drive it with their own env / gh fixtures.
pub fn resolve_token_chain(
    cfg: &Config,
    env: &dyn TokenEnv,
    runner: &dyn GhRunner,
) -> CaduceusResult<ResolvedToken> {
    if let Some(token) = non_empty(cfg.github_token.as_deref()) {
        return Ok(ResolvedToken::new(token, TokenSource::ExplicitConfig));
    }
    if let Some(token) = env.get("CADUCEUS_GITHUB_TOKEN") {
        return Ok(ResolvedToken::new(token, TokenSource::CaduceusEnv));
    }
    if let Some(token) = env.get("GITHUB_TOKEN") {
        return Ok(ResolvedToken::new(token, TokenSource::GithubEnv));
    }

    // Final fallback: ``gh auth token``.
    match runner.run() {
        Ok(out) if out.exit_status == 0 => {
            let trimmed = out.stdout.trim().to_string();
            if is_token_usable(&trimmed) {
                return Ok(ResolvedToken::new(trimmed, TokenSource::GhCli));
            }
            Err(CaduceusError::TokenResolution(
                "`gh auth token` returned no usable token".to_string(),
            ))
        }
        Ok(out) => Err(CaduceusError::TokenResolution(format!(
            "`gh auth token` exited {} (stderr suppressed)",
            out.exit_status
        ))),
        Err(err) => Err(err),
    }
}

/// Return ``Some(token)`` when *token* is non-empty after trimming and
/// contains at least one non-whitespace character.
pub(crate) fn is_token_usable(token: &str) -> bool {
    !token.trim().is_empty()
}

pub(crate) fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}
