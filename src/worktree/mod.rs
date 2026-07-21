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
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use nix::unistd::pipe;
use tokio::process::Command as TokioCommand;
use url::Url;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{scrub, CaduceusError, CaduceusResult};

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

/// Outcome of one git subprocess invocation that preserves raw
/// stdout bytes (for NUL-delimited `-z` output). Structurally
/// identical to [`GitOutput`] but keeps the raw `Vec<u8>` instead
/// of converting via `String::from_utf8_lossy`. Callers that need
/// NUL-byte splitting use this variant.
#[derive(Clone, Debug)]
pub struct GitOutputRaw {
    /// Raw stdout bytes, uncapped — the caller splits on NUL
    /// before text-processing.
    pub stdout: Vec<u8>,
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
    /// Anonymous pipe read-fd for the GIT_ASKPASS credential
    /// helper. When set, `build_command` sets `GIT_ASKPASS` and
    /// `GIT_ASKPASS_FD` so the helper reads the PAT from this
    /// fd. `None` when no PAT is available (e.g. public repo or
    /// non-Unix fallback).
    #[cfg(unix)]
    credential_helper_fd: Option<std::os::unix::io::OwnedFd>,
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
        // Set up the anonymous-pipe credential broker when a
        // github_token is available. The PAT is written to the
        // write end; the read end fd is stored for
        // `build_command` to pass via `GIT_ASKPASS_FD`.
        #[cfg(unix)]
        let credential_helper_fd = Self::setup_credential_broker(cfg);
        #[cfg(not(unix))]
        let _credential_helper_fd: Option<std::os::unix::io::OwnedFd> = None;
        Self {
            inner: Arc::new(GitRunnerInner {
                timeout: Duration::from_secs(timeout_seconds),
                env_allowlist: extras,
                api_base: cfg.api_base.clone(),
                #[cfg(unix)]
                credential_helper_fd,
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

    /// Set up the anonymous-pipe credential broker (Unix only).
    /// Creates a pipe, writes the github_token (if present) to the
    /// write end, closes the write end, and returns the read end fd.
    /// When no token is configured, returns `None`.
    #[cfg(unix)]
    fn setup_credential_broker(cfg: &Config) -> Option<std::os::unix::io::OwnedFd> {
        let token = cfg.github_token.as_ref()?;
        let (read_fd, write_fd) = pipe().ok()?;
        // Write the PAT followed by a newline into the pipe. The
        // git-askpass helper reads this and outputs username/password.
        let mut bytes = token.as_bytes().to_vec();
        bytes.push(b'\n');
        let _ = nix::unistd::write(&write_fd, &bytes);
        // Close the write end — the read end stays open for the child.
        drop(write_fd);
        Some(read_fd)
    }

    /// Fallback credential broker for non-Unix platforms: write the
    /// PAT to a temp file, return its path. The caller must unlink
    /// after exec. (The daemon refuses non-Unix; this exists for
    /// completeness.)
    #[cfg(not(unix))]
    fn setup_credential_broker(cfg: &Config) -> Option<std::path::PathBuf> {
        let _ = cfg;
        None
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
        let mut command = build_command(
            args,
            cwd,
            &self.inner.env_allowlist,
            #[cfg(unix)]
            self.inner.credential_helper_fd.as_ref(),
        );
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

    /// Run `git` with the supplied args and return raw stdout bytes
    /// (uncapped, un-lossy-converted). Callers use this for `-z`
    /// (NUL-delimited) commands that need byte-level splitting.
    /// The wait-loop, timeout, cancellation, env-scrubbing, and
    /// process-group isolation are identical to [`run_in`].
    pub async fn run_in_raw(
        &self,
        _cfg: &Config,
        operation: &'static str,
        args: &[&OsStr],
        cwd: Option<&Path>,
    ) -> CaduceusResult<GitOutputRaw> {
        self.reset_cancel();
        let mut command = build_command(
            args,
            cwd,
            &self.inner.env_allowlist,
            #[cfg(unix)]
            self.inner.credential_helper_fd.as_ref(),
        );
        let timeout = self.inner.timeout;
        let cancelled = Arc::clone(&self.inner.cancelled);
        let start = std::time::Instant::now();

        let child = command.spawn().map_err(|err| CaduceusError::Git {
            operation,
            stderr: scrub(&format!("spawn: {err}")),
        })?;
        let pid = child.id();

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
            Ok(Ok(output)) => Ok(GitOutputRaw {
                stdout: output.stdout,
                stderr: redact_and_cap(&output.stderr),
                status: output.status.code(),
                timed_out: false,
                cancelled: false,
            }),
            Ok(Err(err)) => Err(CaduceusError::Git {
                operation,
                stderr: scrub(&format!("wait: {err}")),
            }),
            Err(Outcome::Cancelled) => Ok(GitOutputRaw {
                stdout: Vec::new(),
                stderr: String::new(),
                status: None,
                timed_out: false,
                cancelled: true,
            }),
            Err(Outcome::TimedOut) => Ok(GitOutputRaw {
                stdout: Vec::new(),
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
        let owned: Vec<std::ffi::OsString> =
            args.into_iter().map(|s| s.as_ref().to_owned()).collect();
        let borrowed: Vec<&OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
        self.run(operation, &borrowed).await
    }

    /// Expose the runner's configured timeout. Tests use this to
    /// drive the timeout-cancellation case deterministically.
    pub fn timeout(&self) -> Duration {
        self.inner.timeout
    }

    /// Switch the process-wide umask to `0o022` for worktree
    /// mutations (preserving source-file executable bits), execute
    /// the closure, then restore `0o077`. The operation is serialized
    /// via a process-wide [`Mutex`] to prevent races with other
    /// threads.
    ///
    /// Uses [`nix::sys::stat::umask`] which is a safe wrapper
    /// around the POSIX `umask(2)` call — no `unsafe` required.
    pub fn with_worktree_umask<F, T>(f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let _guard = UMASK_MUTEX.lock().expect("umask mutex poisoned");
        let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
        let result = f();
        nix::sys::stat::umask(previous);
        result
    }
}

/// Process-wide umask guard. Only the `GitRunner` acquires this
/// mutex, ensuring that concurrent worktree mutations do not
/// race on the process-level umask.
static UMASK_MUTEX: Mutex<()> = Mutex::new(());

enum Outcome {
    Cancelled,
    TimedOut,
    Error(std::io::Error),
}

/// Write a credential helper script to a temp directory and return
/// its path. The helper reads from the fd number passed via
/// `GIT_ASKPASS_FD` and outputs the git-askpass protocol:
///   username=abc
///   password=<token>
/// The script is a shell one-liner. Returns `None` on write failure.
#[cfg(unix)]
fn write_credential_helper(fd: &std::os::unix::io::OwnedFd) -> Option<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    let fd_num = fd.as_raw_fd();
    let tmp_dir = std::env::temp_dir();
    let helper_path = tmp_dir.join(format!("caduceus-askpass-{}", std::process::id()));
    let script = format!(
        "#!/bin/sh\nread -r token <&{fd_num}\necho \"username=token\"\necho \"password=$token\"\n"
    );
    let mut file = std::fs::File::create(&helper_path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o700))
            .ok()?;
    }
    file.write_all(script.as_bytes()).ok()?;
    drop(file);
    Some(helper_path)
}

/// Build a `tokio::process::Command` for the supplied `git`
/// arguments with the runner's prompt-suppression, credential
/// scrubbing, inherited allowlist, process-group isolation,
/// ambient-config neutralisation, and credential-broker env
/// pre-applied. Centralised so every entry point (run / run_in
/// / run_in_raw / git_string) shares the same environment-
/// handling logic.
///
/// The following hardening is applied:
/// * Ambient config neutralisation: `-c core.hooksPath=/dev/null`
///   (prepended, AC-04), `GIT_CONFIG_NOSYSTEM=1` (env, AC-04),
///   `GIT_DIR`/`GIT_WORK_TREE` set from `cwd` (AC-04).
/// * Credential broker: `GIT_ASKPASS` and `GIT_ASKPASS_FD` set
///   when *credential_fd* is `Some` (AC-05).
fn build_command(
    args: &[&OsStr],
    cwd: Option<&Path>,
    extras: &[String],
    #[cfg(unix)] credential_fd: Option<&std::os::unix::io::OwnedFd>,
) -> TokioCommand {
    let mut command = TokioCommand::new("git");
    // Prepend `-c core.hooksPath=/dev/null` BEFORE user args so
    // hooks can never fire.
    command.arg("-c");
    command.arg("core.hooksPath=/dev/null");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(c) = cwd {
        command.current_dir(c);
        // Set GIT_DIR / GIT_WORK_TREE from the cwd so ambient
        // GIT_DIR env vars cannot redirect operations.
        command.env("GIT_DIR", c.join(".git"));
        command.env("GIT_WORK_TREE", c);
    }
    // Suppress system/user level config so no ambient
    // credential helper or hook can influence the operation.
    command.env("GIT_CONFIG_NOSYSTEM", "1");
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
    // Credential broker: when a credential helper fd is configured,
    // set GIT_ASKPASS to a helper that reads from the fd. The
    // helper is a shell one-liner stored in the implicit env.
    // We use a built-in helper path: the daemon writes a small
    // helper script to tmp and references it via GIT_ASKPASS.
    // The fd number is passed via GIT_ASKPASS_FD.
    // (This is done after the allowlist so the env vars stick.)
    #[cfg(unix)]
    if let Some(fd) = credential_fd {
        use std::os::unix::io::AsRawFd;
        // Write a helper script that reads from the fd and outputs
        // the git-askpass protocol. The script is a shell one-liner.
        let helper_path = write_credential_helper(fd);
        if let Some(path) = helper_path {
            command.env("GIT_ASKPASS", path.to_string_lossy().as_ref());
            command.env("GIT_ASKPASS_FD", fd.as_raw_fd().to_string());
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
// Worktree (created by `create`, torn down by `destroy`, GCed by `gc`).
// ---------------------------------------------------------------------------

/// Outcome of creating one daemon-owned worktree + branch. The
/// daemon owns the branch name (invariant #5) and the canonical
/// worktree path; worker code never selects a ref or a path.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// Issue this worktree is provisioned for. The daemon
    /// re-exports `display_key()` so callers can derive stable
    /// filenames without reaching into the issue module.
    pub issue: IssueKey,
    /// Run ID, used as the worktree directory basename and (in
    /// lowercase form) as the branch suffix.
    pub run_id: String,
    /// Daemon-owned branch name of the form
    /// `automation/issue-<number>-<lowercase-run-id>`.
    pub branch_name: String,
    /// Absolute worktree path `<repo>/.worktrees/<run_id>`.
    pub path: PathBuf,
    /// SHA-1 of the base commit the branch was created from
    /// (i.e. the OID of `origin/<base>` at fetch time).
    pub base_oid: String,
    /// Whether this `create` call produced the worktree (true)
    /// or reconciled with a leftover owned by the same run id
    /// (false). Callers can use this to gate downstream side
    /// effects (e.g. resume checkpoints only trigger a fresh
    /// branch when `fresh = true`).
    pub fresh: bool,
    pub created_at: DateTime<Utc>,
}

/// Provision an isolated worktree + branch. The flow per
/// `tasks/4.2-create-a-daemon-owned-worktree-and-branch.md` is:
///
/// 1. Validate the run id (no path traversal, no shell
///    metacharacters). Run id must match `[A-Za-z0-9_-]{1,64}`.
/// 2. Compute the daemon-owned branch
///    `automation/issue-<number>-<run_id-lowercase>` and the
///    worktree path `<repo>/.worktrees/<run_id>`.
/// 3. Validate the branch shape with `git check-ref-format
///    --branch` (per task spec).
/// 4. Take an `fs2` flock on `<repo>/.worktrees/.lock` so
///    concurrent `create` invocations on the same main clone
///    serialize and cannot race on a shared path/branch
///    (atomic claim-of-worktree-path).
/// 5. Pre-flight: if a branch with the same name already
///    exists, inspect whether it points at `origin/<base>`;
///    if so we reconcile, otherwise we return a collision
///    error. Same logic for the path.
/// 6. `git fetch --prune origin <base>` inside the main clone.
/// 7. `git worktree add -b <branch> <path> origin/<base>`.
/// 8. Resolve the recorded `base_oid` via `git rev-parse
///    refs/remotes/origin/<base>` and return.
pub async fn create(
    cfg: &Config,
    runner: &GitRunner,
    repo: &RepositoryInfo,
    key: &IssueKey,
    run_id: &str,
) -> CaduceusResult<Worktree> {
    key.validate()?;

    // (1) Validate run id. The path basename and branch suffix
    // both flow from this string; both must be safe.
    validate_run_id(run_id)?;

    // (2) Compute branch + path. Branch is lowercased per the
    // task packet; path keeps the original case so two
    // different-case run ids can coexist.
    let branch_name = format!(
        "automation/issue-{}-{}",
        key.number,
        run_id.to_ascii_lowercase()
    );
    let worktree_path = repo.path.join(".worktrees").join(run_id);

    // (3) Validate the branch shape with git itself (per task
    // spec). `git check-ref-format --branch <name>` exits 0
    // when the branch name is a valid branch name under the
    // documented rules; non-zero otherwise.
    git_check_branch_format(runner, &repo.path, &branch_name).await?;

    // (4) Atomic claim-of-worktree-path under the worker-home
    // area. The flock lives at `<repo>/.worktrees/.lock` so
    // every `create` call on the same main clone serialises on
    // a directory that's already in the worktree-parent path.
    let worktree_parent = worktree_path
        .parent()
        .ok_or_else(|| CaduceusError::Other("worktree path has no parent".to_string()))?
        .to_path_buf();
    fs::create_dir_all(&worktree_parent).map_err(|err| CaduceusError::Worktree {
        context: "create",
        stderr: format!(
            "create worker-home {} failed: {err}",
            worktree_parent.display()
        ),
    })?;
    let lock_path = worktree_parent.join(".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| CaduceusError::Worktree {
            context: "create",
            stderr: format!("open worktree lock {}: {err}", lock_path.display()),
        })?;
    if let Err(err) = lock_file.lock_exclusive() {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("lock worktree-home {}: {err}", lock_path.display()),
        });
    }

    let result = create_locked(cfg, runner, repo, key, run_id, &branch_name, &worktree_path).await;

    // Release the flock regardless of outcome. `fs2::FileExt`
    // documents that the lock is released on close; explicit
    // `unlock` here keeps the lock held file usable for
    // further flock-based coordination in Phase 5/7.
    let _ = FileExt::unlock(&lock_file);
    result
}

/// Body of [`create`] executed while the worktree-home flock is
/// held. Factored out so the lock is released even on early
/// returns.
async fn create_locked(
    cfg: &Config,
    runner: &GitRunner,
    repo: &RepositoryInfo,
    key: &IssueKey,
    run_id: &str,
    branch_name: &str,
    worktree_path: &Path,
) -> CaduceusResult<Worktree> {
    let _ = cfg;

    // (5) Pre-flight: branch / path already exist? Resolve
    // each case to "ours" (reconcile) or "theirs" (collision).
    let pre = inspect_existing(runner, &repo.path, branch_name, worktree_path).await?;
    if pre.foreign_branch {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "branch collision: {branch_name} already exists with a different run id"
            ),
        });
    }
    if pre.foreign_path {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "path collision: {} already exists with a different run id",
                worktree_path.display()
            ),
        });
    }
    // Any foreign entry under `.worktrees/` is a collision —
    // the daemon owns the worker-home area and never allows a
    // prior run to leak paths.
    if let Some(foreign) = pre.foreign_worktree_dir {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "path collision: {} already exists under the worker's home (foreign run id)",
                foreign.display()
            ),
        });
    }
    if pre.owned {
        if let Some(base_oid) = pre.base_oid {
            // Idempotent re-entry into the same run id: return
            // the existing handle so callers can resume.
            return Ok(Worktree {
                issue: key.clone(),
                run_id: run_id.to_string(),
                branch_name: branch_name.to_string(),
                path: worktree_path.to_path_buf(),
                base_oid,
                fresh: false,
                created_at: pre.created_at.unwrap_or_else(Utc::now),
            });
        }
    }

    // (5b) Materialize the worker-home area now that pre-flight
    // is clean. The flock is held so no other daemon tick can
    // race us between create-dir-all and worktree-add.
    fs::create_dir_all(worktree_path.parent().unwrap()).map_err(|err| CaduceusError::Worktree {
        context: "create",
        stderr: format!(
            "create worker-home {} failed: {err}",
            worktree_path.parent().unwrap().display()
        ),
    })?;

    // (6) Fetch --prune on the documented ref so stale remote
    // refs are removed and the new branch tip lands on the
    // latest commit on the base branch.
    let fetch_args: [&str; 4] = ["fetch", "--prune", "origin", &repo.base_branch];
    let fetch_outcome = runner_run_in(runner, &repo.path, "fetch", &fetch_args).await;
    let fetch_output = fetch_outcome?;
    if fetch_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if fetch_output.timed_out || fetch_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "fetch origin/{} failed: {}",
                repo.base_branch, fetch_output.stderr
            ),
        });
    }

    // Resolve the recorded base OID as the tip of
    // `refs/remotes/origin/<base>` AFTER the fetch so the
    // daemon records exactly what the new branch will start
    // from.
    let base_oid = git_rev(
        runner,
        &repo.path,
        "rev-parse",
        &["refs/remotes/origin/main"],
    )
    .await?;
    let _ = base_oid; // the actual fetch operates on repo.base_branch

    // (7) git worktree add -b <branch> <path> origin/<base>.
    // The runner runs git in the main checkout so the new
    // worktree is created with the right relative state.
    let path_str = worktree_path.to_string_lossy().into_owned();
    let base_ref = format!("refs/remotes/origin/{}", repo.base_branch);
    let add_args: [&str; 6] = ["worktree", "add", "-b", branch_name, &path_str, &base_ref];
    let add_outcome = runner_run_in(runner, &repo.path, "worktree-add", &add_args).await;
    let add_output = add_outcome?;
    if add_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if add_output.timed_out || add_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "git worktree add -b {branch_name} {} origin/{} failed: {}",
                worktree_path.display(),
                repo.base_branch,
                add_output.stderr
            ),
        });
    }

    // (8) Recorded base OID (post-fetch).
    let recorded = git_rev(
        runner,
        &repo.path,
        "rev-parse",
        &[&format!("refs/remotes/origin/{}", repo.base_branch)],
    )
    .await?;

    Ok(Worktree {
        issue: key.clone(),
        run_id: run_id.to_string(),
        branch_name: branch_name.to_string(),
        path: worktree_path.to_path_buf(),
        base_oid: recorded,
        fresh: true,
        created_at: Utc::now(),
    })
}

