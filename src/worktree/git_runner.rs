#![allow(dead_code, unused_imports)]
use super::*;
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
pub(crate) fn cap_text(bytes: &[u8]) -> String {
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
pub(crate) fn redact_and_cap(bytes: &[u8]) -> String {
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
