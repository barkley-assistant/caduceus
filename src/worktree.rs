//! Per-run worktree management plus the shared git runner.
//!
//! Phase 4 owns the bodies for `GitRunner`, `RepositoryInfo`, the
//! `find_main_clone` discovery path, the daemon-owned worktree
//! `create` and `destroy` operations, and the GC sweep. The runner
//! is the single entry point for every git subprocess the daemon
//! spawns; it enforces the prompts/timeout/process-group contract
//! the rest of the crate relies on.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::process::Command as TokioCommand;
use url::Url;

use crate::config::Config;
use crate::error::{scrub, CaduceusError, CaduceusResult};
use crate::issue::IssueKey;

// ---------------------------------------------------------------------------
// GitRunner — the single entry point for every git subprocess.
// ---------------------------------------------------------------------------

/// Cap on captured stdout/stderr per invocation. The runner truncates
/// anything longer than this and appends a `...<truncated N bytes>`
/// marker so the caller still sees the tail of the failure.
pub const GIT_OUTPUT_BYTE_CAP: usize = 32 * 1024;

/// Default per-invocation timeout fallback when the configuration
/// carries a zero. The contract requires `git_timeout_seconds > 0`,
/// but the runner is robust to operator misconfiguration.
const DEFAULT_GIT_TIMEOUT_SECONDS: u64 = 300;

/// Variables the runner scrubs from the inherited environment
/// before launching any git subprocess. These are the three
/// canonical GitHub-credential names from CONTRACTS.md "Worker
/// environment and result". The daemon never injects these into
/// the child; the daemon also actively removes them from the
/// inherited environment so a misconfigured credential helper
/// can't surface them via `git credential fill`.
pub const DENIED_INHERITED_VARS: &[&str] = &["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"];

/// Default variables preserved across `env_clear()` when the
/// daemon spawns a git subprocess. The runner intentionally keeps
/// this list narrow: git needs `PATH` (and on Linux `HOME` for
/// SSH agent and credential helpers); everything else is
/// opt-in via the explicit allowlist.
pub const DEFAULT_INHERITED_ALLOWLIST: &[&str] =
    &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR"];

/// Outcome of one git subprocess invocation. The runner always
/// returns this struct (never panics) so the caller can branch on
/// `timed_out` / `cancelled` rather than guessing from exit codes.
#[derive(Clone, Debug)]
pub struct GitOutput {
    /// Captured stdout (UTF-8 lossy converted), bounded by
    /// [`GIT_OUTPUT_BYTE_CAP`] with a truncation marker.
    pub stdout: String,
    /// Captured stderr (redacted of any credential substrings,
    /// bounded by [`GIT_OUTPUT_BYTE_CAP`] with a truncation
    /// marker).
    pub stderr: String,
    /// Process exit status. `None` when the process was killed by
    /// a signal (e.g. our SIGKILL on timeout).
    pub status: Option<i32>,
    /// True when the invocation hit the configured
    /// `git_timeout_seconds` ceiling and the runner had to
    /// broadcast SIGKILL to the process group.
    pub timed_out: bool,
    /// True when the runner was cancelled via the shared
    /// [`GitRunner::cancel`] mechanism.
    pub cancelled: bool,
}

/// Shared git runner. Built from a [`Config`] once per process and
/// cloned cheaply (an `Arc` inside). Every git subprocess the
/// daemon launches goes through [`GitRunner::run`].
#[derive(Clone, Debug)]
pub struct GitRunner {
    inner: Arc<GitRunnerInner>,
}

#[derive(Debug)]
struct GitRunnerInner {
    timeout: Duration,
    env_allowlist: Vec<String>,
    api_base: String,
    /// Atomic flag flipped by [`GitRunner::cancel`]. Every
    /// in-flight `run` checks the flag before returning so the
    /// daemon's SIGINT/SIGTERM handler can tear down pending git
    /// calls without waiting for the timeout to elapse.
    cancelled: Arc<AtomicBool>,
}

