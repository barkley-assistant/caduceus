//! `caduceus run` initialises file logging (issue #386).
//!
//! Regression: the CLI `run` handler — the entry path the cron pulse
//! wrapper, bare `caduceus`, and explicit `caduceus run` all take —
//! never called `logging::init`, so `<state_dir>/processor.log` was
//! never created and every tracing event (including the handler's own
//! git-identity warning) was silently dropped.
//!
//! Hermeticity: each test seeds `state_meta.json` so the cadence gate
//! returns a gate-skip before any network I/O — the same trick as
//! `tests/daemon/signal_test.rs::seed_recent_tick`. The spawned binary
//! must still create the log file, because `logging::init` runs before
//! the tick even starts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caduceus::meta::{StateMeta, TickOutcome, META_VERSION};
use chrono::Utc;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

/// Write a minimal valid config pointing `state_dir` at *dir* and the
/// worker at a noop script. `log_path` overrides the default
/// `<state_dir>/processor.log` when supplied (YAML shape mirrors
/// `review_cli_test.rs` / `signal_test.rs`).
fn write_config(state_dir: &Path, log_path: Option<&Path>) -> PathBuf {
    let hermes_home = state_dir.join("hermes");
    fs::create_dir_all(&hermes_home).expect("create hermes home");
    let noop = state_dir.join("noop.py");
    fs::write(&noop, "#!/usr/bin/env python3\n").expect("write noop worker");
    let mut yaml = format!(
        "caduceus:\n  state_dir: \"{}\"\n  poll_interval_seconds: 3600\n",
        state_dir.display()
    );
    if let Some(path) = log_path {
        yaml.push_str(&format!("  log_path: \"{}\"\n", path.display()));
    }
    yaml.push_str(&format!(
        "  watched_repos:\n    - \"owner/repo\"\n  worker_command:\n    - \"python3\"\n    - \"{}\"\n  reduced_containment_acknowledged: true\n",
        noop.display()
    ));
    let config_path = state_dir.join("config.yaml");
    fs::write(&config_path, yaml).expect("write config");
    config_path
}

/// Seed `state_meta.json` with a fresh `last_tick_finished` and a
/// future `next_allowed_poll_at` so the cadence gate's precheck fires
/// and the tick returns a gate-skip without polling GitHub (verbatim
/// shape of `signal_test.rs::seed_recent_tick`).
fn seed_recent_tick(state_dir: &Path) {
    let now = Utc::now();
    let meta = StateMeta {
        version: META_VERSION,
        last_tick_started: Some(now),
        last_tick_finished: Some(now),
        last_outcome: Some(TickOutcome::Processed),
        last_http_status: Some(200),
        next_allowed_poll_at: Some(now + chrono::Duration::seconds(3600)),
        last_reap_at: None,
        last_reaped_count: 0,
        rate_limit: None,
        last_error: None,
        recent_diagnostics: Vec::new(),
    };
    let body = serde_json::to_vec(&meta).expect("serialize meta");
    fs::write(state_dir.join("state_meta.json"), body).expect("write state_meta.json");
}

/// Run the built `caduceus` binary's `run` subcommand as a subprocess.
/// `RUST_LOG` is removed so the default `caduceus=info,info` filter is
/// deterministic regardless of the host environment.
fn run_binary(config: &Path) -> std::process::Output {
    let hermes_home = config.parent().expect("config has a parent").join("hermes");
    Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", config)
        .env("HERMES_HOME", &hermes_home)
        .env_remove("RUST_LOG")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn caduceus")
}

fn assert_exit_zero(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "expected exit 0 (gate-skipped tick); got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn run_creates_processor_log_in_active_state_dir() {
    let dir = tempdir("run-logging-default");
    let config = write_config(&dir, None);
    seed_recent_tick(&dir);

    let output = run_binary(&config);
    assert_exit_zero(&output);

    let log = dir.join("processor.log");
    assert!(
        log.is_file(),
        "`caduceus run` must create {} (issue #386); state_dir listing: {:?}",
        log.display(),
        fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
    );
    let body = fs::read_to_string(&log).expect("read processor.log");
    assert!(
        body.contains("tick skipped by gate"),
        "the gate-skip event must be flushed into the log before exit; got: {body}"
    );
}

#[test]
fn run_honours_configured_log_path_over_default() {
    let dir = tempdir("run-logging-override");
    let custom = dir.join("logs").join("daemon.log");
    let config = write_config(&dir, Some(&custom));
    seed_recent_tick(&dir);

    let output = run_binary(&config);
    assert_exit_zero(&output);

    assert!(
        custom.is_file(),
        "configured log_path {} must be created (issue #386)",
        custom.display(),
    );
    assert!(
        !dir.join("processor.log").exists(),
        "default processor.log must NOT be created when log_path is overridden",
    );
}