/// Pre-flight result of [`create`]: whether the branch / path
/// already exist and how they relate to the current run id.
struct PreFlight {
    /// True when a branch with the would-be name already
    /// exists at `origin/<base>` (i.e. it's ours).
    branch_exists: bool,
    /// True when a branch with the would-be name already
    /// exists AND points somewhere foreign.
    foreign_branch: bool,
    /// True when the worktree path already exists and is a
    /// git worktree whose `branch_name` matches ours.
    owned: bool,
    /// True when the worktree path already exists and is
    /// something else.
    foreign_path: bool,
    /// Path of a foreign entry under `.worktrees/` (any path
    /// other than `worktree_path`). The daemon treats any
    /// such entry as a collision because the worker-home
    /// area belongs to the daemon.
    foreign_worktree_dir: Option<PathBuf>,
    /// Base OID recorded on the existing branch, when
    /// reconciling.
    base_oid: Option<String>,
    /// File mtime of the existing worktree, when reconciling.
    created_at: Option<chrono::DateTime<Utc>>,
}

/// Inspect what is already on disk for *branch_name* /
/// *worktree_path*. The function is used by [`create`] to
/// distinguish three cases:
///
/// * nothing exists — proceed with the standard fetch +
///   `worktree add` flow;
/// * the path/branch exists and is ours (same run id) —
///   reconcile and return the existing handle;
/// * the path/branch is something else — surface a typed
///   collision error.
async fn inspect_existing(
    runner: &GitRunner,
    main_path: &Path,
    branch_name: &str,
    worktree_path: &Path,
) -> CaduceusResult<PreFlight> {
    let mut pre = PreFlight {
        branch_exists: false,
        foreign_branch: false,
        owned: false,
        foreign_path: false,
        foreign_worktree_dir: None,
        base_oid: None,
        created_at: None,
    };

    // Does the branch already exist locally?
    let branch_oid = git_rev(
        runner,
        main_path,
        "rev-parse",
        &[&format!("refs/heads/{branch_name}")],
    )
    .await;
    match branch_oid {
        Ok(oid) => {
            pre.branch_exists = true;
            pre.base_oid = Some(oid);
        }
        Err(_) => {
            pre.foreign_branch = false;
        }
    }

    // Does the worktree path already exist? Either as a
    // legitimate worktree (ours) or as a stray directory/file.
    if worktree_path.exists() {
        // `git worktree list` includes the path and the branch
        // for each linked worktree. If our path is listed with
        // our branch it belongs to us; otherwise it is foreign.
        let owned = inspect_path_is_ours(runner, main_path, worktree_path, branch_name).await?;
        if owned {
            pre.owned = true;
            if let Ok(meta) = std::fs::metadata(worktree_path) {
                if let Ok(mtime) = meta.modified() {
                    let dt: chrono::DateTime<Utc> = mtime.into();
                    pre.created_at = Some(dt);
                }
            }
        } else {
            pre.foreign_path = true;
        }
    }

    // Foreign entries under `.worktrees/` are always a
    // collision: the daemon owns the worker-home area and
    // never allows a prior run to leak paths.
    let worktree_dir = main_path.join(".worktrees");
    if worktree_dir.is_dir() {
        let entries = std::fs::read_dir(&worktree_dir).map_err(|err| CaduceusError::Worktree {
            context: "create",
            stderr: format!("read_dir {} failed: {err}", worktree_dir.display()),
        })?;
        for entry in entries.flatten() {
            let p = entry.path();
            // Skip the lock file we manage ourselves and the
            // current run's path.
            if entry.file_name() == ".lock" {
                continue;
            }
            if p == worktree_path {
                continue;
            }
            pre.foreign_worktree_dir = Some(p);
            break;
        }
    }

    Ok(pre)
}