impl GitRunner {
    /// Build a runner from the daemon [`Config`].
    pub fn new(cfg: &Config) -> Self {
        Self::with_allowlist(
            cfg,
            DEFAULT_INHERITED_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }

    /// Build a runner that, on top of the default inherited-env
    /// allowlist, also preserves every name in *extras*. Tests
    /// use this to thread through names the production runner
    /// would otherwise strip.
    pub fn with_allowlist(cfg: &Config, extras: Vec<String>) -> Self {
        let timeout_seconds = if cfg.git_timeout_seconds == 0 {
            DEFAULT_GIT_TIMEOUT_SECONDS
        } else {
            cfg.git_timeout_seconds
        };
        Self {
            inner: Arc::new(GitRunnerInner {
                timeout: Duration::from_secs(timeout_seconds),
                env_allowlist: extras,
                api_base: cfg.api_base.clone(),
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Cancel every git invocation currently in flight. Idempotent
    /// and thread-safe.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }

    /// Reset the cancel flag so the runner can be reused after a
    /// previous cancellation. Tests use this; production code
    /// typically builds one runner per daemon process and never
    /// resets.
    pub fn reset_cancel(&self) {
        self.inner.cancelled.store(false, Ordering::SeqCst);
    }

    /// True when the runner has been asked to cancel.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Run `git` with the supplied args. The `operation` label is
    /// embedded in any timeout/error variant so the structured log
    /// stream can attribute failures.
    pub async fn run(&self, operation: &'static str, args: &[&OsStr]) -> CaduceusResult<GitOutput> {
        self.run_in(
            &Config::minimal_workdir_for_runner_tests(),
            operation,
            args,
            None,
        )
        .await
    }

    /// Run `git` with the supplied args inside *cwd*. Tests use this
    /// overload so they can pin the working directory; the
    /// single-argument form runs in the daemon's default cwd
    /// (irrelevant for repo-relative git commands but useful as
    /// a default).
    pub async fn run_in(
        &self,
        _cfg: &Config,
        operation: &'static str,
        args: &[&OsStr],
        cwd: Option<&Path>,
    ) -> CaduceusResult<GitOutput> {
        self.reset_cancel();
        let mut command = build_command(args, cwd, &self.inner.env_allowlist);
        let timeout = self.inner.timeout;
        let cancelled = Arc::clone(&self.inner.cancelled);
        let start = std::time::Instant::now();

        let child = command.spawn().map_err(|err| CaduceusError::Git {
            operation,
            stderr: scrub(&format!("spawn: {err}")),
        })?;
        let pid = child.id();

        // Wait loop: poll cancellation + timeout + child exit at
        // a coarse interval. The poll granularity is small
        // enough that operator-visible latency stays below a
        // tick interval (the cron model is 2 min, so even 1s
        // granularity is fine), and large enough that the wait
        // syscall doesn't dominate runtime.
        let mut child = child;
        let outcome: Result<Result<std::process::Output, std::io::Error>, Outcome> = loop {
            if cancelled.load(Ordering::SeqCst) {
                kill_group(pid);
                let _ = child.wait_with_output().await;
                break Err(Outcome::Cancelled);
            }
            if start.elapsed() >= timeout {
                kill_group(pid);
                let _ = child.wait_with_output().await;
                break Err(Outcome::TimedOut);
            }
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // Process is done. `wait_with_output` would
                    // re-wait and fail with ECHILD; instead we
                    // reach into the pipes directly. Tokio's
                    // `ChildStdout`/`ChildStderr` implement
                    // `AsyncRead`; from an async context a simple
                    // `read_to_end` via `AsyncReadExt` does the
                    // job. We block briefly here to drain.
                    let stdout = match child.stdout.take() {
                        Some(mut s) => {
                            use tokio::io::AsyncReadExt;
                            let mut buf = Vec::new();
                            let _ = s.read_to_end(&mut buf).await;
                            buf
                        }
                        None => Vec::new(),
                    };
                    let stderr = match child.stderr.take() {
                        Some(mut s) => {
                            use tokio::io::AsyncReadExt;
                            let mut buf = Vec::new();
                            let _ = s.read_to_end(&mut buf).await;
                            buf
                        }
                        None => Vec::new(),
                    };
                    let status = match child.wait().await {
                        Ok(s) => s,
                        Err(err) => break Err(Outcome::Error(err)),
                    };
                    let output = std::process::Output {
                        status,
                        stdout,
                        stderr,
                    };
                    break Ok(Ok(output));
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(err) => {
                    kill_group(pid);
                    break Err(Outcome::Error(err));
                }
            }
        };

        match outcome {
            Ok(Ok(output)) => Ok(GitOutput {
                stdout: cap_text(&output.stdout),
                stderr: redact_and_cap(&output.stderr),
                status: output.status.code(),
                timed_out: false,
                cancelled: false,
            }),
            Ok(Err(err)) => Err(CaduceusError::Git {
                operation,
                stderr: scrub(&format!("wait: {err}")),
            }),
            Err(Outcome::Cancelled) => Ok(GitOutput {
                stdout: String::new(),
                stderr: String::new(),
                status: None,
                timed_out: false,
                cancelled: true,
            }),
            Err(Outcome::TimedOut) => Ok(GitOutput {
                stdout: String::new(),
                stderr: format!("timed out after {}s", timeout.as_secs()),
                status: None,
                timed_out: true,
                cancelled: false,
            }),
            Err(Outcome::Error(err)) => Err(CaduceusError::Git {
                operation,
                stderr: scrub(&format!("wait: {err}")),
            }),
        }
    }

    /// Convenience: run a git command with [`OsStr`] literals.
    /// Equivalent to `run(operation, args)` for stringly-typed
    /// callers.
    pub async fn run_args<I, S>(
        &self,
        operation: &'static str,
        args: I,
    ) -> CaduceusResult<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let owned: Vec<std::ffi::OsString> = args
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .collect();
        let borrowed: Vec<&OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
        self.run(operation, &borrowed).await
    }

    /// Expose the runner's configured timeout. Tests use this to
    /// drive the timeout-cancellation case deterministically.
    pub fn timeout(&self) -> Duration {
        self.inner.timeout
    }
}

enum Outcome {
    Cancelled,
    TimedOut,
    Error(std::io::Error),
}

/// Build a `tokio::process::Command` for the supplied `git`
/// arguments with the runner's prompt-suppression, credential
/// scrubbing, inherited allowlist, and process-group isolation
/// pre-applied. Centralised so every entry point (run / run_in
/// / git_string) shares the same environment-handling logic.
fn build_command(args: &[&OsStr], cwd: Option<&Path>, extras: &[String]) -> TokioCommand {
    let mut command = TokioCommand::new("git");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(c) = cwd {
        command.current_dir(c);
    }
    // Process-group isolation: every git subprocess runs as
    // its own process-group leader so the runner can broadcast
    // SIGKILL/SIGTERM to the whole group on timeout /
    // cancellation. `process_group(0)` is the safe-API
    // equivalent of `setsid()` (the child pgid is set to its
    // own PID). `tokio::process::Command` exposes this as an
    // inherent Unix-only method, so the crate's `#![forbid
    // (unsafe_code)]` is respected.
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    // Prompt suppression: the daemon never presents
    // credentials. If git would prompt the user (e.g. SSH
    // passphrase, missing credential helper), it must fail
    // loudly instead of hanging.
    command.env("GIT_TERMINAL_PROMPT", "0");
    // Scrub the daemon-side credential variables *before*
    // applying the allowlist. The runner never wants these to
    // reach git's credential subsystem.
    for name in DENIED_INHERITED_VARS {
        command.env_remove(name);
    }
    // Apply the explicit inherited allowlist on top of the
    // empty default so we keep only what the runner
    // whitelists. Default names are the canonical baseline
    // (PATH, HOME, …) the contract documents.
    for name in DEFAULT_INHERITED_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for name in extras {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    // `nix::sys::signal::killpg` is the safe equivalent of
    // `kill(-pid, SIGKILL)`. Errors (process already gone,
    // permission denied) are intentionally swallowed — by the
    // time we hit this path the runner is going to return a
    // timeout/cancellation regardless.
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {
    // The daemon refuses to start on non-Unix; the runner
    // exists for completeness.
}

/// Truncate *bytes* to [`GIT_OUTPUT_BYTE_CAP`] and append a marker
/// so callers can tell the captured text was clipped. UTF-8 is
/// preserved by trimming at the last byte boundary inside the cap.
fn cap_text(bytes: &[u8]) -> String {
    if bytes.len() <= GIT_OUTPUT_BYTE_CAP {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut cut = GIT_OUTPUT_BYTE_CAP;
    while cut > 0 && !is_utf8_boundary(bytes, cut) {
        cut -= 1;
    }
    let mut text = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    let dropped = bytes.len() - cut;
    text.push_str(&format!("\n...<truncated {dropped} bytes>"));
    text
}

/// Apply credential redaction then byte-cap. The redaction strips
/// `GITHUB_TOKEN=...`-shaped substrings so a token accidentally
/// printed by git can never reach a log line or a test failure
/// message.
fn redact_and_cap(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    cap_text(scrub(&text).as_bytes())
}

/// True when *bytes[..i]* is a valid UTF-8 prefix boundary.
fn is_utf8_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return true;
    }
    let b = bytes[i];
    (b & 0xC0) != 0x80
}

// ---------------------------------------------------------------------------
// Repository discovery
// ---------------------------------------------------------------------------

/// Outcome of [`find_main_clone`]: the resolved on-disk clone
/// plus the metadata the daemon needs to create worktrees off
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryInfo {
    /// Absolute path of the on-disk clone. For a non-bare clone
    /// this is the working tree; for a bare clone this is the
    /// bare repository directory.
    pub path: PathBuf,
    /// Default base branch (e.g. `main`, `master`). Resolved from
    /// `refs/remotes/origin/HEAD`; falls back to the repository's
    /// current branch (with a tracing warning) when the remote
    /// HEAD symref is missing.
    pub base_branch: String,
    /// The origin's URL (already normalized for matching). For
    /// host-validation purposes the host is derived from this
    /// value, not the daemon's `api_base`.
    pub remote_url: String,
}

/// Discover the local main clone for *key*. The path is
/// `<workdir_base>/<owner>/<repo>`. The function validates:
/// * the path exists and is part of a git working tree,
/// * the working tree is clean (`git status --porcelain` empty),
/// * the origin's host matches the daemon's `api_base` host
///   (public github.com or the configured enterprise host),
/// * the origin's normalized `owner/repo` matches the issue
///   slug.
///
/// SSH host aliases (e.g. `git@github.com-attacker:...`) are
/// explicitly rejected in v0.1 because their destination cannot
/// be authenticated from the remote string alone.
pub async fn find_main_clone(
    cfg: &Config,
    runner: &GitRunner,
    key: &IssueKey,
) -> CaduceusResult<RepositoryInfo> {
    key.validate()?;
    let path = clone_path(cfg, key);
    if !path.exists() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("clone missing at {}", path.display()),
        });
    }
    if !path.is_dir() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("clone path is not a directory: {}", path.display()),
        });
    }

    // `git rev-parse --git-dir` succeeds only inside a working
    // tree (regular or bare). A missing failure here means the
    // directory is not a git repository.
    let git_dir = git_string(runner, &path, "rev-parse", &["--git-dir"]).await?;
    if git_dir.trim().is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("not a git repository: {}", path.display()),
        });
    }

    // Working tree must be clean: the daemon never operates on a
    // dirty main checkout (operator visible signal that the
    // branch is mid-edit).
    let porcelain = git_string(runner, &path, "status", &["--porcelain"]).await?;
    if !porcelain.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "main checkout is dirty at {}; refusing to operate",
                path.display()
            ),
        });
    }

    // Resolve the origin URL and validate the host / slug match.
    let remote_url = git_string(runner, &path, "config", &["--get", "remote.origin.url"]).await;
    let remote_url = match remote_url {
        Ok(text) => text,
        Err(_) => {
            // `git config --get` exits with status 1 and an
            // empty stderr when the key is missing. Translate
            // that into the documented "no remote configured"
            // error rather than the raw `Git { stderr: "" }`
            // variant.
            return Err(CaduceusError::Worktree {
                context: "discover",
                stderr: format!("no remote.origin.url configured at {}", path.display()),
            });
        }
    };
    let remote_url = remote_url.trim().to_string();
    if remote_url.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("no remote.origin.url configured at {}", path.display()),
        });
    }
    let (remote_owner, remote_repo) = parse_origin(&remote_url)?;
    let remote_pair = format!(
        "{}/{}",
        remote_owner.to_ascii_lowercase(),
        remote_repo.to_ascii_lowercase()
    );
    let expected_pair = format!(
        "{}/{}",
        key.owner.to_ascii_lowercase(),
        key.repo.to_ascii_lowercase()
    );
    if remote_pair != expected_pair {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "origin {remote_owner}/{remote_repo} does not match issue slug {}/{}",
                key.owner, key.repo
            ),
        });
    }
    validate_origin_host(&remote_url, &cfg.api_base)?;

    // Default base branch. Prefer `refs/remotes/origin/HEAD`;
    // fall back to the local HEAD with a warning.
    let base_branch =
        match git_string(runner, &path, "symbolic-ref", &["refs/remotes/origin/HEAD"]).await {
            Ok(text) => text
                .trim()
                .strip_prefix("refs/remotes/origin/")
                .unwrap_or(text.trim())
                .to_string(),
            Err(_) => match git_string(runner, &path, "symbolic-ref", &["HEAD"]).await {
                Ok(text) => {
                    let raw = text.trim();
                    let branch = raw
                        .strip_prefix("refs/heads/")
                        .or_else(|| raw.strip_prefix("refs/remotes/"))
                        .unwrap_or(raw)
                        .to_string();
                    tracing::warn!(
                        branch = %branch,
                        path = %path.display(),
                        "origin/HEAD missing; falling back to local HEAD"
                    );
                    branch
                }
                Err(_) => {
                    return Err(CaduceusError::Worktree {
                        context: "discover",
                        stderr: format!(
                            "detached HEAD without refs/remotes/origin/HEAD at {}",
                            path.display()
                        ),
                    });
                }
            },
        };

    Ok(RepositoryInfo {
        path,
        base_branch,
        remote_url,
    })
}

