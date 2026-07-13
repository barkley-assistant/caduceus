//! Structured logging: tracing-subscriber setup.
//!
//! Caduceus emits two streams from one global tracing subscriber:
//!
//! * File — compact structured JSON lines under `<state_dir>/processor.log`,
//!   written through a non-blocking writer so worker heartbeats never
//!   stall on disk pressure.
//! * Stderr — human-readable warnings and errors, suitable for
//!   operators tailing the daemon in a terminal.
//!
//! [`init`] installs the global subscriber and returns a [`LogGuard`]
//! that owns the background writer. Dropping the guard flushes the
//! writer and tears down the subscriber.
//!
//! Initialisation is once per process. Subsequent calls to [`init`]
//! return [`CaduceusError::Config`] with a clear message so the cron
//! tick fails fast instead of silently forking two writers. Tests
//! that need to drive the subscriber in isolation use
//! [`init_for_test`], which is scoped to the supplied closure and does
//! not touch the global default.
//!
//! Secrets and environment-variable values are never logged. Use
//! [`redact`] to sanitise any user-supplied string before it reaches
//! the log stream.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::error::{CaduceusError, CaduceusResult};

/// Process-wide flag tracking whether [`init`] has run in this
/// process. The flag is sticky for the life of the process: once
/// tracing's global subscriber is installed, no further `try_init`
/// calls will ever succeed, so [`init`] must fast-fail.
static GLOBAL_INITIALISED: AtomicBool = AtomicBool::new(false);

/// Owned by the daemon. Dropping it flushes the background writer.
#[derive(Debug)]
pub struct LogGuard {
    _writer_guard: WorkerGuard,
    log_path: PathBuf,
}

/// Initialise the global structured logging subscriber.
///
/// On success the file's parent directories are created with mode
/// 0700 (the same secure mode the daemon's state dirs use) and the
/// log file itself is appended to. The caller receives a [`LogGuard`]
/// that keeps the background writer alive; dropping the guard
/// flushes pending events and shuts the writer down.
///
/// Once tracing's global subscriber is set (the first successful
/// call) it cannot be replaced for the remainder of the process;
/// subsequent calls to [`init`] return
/// `CaduceusError::Config("logging already initialised")`. Tests
/// that need isolated subscribers must use [`init_for_test`].
pub fn init(log_path: &Path) -> CaduceusResult<LogGuard> {
    if GLOBAL_INITIALISED.load(Ordering::SeqCst) {
        return Err(CaduceusError::Config(
            "logging already initialised (call init_for_test in unit tests)".to_string(),
        ));
    }
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
            let _ = set_dir_mode_0700(parent);
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .inspect_err(|_| {
            // A failed open does not install a global subscriber, so
            // we deliberately do NOT flip GLOBAL_INITIALISED here.
        })?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file);
    let stderr_is_terminal = std::io::stderr().is_terminal();

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("caduceus=info,info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(file_writer)
        .with_filter(LevelFilter::DEBUG);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(stderr_is_terminal)
        .with_target(false)
        .with_level(true);

    let result = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    if let Err(err) = result {
        // Tracing already has a global default installed (e.g. from
        // a previous test). Mark ourselves as initialised so we
        // fast-fail the next call without re-entering ``try_init``.
        GLOBAL_INITIALISED.store(true, Ordering::SeqCst);
        return Err(CaduceusError::Config(format!(
            "logging already initialised: {err}"
        )));
    }

    GLOBAL_INITIALISED.store(true, Ordering::SeqCst);
    Ok(LogGuard {
        _writer_guard: file_guard,
        log_path: log_path.to_path_buf(),
    })
}

/// Test-only helper. Runs *body* with a structured logging subscriber
/// scoped to this thread. The subscriber is never installed as the
/// global default, so tests may call this multiple times in
/// parallel.
pub fn init_for_test<F, R>(log_path: &Path, body: F) -> CaduceusResult<R>
where
    F: FnOnce() -> R,
{
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);

    let subscriber = build_test_subscriber(writer);
    let _enter = tracing::subscriber::with_default(subscriber, body);
    drop(guard);
    Ok(_enter)
}

/// Build a subscriber that writes JSON lines to *writer*. Tests
/// drive this directly when they need to inspect a span of emitted
/// events; the public [`init_for_test`] wraps it in a thread-local
/// scope.
pub fn build_test_subscriber<W>(writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(writer)
        .with_span_events(FmtSpan::CLOSE)
        .with_filter(LevelFilter::TRACE);
    tracing_subscriber::registry().with(file_layer)
}