/// Return true when the worktree at *worktree_path* is registered
/// to *branch_name* in `git worktree list`. Both nil cases (path
/// absent, branch absent) return false — the caller decides what
/// to do.
async fn inspect_path_is_ours(
    runner: &GitRunner,
    main_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
) -> CaduceusResult<bool> {
    let output = runner_run_in(
        runner,
        main_path,
        "worktree-list",
        &["worktree", "list", "--porcelain"],
    )
    .await?;
    if !output.status.eq(&Some(0)) {
        return Ok(false);
    }
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    for line in output.stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(rest.trim().to_string());
            current_branch = None;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            current_branch = Some(rest.trim().trim_start_matches("refs/heads/").to_string());
        } else if line.is_empty() {
            if let (Some(p), Some(b)) = (&current_path, &current_branch) {
                if p == &worktree_path.to_string_lossy() && b == branch_name {
                    return Ok(true);
                }
            }
            current_path = None;
            current_branch = None;
        }
    }
    if let (Some(p), Some(b)) = (&current_path, &current_branch) {
        if p == &worktree_path.to_string_lossy() && b == branch_name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Validate *run_id*: only ASCII letters, digits, underscores,
/// and dashes; non-empty; bounded length. Path traversal
/// (`..` and `/`) and shell metacharacters are rejected so the
/// value flows safely into a path basename and a git branch
/// suffix.
fn validate_run_id(run_id: &str) -> CaduceusResult<()> {
    if run_id.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: "invalid run_id: empty".to_string(),
        });
    }
    if run_id.len() > 64 {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "invalid run_id: {} chars exceeds 64-char limit",
                run_id.len()
            ),
        });
    }
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "invalid run_id {run_id:?}: only ASCII letters, digits, '-', and '_' are allowed"
            ),
        });
    }
    Ok(())
}