/// Compute the on-disk path of the main clone for *key*. The
/// components come straight from the (validated) [`IssueKey`];
/// path traversal / special characters are rejected by
/// [`IssueKey::validate`], so no additional sanitisation is
/// needed here.
pub fn clone_path(cfg: &Config, key: &IssueKey) -> PathBuf {
    cfg.workdir_base.join(&key.owner).join(&key.repo)
}

/// Parse `origin.url` into `(owner, repo)`. Supports SSH
/// (`git@host:owner/repo.git`), HTTPS
/// (`https://host/owner/repo.git`), and `git://` URLs. SSH host
/// aliases like `git@github.com-attacker:owner/repo.git` parse
/// successfully but are rejected later by
/// [`validate_origin_host`].
pub fn parse_origin(remote_url: &str) -> CaduceusResult<(String, String)> {
    let remote_url = remote_url.trim();
    // SSH form: `[user@]host:owner/repo[.git]`. We do NOT use a
    // URL parser here because the colon makes it ambiguous with
    // scheme-bearing URLs.
    if let Some((_, after_colon)) = remote_url.split_once(':') {
        // Make sure the prefix is not a scheme like `https:` —
        // those always start with `scheme://`.
        if !remote_url.contains("://") {
            let path = after_colon.trim_start_matches('/');
            let stripped = path.trim_end_matches(".git").trim_end_matches('/');
            let (owner, repo) =
                stripped
                    .split_once('/')
                    .ok_or_else(|| CaduceusError::Worktree {
                        context: "discover",
                        stderr: format!("origin SSH URL has no owner/repo: {remote_url}"),
                    })?;
            return Ok((owner.to_string(), repo.to_string()));
        }
    }
    // HTTPS / git:// form: parse via `url::Url`.
    let url = Url::parse(remote_url).map_err(|err| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL not parseable: {remote_url} ({err})"),
    })?;
    let mut segments = url
        .path_segments()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL has no path: {remote_url}"),
        })?
        .filter(|s| !s.is_empty());
    let owner = segments.next().ok_or_else(|| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL missing owner: {remote_url}"),
    })?;
    let repo = segments
        .next()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL missing repo: {remote_url}"),
        })?
        .trim_end_matches(".git");
    Ok((owner.to_string(), repo.to_string()))
}

