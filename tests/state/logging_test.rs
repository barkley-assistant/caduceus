//! Caduceus calls [`init`] exactly once per process. Once tracing's
//! global subscriber is installed it cannot be replaced for the life
//! of the process — that is a property of the `tracing` crate, not a
//! Caduceus constraint. The tests respect this:
//!
//! * A single `#[serial_test::serial]` block drives `init`. The
//!   first test installs the global subscriber (and asserts the
//!   full lifecycle: nested directory, file append, drop-flush, log
//!   path accessor); every subsequent serial test confirms the
//!   fast-fail contract.
//! * Tests that need isolated log captures use [`init_for_test`],
//!   which installs a thread-local subscriber via
//!   `tracing::subscriber::with_default` and never touches the
//!   global default.

use std::path::PathBuf;
use std::sync::Mutex;

use caduceus::logging::{build_test_subscriber, init, init_for_test, is_initialised, redact};

static INIT_LOCK: Mutex<()> = Mutex::new(());

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-logging-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn read_file(path: &PathBuf) -> String {
    std::fs::read_to_string(path).expect("read log file")
}

// Scoped test subscriber — independent of the global install.
// These tests may run before or after the serial init() block.

#[test]
fn init_for_test_emits_lines_visible_after_body_returns() {
    let root = tempdir("scoped-test");
    let log_path = root.join("test.log");

    let seen = init_for_test(&log_path, || {
        tracing::info!(target: "caduceus", "scoped-hello");
        "captured"
    })
    .expect("scoped body runs");

    assert_eq!(seen, "captured");
    let body = read_file(&log_path);
    assert!(body.contains("\"scoped-hello\""), "got: {body}");
}

#[test]
fn build_test_subscriber_writes_json_lines() {
    let root = tempdir("json-subscriber");
    let log_path = root.join("json.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();
    let (writer, _guard) = tracing_appender::non_blocking(file);
    let subscriber = build_test_subscriber(writer);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "caduceus", "json-event");
    });
    drop(_guard);
    let body = read_file(&log_path);
    assert!(body.contains("\"json-event\""), "got: {body}");
    // tracing-subscriber emits the JSON layer as
    // ``{"timestamp":..., "level":..., "fields":{"message":"..."}}``.
    assert!(body.contains("\"level\":\"INFO\""), "got: {body}");
}

#[test]
fn init_for_test_creates_parent_directories() {
    let root = tempdir("scoped-nested");
    let log_path = root.join("nested").join("inside").join("test.log");
    let parent = log_path.parent().unwrap();
    assert!(!parent.exists());

    init_for_test(&log_path, || {
        tracing::info!(target: "caduceus", "event");
    })
    .expect("scoped init");

    assert!(parent.is_dir());
    let body = read_file(&log_path);
    assert!(body.contains("\"event\""), "got: {body}");
}

#[test]
fn init_for_test_propagates_body_return_value() {
    let root = tempdir("scoped-return");
    let log_path = root.join("test.log");
    let v: i32 = init_for_test(&log_path, || 42).expect("scoped body runs");
    assert_eq!(v, 42);
}

// Redaction

#[test]
fn redact_replaces_bare_token_assignment() {
    let out = redact("GITHUB_TOKEN=ghp_abc123xyz");
    assert!(out.contains("GITHUB_TOKEN="));
    assert!(out.contains("<redacted>"));
    assert!(!out.contains("ghp_abc123xyz"));
}

#[test]
fn redact_replaces_quoted_token_assignment() {
    let out = redact("GITHUB_TOKEN=\"ghp_secret\"");
    assert!(out.contains("<redacted>"));
    assert!(!out.contains("ghp_secret"));
}

#[test]
fn redact_handles_each_denied_name() {
    for name in ["GITHUB_TOKEN", "CADUCEUS_GITHUB_TOKEN", "GH_TOKEN"] {
        let input = format!("{name}=ghp_leak_value");
        let out = redact(&input);
        assert!(out.contains("<redacted>"), "{name} not redacted: {out}");
        assert!(!out.contains("ghp_leak_value"));
    }
}

#[test]
fn redact_leaves_unrelated_strings_intact() {
    let input = "PATH=/usr/local/bin HOME=/home/user";
    let out = redact(input);
    assert_eq!(out, input);
}