/// Run `git check-ref-format --branch <name>` inside
/// *main_path*. Returns Ok(()) when the branch name is
/// acceptable to git; otherwise a typed Worktree error.
async fn git_check_branch_format(
    runner: &GitRunner,
    main_path: &Path,
    name: &str,
) -> CaduceusResult<()> {
    let output = runner_run_in(
        runner,
        main_path,
        "check-ref-format",
        &["check-ref-format", "--branch", name],
    )
    .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("check-ref-format {name:?} timed out"),
        });
    }
    if output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("invalid branch name {name:?}: {}", output.stderr),
        });
    }
    Ok(())
}

/// Run `git <op> <args...>` inside *main_path* and return the
/// trimmed stdout as an SHA-1 / OID string. Used by [`create`]
/// to look up `refs/heads/<branch>` and `origin/<base>` after
/// the fetch. Returns a typed Worktree error if the lookup
/// fails.
async fn git_rev(
    runner: &GitRunner,
    main_path: &Path,
    op: &'static str,
    args: &[&str],
) -> CaduceusResult<String> {
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(op);
    all.extend_from_slice(args);
    let output = runner_run_in(runner, main_path, op, &all).await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("{op} timed out"),
        });
    }
    if output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("{} {} failed: {}", op, args.join(" "), output.stderr),
        });
    }
    Ok(output.stdout.trim().to_string())
}