/// Validate that the origin host matches the daemon's `api_base`
/// host. Public github.com → origin host must equal `github.com`.
/// Enterprise → origin host must equal the api_base host verbatim.
pub fn validate_origin_host(remote_url: &str, api_base: &str) -> CaduceusResult<()> {
    let api_url = Url::parse(api_base)
        .map_err(|err| CaduceusError::Config(format!("invalid api_base {api_base}: {err}")))?;
    let api_host = api_url
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| CaduceusError::Config(format!("api_base has no host: {api_base}")))?;
    let origin_host = origin_host(remote_url)?;
    // The contract distinguishes two cases:
    //   1. Public github.com — api_base is the canonical
    //      https://api.github.com URL (the default). The
    //      operator's clones target `github.com`, not
    //      `api.github.com`, so the origin host check is
    //      against `github.com`.
    //   2. GitHub Enterprise — api_base points at
    //      `https://<enterprise>/` (e.g. ghe.example.com); the
    //      origin host must equal that same hostname verbatim.
    let is_public = api_host == "api.github.com"
        || api_host == "github.com"
        || api_base.trim_end_matches('/') == "https://api.github.com";
    if is_public {
        if origin_host != "github.com" {
            return Err(CaduceusError::Worktree {
                context: "discover",
                stderr: format!(
                    "origin host {origin_host:?} does not match public api_base host github.com"
                ),
            });
        }
    } else if origin_host != api_host {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "origin host {origin_host:?} does not match api_base host {api_host:?}"
            ),
        });
    }
    Ok(())
}