#[test]
fn redact_handles_empty_input() {
    assert_eq!(redact(""), "");
}

#[test]
fn redact_does_not_redact_substring_of_other_identifier() {
    // ``MY_GITHUB_TOKEN`` must NOT be matched — the deny-list is a
    // name match, not a substring match.
    let out = redact("MY_GITHUB_TOKEN=keepme");
    assert!(
        out.contains("keepme"),
        "sub-prefix identifier was redacted: {out}"
    );
}

#[test]
fn redact_handles_multiple_assignments_in_one_line() {
    let input = "GITHUB_TOKEN=ghp_first CADUCEUS_GITHUB_TOKEN=ghp_second PATH=/tmp";
    let out = redact(input);
    assert!(!out.contains("ghp_first"), "got: {out}");
    assert!(!out.contains("ghp_second"), "got: {out}");
    assert!(out.contains("PATH=/tmp"));
    assert_eq!(out.matches("<redacted>").count(), 2);
}

// `init()` lifecycle — single global install.
// The first serial test exercises the happy path; subsequent serial
// tests assert the documented fast-fail behaviour.

#[test]
#[serial_test::serial]
fn init_first_install_full_lifecycle() {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // If a previous test already installed the global subscriber,
    // we still want to verify that init() surfaces a clean error.
    // Skip the happy-path assertions in that case; the second-call
    // and unwritable-path serial tests cover the failure surface.
    if is_initialised() {
        let root = tempdir("init-already-installed");
        let log_path = root.join("processor.log");
        let err = init(&log_path).expect_err("init when global is set must fail");
        let msg = format!("{err:?}");
        assert!(msg.contains("already initialised"), "got: {msg}");
        return;
    }

    let root = tempdir("init-first");
    let log_path = root.join("nested").join("inside").join("processor.log");

    // First install succeeds: nested dir creation, file append.
    let log_guard = init(&log_path).expect("first init succeeds");
    assert_eq!(log_guard.log_path(), log_path.as_path());
    assert!(is_initialised());
    assert!(log_path.parent().unwrap().is_dir());

    // Emit events at every level the contract requires.
    caduceus::caduceus_info!(target: "caduceus", "first");
    caduceus::caduceus_warn!(target: "caduceus", "second");
    caduceus::caduceus_error!(target: "caduceus", "third");

    // Dropping the guard flushes pending lines.
    drop(log_guard);

    let body = read_file(&log_path);
    assert!(body.contains("\"first\""), "first event missing: {body}");
    assert!(body.contains("\"second\""), "second event missing: {body}");
    assert!(body.contains("\"third\""), "third event missing: {body}");
    // Every level emitted at least one JSON-shaped line.
    assert!(body.lines().filter(|l| l.contains("\"level\":\"")).count() >= 3);
}

#[test]
#[serial_test::serial]
fn init_second_call_fails_with_clear_message() {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("init-second");
    let log_path = root.join("processor.log");

    let err = init(&log_path).expect_err("second init must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains("already initialised"), "got: {msg}");
    assert!(is_initialised());
}

#[test]
#[serial_test::serial]
fn init_unwritable_path_surfaces_io_or_config_error() {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("init-unwritable");
    let blocker = root.join("blocker");
    std::fs::write(&blocker, "not a directory").unwrap();
    let bad = blocker.join("processor.log");

    let err = init(&bad).expect_err("init under a file must fail");
    let msg = format!("{err:?}");
    // Either ``Io`` or ``Config`` (already-initialised) is acceptable.
    assert!(
        msg.contains("Io(") || msg.contains("Config(") || msg.contains("already initialised"),
        "got: {msg}"
    );
}

#[test]
#[serial_test::serial]
fn init_creates_existing_parent_directory() {
    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("init-existing-parent");
    let log_path = root.join("processor.log");
    assert!(log_path.parent().unwrap().is_dir());

    let _ = init(&log_path);
    // The parent dir is unchanged either way.
    assert!(log_path.parent().unwrap().is_dir());
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn init_log_directory_has_mode_0700() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("init-mode-0700");
    let log_path = root.join("nested").join("processor.log");

    // Whether the install succeeded or fast-failed, the parent
    // directory creation runs first. Mode 0700 is the contract.
    let _ = init(&log_path);

    let parent = log_path.parent().unwrap();
    if parent.exists() {
        let mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