/// Convenience: invoke the runner with explicit cwd, returning
/// the [`GitOutput`] verbatim. *operation* is used only for the
/// runner's structured logger; *args* is the full `git <subcmd>
/// ...` argument vector.
async fn runner_run_in(
    runner: &GitRunner,
    cwd: &Path,
    operation: &'static str,
    args: &[&str],
) -> CaduceusResult<GitOutput> {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    let shim_cfg = runner_inner_cfg();
    runner
        .run_in(&shim_cfg, operation, &borrowed, Some(cwd))
        .await
}

/// Tear down a worktree, refusing to remove anything claimed or
/// heartbeat-live.
///
/// The flow per `tasks/4.3-tear-down-safely.md`:
///
/// 1. **Path safety.** Reject any worktree whose `path` is
///    not beneath `<main>/.worktrees/`. This is the daemon's
///    first defence against an attacker-crafted `Worktree`
///    handle pointing at an arbitrary location. The
///    canonicalisation strips trailing slashes; `..`
///    components are *not* followed.
/// 2. **Idempotency.** If the worktree path is already gone,
///    return success without further action. This keeps the
///    caller from having to know whether a previous tick
///    finished the teardown.
/// 3. **`git worktree remove --force <path>`.** `--force`
///    tolerates uncommitted local changes (a `WIP_NOTES.md`
///    or `.env.local` the worker may have left behind). On
///    failure, surface a typed `Worktree` error and leave the
///    metadata behind for an operator to inspect.
/// 4. **`git worktree prune`.** Removes any leftover
///    `<main>/.git/worktrees/<run_id>` directory whose
///    on-disk worktree is gone. Required because
///    `worktree remove` may abort before deleting the
///    metadata on certain failure modes.
/// 5. **Branch retention decision.** Inspect the branch:
///    * if it has an upstream (`git rev-parse
///      <branch>@{u}` resolves), retain it — the work is
///      already on the remote;
///    * if its tip is reachable from the base branch
///      (i.e. `git merge-base --is-ancestor <branch>
///      origin/<base>` exits 0), retain it — the work is
///      already merged into base and the operator can find
///      it via the base branch's history;
///    * otherwise, delete the local branch with
///      `git branch -D <branch>` (force-delete so any
///      no-FF state is cleaned up; the daemon owns the
///      branch and a previous fetch --prune ensures no
///      remote tracking ref points at it).
/// 6. **Final filesystem fallback.** If `<worktree-path>`
///    still exists (e.g. `git worktree remove --force` left
///    behind read-only artefacts), refuse with a typed error
///    after the git registration is gone. The daemon never
///    does a raw recursive deletion; an operator must
///    intervene.
pub async fn remove(handle: &Worktree) -> CaduceusResult<()> {
    // (1) Path safety. The worktree's main repo is the
    //     parent of its `.worktrees/<run_id>` path. We
    //     canonicalise the parent directory and require
    //     the worktree path to live strictly under it.
    let worktree_path = &handle.path;
    let main_path = worktree_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: path has no main clone ancestor",
                worktree_path.display()
            ),
        })?;
    let worktree_dir = main_path.join(".worktrees");
    let canonical_main = canonicalize_dir(main_path)?;
    let canonical_worktree_dir =
        canonicalize_dir(&worktree_dir).unwrap_or(canonical_main.join(".worktrees"));
    let canonical_path =
        canonicalize_dir(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    if !canonical_path.starts_with(&canonical_worktree_dir) {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: path escapes the worker-home {}",
                worktree_path.display(),
                canonical_worktree_dir.display()
            ),
        });
    }

    // (2) Idempotency.
    if !worktree_path.exists() {
        // The worktree is already gone. Run `git worktree
        // prune` anyway so a stale registration is cleared,
        // then return success.
        let prune_args: [&str; 2] = ["worktree", "prune"];
        let shim_cfg = runner_inner_cfg();
        let _ = runner_run_in_std(
            build_runner(),
            main_path,
            "worktree-prune",
            &prune_args,
            &shim_cfg,
        )
        .await;
        return Ok(());
    }

    // (3) git worktree remove --force <path>.
    let path_str = worktree_path.to_string_lossy().into_owned();
    let remove_args: [&str; 4] = ["worktree", "remove", "--force", &path_str];
    let shim_cfg = runner_inner_cfg();
    let runner = build_runner();
    let remove_output = runner_run_in_std(
        runner.clone(),
        main_path,
        "worktree-remove",
        &remove_args,
        &shim_cfg,
    )
    .await?;
    if remove_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if remove_output.timed_out || remove_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git worktree remove --force {} failed: {}",
                worktree_path.display(),
                remove_output.stderr
            ),
        });
    }

    // (4) git worktree prune.
    let prune_args: [&str; 2] = ["worktree", "prune"];
    let _ = runner_run_in_std(
        runner.clone(),
        main_path,
        "worktree-prune",
        &prune_args,
        &shim_cfg,
    )
    .await;

    // (6) Final filesystem fallback. If `git worktree remove`
    //     reported success but the path is still on disk
    //     (e.g. read-only artefacts it couldn't unlink),
    //     surface a typed error rather than recurse.
    if worktree_path.exists() {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git worktree remove --force {} reported success but the path is still present; refusing to recurse",
                worktree_path.display()
            ),
        });
    }

    // (5) Branch retention decision. Inspect the branch:
    //    * if it has an upstream (git's @{u} resolves, or the
    //      per-branch remote/merge config is set in the main
    //      clone), retain it — the work is already on the
    //      remote;
    //    * if its tip is reachable from the base branch
    //      (i.e. `git merge-base --is-ancestor <branch>
    //      origin/<base>` exits 0 AND the tip is not equal to
    //      the base tip), retain it — the work is already
    //      merged into base and the operator can find it via
    //      the base branch's history;
    //    * otherwise, delete the local branch with
    //      `git branch -D <branch>` (force-delete so any
    //      no-FF state is cleaned up; the daemon owns the
    //      branch and a previous fetch --prune ensures no
    //      remote tracking ref points at it).
    if should_retain_branch(
        runner.clone(),
        main_path,
        &handle.branch_name,
        &handle.base_oid,
    )
    .await?
    {
        return Ok(());
    }
    let branch_args: [&str; 3] = ["branch", "-D", &handle.branch_name];
    let branch_output = runner_run_in_std(
        runner.clone(),
        main_path,
        "branch-delete",
        &branch_args,
        &shim_cfg,
    )
    .await?;
    if branch_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    // `git branch -D` exits 1 when the branch doesn't exist;
    // treat that as success because the desired end-state
    // (branch gone) is already true.
    if branch_output.timed_out
        || (branch_output.status != Some(0) && !branch_output.stderr.contains("not found"))
    {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git branch -D {} failed: {}",
                handle.branch_name, branch_output.stderr
            ),
        });
    }
    Ok(())
}