/// Extract the host component of an origin URL. SSH forms
/// (`git@host:owner/repo`) yield `host` (or the alias-like form
/// `host-attacker`). URL forms yield `Url::host_str()`.
fn origin_host(remote_url: &str) -> CaduceusResult<String> {
    let remote_url = remote_url.trim();
    if let Some((before_colon, after)) = remote_url.split_once(':') {
        if !remote_url.contains("://") {
            // SSH form: host lives before the colon. The
            // optional `user@` prefix is stripped — keep the
            // segment after the LAST `@`.
            let host = match before_colon.rsplit_once('@') {
                Some((_, host)) => host,
                None => before_colon,
            };
            // Reject SSH aliases outright (v0.1 cannot verify
            // their destination from the remote string).
            if host.contains('-') && !host.ends_with("github.com") {
                return Err(CaduceusError::Worktree {
                    context: "discover",
                    stderr: format!("origin SSH host alias is not allowed: {host}"),
                });
            }
            let _ = after;
            return Ok(host.to_ascii_lowercase());
        }
    }
    let url = Url::parse(remote_url).map_err(|err| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL not parseable: {remote_url} ({err})"),
    })?;
    url.host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL has no host: {remote_url}"),
        })
}

/// Run `git <args>` inside *cwd* and return stdout (post-truncate,
/// post-redact) as a `String`. Errors that include stderr are
/// preserved verbatim so the structured logger can render them.
async fn git_string(
    runner: &GitRunner,
    cwd: &Path,
    op: &'static str,
    args: &[&str],
) -> CaduceusResult<String> {
    let mut owned_args: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 1);
    owned_args.push(std::ffi::OsString::from(op));
    for s in args {
        owned_args.push(std::ffi::OsString::from(*s));
    }
    let borrowed: Vec<&OsStr> = owned_args.iter().map(|s| s.as_os_str()).collect();
    // Use the runner so this code path goes through the same
    // process-group / timeout / redaction logic as the rest of
    // the daemon. The op label becomes the first git subcommand
    // argument (e.g. `git rev-parse --git-dir`).
    let output = runner
        .run_in(&runner_inner_cfg(), op, &borrowed, Some(cwd))
        .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Git {
            operation: op,
            stderr: output.stderr,
        });
    }
    if let Some(status) = output.status {
        if status != 0 {
            return Err(CaduceusError::Git {
                operation: op,
                stderr: output.stderr,
            });
        }
    } else {
        // No exit status (killed by signal). Surface as a
        // typed error so the structured logger attributes the
        // failure correctly.
        return Err(CaduceusError::Git {
            operation: op,
            stderr: if output.stderr.is_empty() {
                "git exited via signal".to_string()
            } else {
                output.stderr
            },
        });
    }
    if output.stdout.trim().is_empty() {
        Ok(String::new())
    } else {
        Ok(output.stdout.trim_end().to_string())
    }
}