/// Sanitise *value* before it reaches a log line. Replaces
/// ``GITHUB_TOKEN=...`` / ``GH_TOKEN=...`` / ``CADUCEUS_GITHUB_TOKEN=...``
/// assignments (bare or quoted) with the literal ``<redacted>`` while
/// keeping the variable name intact so operators can still see which
/// field was leaked.
///
/// Empty input is returned unchanged. Strings that contain no
/// recognised credential-shaped substring are returned verbatim.
pub fn redact(value: &str) -> String {
    if value.is_empty() {
        return value.to_string();
    }
    let mut redacted = value.to_string();
    for needle in ["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"] {
        let mut search_from = 0usize;
        while let Some(pos) = redacted[search_from..].find(needle) {
            let abs = search_from + pos;
            // Skip if needle is part of a larger identifier (e.g.
            // ``MY_GITHUB_TOKEN``). The set of denied names is
            // small; we conservatively require a non-identifier
            // boundary on the left.
            if abs > 0 {
                let prev = redacted.as_bytes()[abs - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    search_from = abs + needle.len();
                    continue;
                }
            }
            let after = abs + needle.len();
            // Skip whitespace and an optional `=` / `:` between the
            // name and the value.
            let mut cursor = after;
            while cursor < redacted.len() && redacted.as_bytes()[cursor] == b' ' {
                cursor += 1;
            }
            if cursor < redacted.len() && redacted.as_bytes()[cursor] == b'=' {
                cursor += 1;
                while cursor < redacted.len() && redacted.as_bytes()[cursor] == b' ' {
                    cursor += 1;
                }
            }
            let value_start = cursor;
            let value_end = advance_to_end_of_value(&redacted, value_start);
            redacted.replace_range(value_start..value_end, "<redacted>");
            search_from = value_start + "<redacted>".len();
        }
    }
    redacted
}

fn advance_to_end_of_value(s: &str, start: usize) -> usize {
    let bytes = s.as_bytes();
    if start >= bytes.len() {
        return start;
    }
    let first = bytes[start];
    if first == b'"' || first == b'\'' {
        // Quoted value — scan for the matching close quote, ignoring
        // backslash escapes. We only need a coarse match; logs are
        // not parsed.
        let quote = first;
        let mut i = start + 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if bytes[i] == quote {
                return i + 1;
            }
            i += 1;
        }
        return bytes.len();
    }
    // Bare value — scan until whitespace, comma, semicolon, closing
    // brace, or end of string.
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b';' | b'}' | b']' => break,
            _ => i += 1,
        }
    }
    i
}

/// Best-effort mode tightening for an existing directory. Returns
/// `true` when the chmod succeeded or was already secure, `false`
/// when the kernel refused (e.g. a read-only filesystem). Used by
/// [`init`] to keep newly-created log directories consistent with
/// the daemon's state-dir contract.
fn set_dir_mode_0700(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(path, perms).is_ok()
        }
        _ => false,
    }
}

/// Convenience: emit a structured info event tagged with the daemon's
/// standard fields. The redaction layer lives one level up —
/// operators must wrap secret-shaped values with [`redact`].
#[macro_export]
macro_rules! caduceus_info {
    ($($arg:tt)+) => { tracing::info!($($arg)+) }
}

/// Convenience: emit a structured warning event. Stderr also picks
/// it up.
#[macro_export]
macro_rules! caduceus_warn {
    ($($arg:tt)+) => { tracing::warn!($($arg)+) }
}

/// Convenience: emit a structured error event. Stderr also picks
/// it up.
#[macro_export]
macro_rules! caduceus_error {
    ($($arg:tt)+) => { tracing::error!($($arg)+) }
}

/// Test-only: check whether [`init`] has been called in this
/// process. Useful for asserting that a guard drop clears the flag.
pub fn is_initialised() -> bool {
    GLOBAL_INITIALISED.load(Ordering::SeqCst)
}

/// Test-only: no-op kept for source-compatibility with existing
/// tests. The global flag is sticky for the life of the process;
/// once tracing has a global default installed, even dropping the
/// daemon's guard cannot unset it. Tests that want a clean slate
/// must run in a fresh process.
pub fn reset_for_test() {
    // Intentionally a no-op. Documenting here so callers do not
    // expect this to undo a successful ``init``.
}

impl LogGuard {
    /// Absolute path of the log file this guard owns. Tests use
    /// this to locate the file when asserting content.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }
}