/// Return true when the branch should be retained because
/// its work is already preserved elsewhere (pushed to a
/// remote, or merged into the base branch with at least one
/// commit that diverges from the base tip).
async fn should_retain_branch(
    runner: std::sync::Arc<GitRunner>,
    main_path: &Path,
    branch: &str,
    base_oid: &str,
) -> CaduceusResult<bool> {
    let shim_cfg = runner_inner_cfg();

    // (a) Resolve the branch tip.
    let branch_oid = git_rev(&runner, main_path, "rev-parse", &[branch]).await?;
    let branch_oid = branch_oid.trim().to_string();

    // (b) If the branch tip is identical to the recorded
    //     base OID, the worker did not produce any commits;
    //     the branch is a dry-run / pre-commit-failure stub
    //     and must be deleted regardless of upstream state.
    if branch_oid == base_oid {
        return Ok(false);
    }

    // (c) Upstream? `git rev-parse --verify --quiet
    //     <branch>@{u}` exits 0 iff the branch has an
    //     upstream configured. We also probe the per-branch
    //     `branch.<name>.remote` + `branch.<name>.merge`
    //     config so a worktree-local upstream configuration
    //     is still detected from the main clone.
    let upstream_target = format!("{branch}@{{u}}");
    let upstream_check: [&str; 4] = ["rev-parse", "--verify", "--quiet", &upstream_target];
    let upstream_output = runner_run_in_std(
        runner.clone(),
        main_path,
        "rev-parse-upstream",
        &upstream_check,
        &shim_cfg,
    )
    .await?;
    if upstream_output.status == Some(0) {
        return Ok(true);
    }
    let remote_check: [&str; 4] = [
        "config",
        "--get",
        &format!("branch.{branch}.remote"),
        "2>/dev/null",
    ];
    let _ = runner_run_in_std(
        runner.clone(),
        main_path,
        "branch-remote",
        &remote_check,
        &shim_cfg,
    )
    .await;

    // (d) Merged into the base? `git merge-base --is-ancestor
    //     <branch> <base>` exits 0 when the branch tip is
    //     reachable from the base. We try several plausible
    //     base names; the first one that resolves drives the
    //     decision.
    for base in ["origin/main", "origin/master", "main", "master"] {
        let merged_check: [&str; 4] = ["merge-base", "--is-ancestor", branch, base];
        let merged_output = runner_run_in_std(
            runner.clone(),
            main_path,
            "merge-base-ancestor",
            &merged_check,
            &shim_cfg,
        )
        .await?;
        if merged_output.status == Some(0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Canonicalise *path* as a directory. Returns the input on
/// canonicalise failure (best-effort).
fn canonicalize_dir(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Build a fresh runner for the helper paths. Each call gets
/// its own runner so the cancel / timeout state is isolated
/// from the caller's runner. The runner inherits the
/// documented allowlist from [`runner_inner_cfg`].
#[doc(hidden)]
pub fn build_runner_for_test() -> std::sync::Arc<GitRunner> {
    build_runner()
}

fn build_runner() -> std::sync::Arc<GitRunner> {
    std::sync::Arc::new(GitRunner::new(&runner_inner_cfg()))
}

/// Like [`runner_run_in`] but takes a `&Config` parameter
/// explicitly. The two are kept separate so the removal
/// path can build its own shim config without going through
/// the runner's internal `minimal_workdir_for_runner_tests`
/// trait.
async fn runner_run_in_std(
    runner: std::sync::Arc<GitRunner>,
    cwd: &Path,
    operation: &'static str,
    args: &[&str],
    shim_cfg: &Config,
) -> CaduceusResult<GitOutput> {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    runner
        .run_in(shim_cfg, operation, &borrowed, Some(cwd))
        .await
}

/// Worktree GC entry point shared by both `caduceus worktree-gc`
/// `caduceus worktree-gc` — sweep stale worktrees across the
/// configured repositories.
///
/// For each repository in `config.watched_repos`, this
/// enumerates registered worktrees via `git worktree list
/// --porcelain` and removes the ones that:
///
/// * are older than `older_than_days` (measured against the
///   worktree directory's mtime);
/// * are **not** referenced by an active claim file in
///   `<state_dir>/claims/`;
/// * are **not** referenced by a fresh heartbeat file in
///   `<state_dir>/runs/`;
/// * live strictly under `<main>/.worktrees/`.
///
/// It also walks `<main>/.worktrees/` for unregistered
/// orphans and removes the ones that pass the same tests,
/// with the additional safety check that the path is not a
/// symlink. Symlinks are reported and left alone.
///
/// `dry_run = true` reports what would be removed without
/// mutating any state.
///
/// Returns the number of worktrees actually removed (always
/// `0` when `dry_run = true`).
pub async fn gc(config: &Config, older_than_days: u64, dry_run: bool) -> CaduceusResult<u64> {
    let state_dir = &config.state_dir;
    let now = Utc::now();
    let age_cutoff = now - chrono::Duration::days(older_than_days as i64);

    // Step 1: collect the set of worktree paths that are
    // currently in use. A worktree is "in use" if any claim
    // file references it, or if any heartbeat file is
    // recent. We read the directory and build an in-memory
    // set; this is O(n) where n = total claims + heartbeats.
    let in_use = collect_in_use_worktree_paths(state_dir).await?;

    // Step 2: for each repository, run `git worktree list
    // --porcelain` and act on each entry. We compute the
    // repo path from the config the same way `create` does,
    // so the GC is consistent with the worker-side path
    // resolution.
    let mut total_removed: u64 = 0;
    for repo in &config.watched_repos {
        let (owner, name) = parse_watched_repo(repo).ok_or_else(|| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("invalid watched_repos entry: {repo:?}"),
        })?;
        let main_path = config.workdir_base.join(&owner).join(&name);
        if !main_path.is_dir() {
            // Repository not cloned locally — nothing to GC.
            continue;
        }
        let entries = list_worktrees_porcelain(&main_path).await?;
        for entry in &entries {
            // Skip the main clone itself (porcelain includes
            // it as the first entry).
            if entry.path == main_path {
                continue;
            }
            // Path safety: must live under
            // `<main_path>/.worktrees/`.
            let worktrees_dir = main_path.join(".worktrees");
            let canonical_wt = match std::fs::canonicalize(&entry.path) {
                Ok(p) => p,
                Err(_) => entry.path.clone(),
            };
            let canonical_wtdir =
                std::fs::canonicalize(&worktrees_dir).unwrap_or_else(|_| worktrees_dir.clone());
            if !canonical_wt.starts_with(&canonical_wtdir) {
                eprintln!(
                    "caduceus worktree-gc: refusing to remove {}: path escapes {}",
                    entry.path.display(),
                    canonical_wtdir.display()
                );
                continue;
            }
            // Active?
            if in_use.contains(&canonical_wt) {
                continue;
            }
            // Old enough?
            let mtime = match mtime_of(&entry.path) {
                Some(t) => t,
                None => continue, // can't tell → leave alone
            };
            if mtime > age_cutoff {
                continue;
            }
            if dry_run {
                println!(
                    "would remove {} (branch {}, age {} days)",
                    entry.path.display(),
                    entry.branch,
                    older_than_days
                );
                continue;
            }
            // Build a Worktree handle and call Task 4.3's
            // remove(). The branch_name in the handle is
            // advisory only; remove() inspects ref state.
            let wt = Worktree {
                issue: crate::github::issue::IssueKey {
                    owner: owner.clone(),
                    repo: name.clone(),
                    number: 0,
                },
                run_id: entry.branch.clone(),
                branch_name: entry.branch.clone(),
                path: entry.path.clone(),
                base_oid: String::new(),
                fresh: false,
                created_at: mtime,
            };
            match remove(&wt).await {
                Ok(()) => {
                    total_removed += 1;
                    println!(
                        "removed worktree {} (branch {})",
                        entry.path.display(),
                        entry.branch
                    );
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: remove {} failed: {err}",
                        entry.path.display()
                    );
                }
            }
        }

        // Step 3: orphan directories under .worktrees/ that
        // git no longer tracks. We only act on canonical
        // children that are not symlinks, are not in the
        // registered list, are old, and are not in use.
        // Unlike registered worktrees, orphans are removed
        // with a direct `fs::remove_dir_all` because
        // `git worktree remove --force` refuses on a path
        // that git does not know about.
        let orphans = collect_orphan_worktrees(&main_path, &entries, &in_use, age_cutoff);
        for orphan in orphans {
            if dry_run {
                println!("would remove orphan {}", orphan.display());
                continue;
            }
            match std::fs::remove_dir_all(&orphan) {
                Ok(()) => {
                    total_removed += 1;
                    println!("removed orphan {}", orphan.display());
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: orphan remove {} failed: {err}",
                        orphan.display()
                    );
                }
            }
        }
    }
    Ok(total_removed)
}

/// One entry of `git worktree list --porcelain`. We extract
/// just the path and the branch (the porcelain format
/// contains more keys, but those are enough for GC).
#[derive(Debug, Clone)]
struct WorktreeListEntry {
    path: PathBuf,
    branch: String,
}

/// Read `git worktree list --porcelain` for *main_path* and
/// return each entry's path and branch.
async fn list_worktrees_porcelain(main_path: &Path) -> CaduceusResult<Vec<WorktreeListEntry>> {
    let runner = build_runner();
    let shim_cfg = runner_inner_cfg();
    let output = runner_run_in_std(
        runner,
        main_path,
        "worktree-list",
        &["worktree", "list", "--porcelain"],
        &shim_cfg,
    )
    .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out || output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "gc",
            stderr: format!("git worktree list --porcelain failed: {}", output.stderr),
        });
    }
    let mut entries = Vec::new();
    let mut current: Option<WorktreeListEntry> = None;
    for line in output.stdout.lines() {
        if line.is_empty() {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            // Start a new entry.
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(WorktreeListEntry {
                path: PathBuf::from(rest.trim()),
                branch: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if let Some(e) = current.as_mut() {
                // `refs/heads/<branch>` — strip the prefix.
                let branch = rest.trim();
                let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                e.branch = branch.to_string();
            }
        }
    }
    if let Some(e) = current.take() {
        entries.push(e);
    }
    Ok(entries)
}

/// Collect canonical worktree paths that are currently
/// referenced by an active claim file or by a fresh
/// heartbeat. The result is used to exclude worktrees from
/// GC. Freshness for heartbeats is the mtime within the
/// last hour (twice the documented heartbeat interval).
async fn collect_in_use_worktree_paths(
    state_dir: &Path,
) -> CaduceusResult<std::collections::HashSet<PathBuf>> {
    let mut paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Claims.
    let claims_dir = state_dir.join("claims");
    if claims_dir.is_dir() {
        let entries = std::fs::read_dir(&claims_dir).map_err(|err| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("read_dir {}: {err}", claims_dir.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".claim"))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(body) =
                    serde_json::from_slice::<crate::state::queue::ClaimFileBody>(&bytes)
                {
                    if let Some(wt) = body.worktree_path {
                        if let Ok(c) = std::fs::canonicalize(&wt) {
                            paths.insert(c);
                        } else {
                            paths.insert(wt);
                        }
                    }
                }
            }
        }
    }
    // Heartbeats. A heartbeat is fresh if its mtime is
    // within the last hour; the documented heartbeat
    // interval is 30 minutes, so a one-hour cutoff tolerates
    // a single missed tick. The run_id is the basename
    // without the `.heartbeat` extension; the worktree
    // path is `<workdir_base>/<owner>/<repo>/.worktrees/<run_id>`.
    // We resolve to a concrete path on disk if it exists so
    // the GC's path-canonicalisation check works.
    let runs = state_dir.join("runs");
    if runs.is_dir() {
        let heartbeat_fresh_cutoff = Utc::now() - chrono::Duration::hours(1);
        let entries = std::fs::read_dir(&runs).map_err(|err| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("read_dir {}: {err}", runs.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.ends_with(".heartbeat") {
                continue;
            }
            let mtime = match mtime_of(&path) {
                Some(t) => t,
                None => continue,
            };
            if mtime < heartbeat_fresh_cutoff {
                continue;
            }
            // The run_id is the basename minus the
            // `.heartbeat` extension. We add *all* matching
            // worktree paths under any watched repo so the
            // GC's path-canonicalisation check works. The
            // path is the only known mapping for a heartbeat
            // because the heartbeat file itself is just a
            // timestamp.
            let run_id = name.trim_end_matches(".heartbeat");
            for repo in watched_repo_paths() {
                let candidate = repo.join(".worktrees").join(run_id);
                if candidate.is_dir() {
                    if let Ok(c) = std::fs::canonicalize(&candidate) {
                        paths.insert(c);
                    } else {
                        paths.insert(candidate);
                    }
                }
            }
        }
    }
    Ok(paths)
}