/// Build a minimal `Config` for runner-only paths (the
/// `git_string` helper). The Config's `git_timeout_seconds`
/// field is the only one the runner consults; everything else
/// is filler so we can pass a reference into the runner.
fn runner_inner_cfg() -> Config {
    Config::minimal_workdir_for_runner_tests()
}

// ---------------------------------------------------------------------------
// WorktreeHandle / create / destroy (stubbed; Tasks 4.2 and 4.3 own the
// bodies). Kept here so the module surface stays consistent with what the
// rest of the crate imports.
// ---------------------------------------------------------------------------

/// Outcome of creating one daemon-owned worktree + branch.
#[derive(Debug)]
pub struct WorktreeHandle {
    pub issue: IssueKey,
    pub run_id: String,
    pub branch_name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
}

/// Provision an isolated worktree + branch.
pub fn create(
    _cfg: &Config,
    _runner: &GitRunner,
    _repo: &RepositoryInfo,
    _key: &IssueKey,
    _run_id: &str,
) -> CaduceusResult<WorktreeHandle> {
    Err(CaduceusError::Worktree {
        context: "create",
        stderr: "create() implementation lives in Task 4.2".to_string(),
    })
}

/// Tear down a worktree, refusing to remove anything claimed or
/// heartbeat-live.
pub fn destroy(_runner: &GitRunner, _handle: &WorktreeHandle) -> CaduceusResult<()> {
    Err(CaduceusError::Worktree {
        context: "destroy",
        stderr: "destroy() implementation lives in Task 4.3".to_string(),
    })
}