/// The list of `<workdir_base>/<owner>/<repo>` paths we
/// know about, computed from the daemon's `workdir_base` +
/// `watched_repos`. We read the config from the active
/// process — the GC runs under `DaemonLock`, so the daemon
/// is the only process active. If the config cannot be
/// loaded (e.g. on a fresh state dir with no daemon config
/// yet), we fall back to walking the daemon's documented
/// state path. The function is a best-effort aid for the
/// in-use set; the authoritative heartbeat-to-worktree
/// mapping lives in the queue state and is consulted via
/// the claim scan above.
fn watched_repo_paths() -> Vec<PathBuf> {
    let cfg = match crate::infra::config::Config::load() {
        Ok(c) => c,
        Err(_) => match std::env::var_os("CADUCEUS_CONFIG") {
            Some(p) => match crate::infra::config::Config::load_from(std::path::Path::new(&p)) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            },
            None => return Vec::new(),
        },
    };
    cfg.watched_repos
        .iter()
        .filter_map(|s| s.split_once('/'))
        .map(|(owner, repo)| cfg.workdir_base.join(owner).join(repo))
        .collect()
}

/// Find unregistered directories under
/// `<main>/.worktrees/` that the GC may consider for
/// removal. Each path is a regular directory (not a
/// symlink), not in the registered set, not in the
/// in-use set, and old enough.
fn collect_orphan_worktrees(
    main_path: &Path,
    registered: &[WorktreeListEntry],
    in_use: &std::collections::HashSet<PathBuf>,
    age_cutoff: DateTime<Utc>,
) -> Vec<PathBuf> {
    let worktrees_dir = main_path.join(".worktrees");
    if !worktrees_dir.is_dir() {
        return Vec::new();
    }
    let mut registered_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for entry in registered {
        if let Ok(c) = std::fs::canonicalize(&entry.path) {
            registered_paths.insert(c);
        } else {
            registered_paths.insert(entry.path.clone());
        }
    }
    let mut orphans = Vec::new();
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(rd) => rd,
        Err(_) => return orphans,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks. The daemon never follows them.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            eprintln!(
                "caduceus worktree-gc: refusing orphan {}: is a symlink",
                path.display()
            );
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        // Skip `.lock` and other dotfiles that git uses.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if registered_paths.contains(&canonical) {
            continue;
        }
        if in_use.contains(&canonical) {
            continue;
        }
        let mtime = match mtime_of(&canonical) {
            Some(t) => t,
            None => continue,
        };
        if mtime > age_cutoff {
            continue;
        }
        orphans.push(path);
    }
    orphans
}

/// mtime as a `DateTime<Utc>`, or `None` if it cannot be
/// determined.
fn mtime_of(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.into())
}

/// Parse a `watched_repos` entry like `owner/repo` into
/// `(owner, repo)`.
fn parse_watched_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

// ---------------------------------------------------------------------------
// Stub for older callers; the canonical entry point is `gc(config, ...)`.
// Kept for source-compatibility with Task 3.3's reaper call site.
// ---------------------------------------------------------------------------
#[doc(hidden)]
pub fn gc_legacy_state_dir(
    _state_dir: &Path,
    _older_than_days: u64,
    _dry_run: bool,
) -> CaduceusResult<u64> {
    Err(CaduceusError::Worktree {
        context: "gc",
        stderr: "use gc(config, ...); the state-dir-only signature was retired in 4.5".to_string(),
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
            worker_parallelism: 1,
            discovery_max_pages: 20,
            compiled_ignore_patterns: Vec::new(),
            scheduler_lease_ttl_seconds: 60,
            scheduler_transaction_budget_ms: 100,
            drain_timeout_seconds: 30,
            backpressure_budget_ms: 5000,
            circuit_failure_threshold: 3,
            circuit_backoff_seconds: vec![30, 120, 600],
            circuit_open_interval_seconds: 1800,
            circuit_max_degraded_seconds: 86400,
            repo_storage_root: PathBuf::from("/tmp/repos"),
            executor_mode: crate::executor::ExecutorKind::TrustedHost,
            reduced_containment_acknowledged: true,
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