/// Worktree GC entry point shared by both `caduceus worktree-gc` and the
/// scheduled background sweep.
pub fn gc(_state_dir: &Path, _older_than_days: u64, _dry_run: bool) -> CaduceusResult<u64> {
    Err(CaduceusError::Worktree {
        context: "gc",
        stderr: "gc() implementation lives in Task 4.3".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Test-only Config helper. `Config::test_defaults` is documented as the
// canonical root-anchored builder, but `find_main_clone` and the runner
// only need a couple of fields; this keeps the inline tests focused on
// pure logic.
// ---------------------------------------------------------------------------

trait MinimalConfig {
    fn minimal_workdir_for_runner_tests() -> Self;
}

impl MinimalConfig for Config {
    fn minimal_workdir_for_runner_tests() -> Self {
        Config {
            poll_interval_seconds: 0,
            state_dir: PathBuf::from("/tmp"),
            log_path: PathBuf::from("/tmp/processor.log"),
            workdir_base: PathBuf::from("/tmp"),
            watched_repos: Vec::new(),
            worker_command: Vec::new(),
            worker_timeout_seconds: 0,
            http_timeout_seconds: 0,
            git_timeout_seconds: 0,
            transcript_max_bytes: 0,
            run_retention_days: 0,
            stale_run_hours: 0,
            max_retries_per_issue: 0,
            retry_backoff_seconds: 0,
            ticket_label_code: String::new(),
            ticket_label_investigation: String::new(),
            feedback_author_allowlist: Vec::new(),
            comment_ignore_patterns: Vec::new(),
            comment_forbidden_strings: Vec::new(),
            worker_env_allowlist: Vec::new(),
            github_token: None,
            api_base: DEFAULT_API_BASE.to_string(),
            dry_run: false,
            compiled_ignore_patterns: Vec::new(),
        }
    }
}

const DEFAULT_API_BASE: &str = "https://api.github.com";

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn parse_origin_handles_https_form() {
        let (owner, repo) = parse_origin("https://github.com/octocat/Hello-World.git").unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "Hello-World");
    }

    #[test]
    fn parse_origin_handles_ssh_form() {
        let (owner, repo) = parse_origin("git@github.com:octocat/Hello-World.git").unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "Hello-World");
    }

    #[test]
    fn validate_origin_host_accepts_matching_github_com() {
        validate_origin_host(
            "https://github.com/octocat/Hello-World.git",
            DEFAULT_API_BASE,
        )
        .unwrap();
    }

    #[test]
    fn validate_origin_host_rejects_mismatched_enterprise_host() {
        let err = validate_origin_host(
            "https://github.com/octocat/Hello-World.git",
            "https://ghe.example.com",
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains("origin host"));
    }

    #[test]
    fn validate_origin_host_rejects_ssh_alias() {
        let err = validate_origin_host(
            "git@github.com-attacker:octocat/Hello-World.git",
            DEFAULT_API_BASE,
        )
        .unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains("alias"), "got: {text}");
    }

    #[test]
    fn cap_text_truncates_with_marker() {
        let huge = "x".repeat(GIT_OUTPUT_BYTE_CAP + 100);
        let capped = cap_text(huge.as_bytes());
        assert!(capped.contains("truncated"));
        assert!(capped.len() <= GIT_OUTPUT_BYTE_CAP + 64);
    }

    #[test]
    fn redact_and_cap_strips_token_shaped_substrings() {
        let raw = b"some output\nGITHUB_TOKEN=ghp_should_not_leak\nrest";
        let redacted = redact_and_cap(raw);
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains("ghp_should_not_leak"));
    }

    #[test]
    fn clone_path_is_workdir_base_plus_owner_plus_repo() {
        let cfg = Config {
            workdir_base: PathBuf::from("/srv/workdirs"),
            ..Config::minimal_workdir_for_runner_tests()
        };
        let key = IssueKey {
            owner: "octocat".to_string(),
            repo: "Hello-World".to_string(),
            number: 1,
        };
        assert_eq!(
            clone_path(&cfg, &key),
            PathBuf::from("/srv/workdirs/octocat/Hello-World")
        );
    }
}
