//! Bare `caduceus` prints help and touches no daemon state (issue #415).
//!
//! Regression: a no-argument invocation used to be rewritten to
//! `caduceus run` and execute a full tick. It must now print the clap
//! help to stdout and exit 0, with no config resolution, no logging
//! initialisation, and no state writes.
//!
//! Hermeticity: the config points `state_dir` at a scratch directory and
//! seeds `state_meta.json` with a future `next_allowed_poll_at`, so that
//! even a broken (tick-running) bare invocation would gate-skip before
//! any network I/O — the same recipe as `tests/cli/run_logging_test.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caduceus::meta::{StateMeta, TickOutcome, META_VERSION};
use chrono::Utc;

#[path = "../fixtures/mod.rs"]
mod fixtures;

use fixtures::tempdir;

/// Subcommand names clap renders in the top-level help. The assert
/// below keeps them in one place so a rename fails loudly here.
const SUBCOMMANDS: [&str; 8] = [
    "run",
    "status",
    "doctor",
    "worktree-gc",
    "queue",
    "review",
    "migrate-state",
    "setup",
];

/// Write a minimal valid config pointing `state_dir` at *dir* and the
/// worker at a noop script (YAML shape mirrors
/// `tests/cli/run_logging_test.rs` / `signal_test.rs`).
fn write_config(state_dir: &Path) -> PathBuf {
    let hermes_home = state_dir.join("hermes");
    fs::create_dir_all(&hermes_home).expect("create hermes home");
    let noop = state_dir.join("noop.py");
    fs::write(&noop, "#!/usr/bin/env python3\n").expect("write noop worker");
    let yaml = format!(
        "caduceus:\n  state_dir: \"{}\"\n  poll_interval_seconds: 3600\n  \
         watched_repos:\n    - \"owner/repo\"\n  worker_command:\n    - \"python3\"\n    - \"{}\"\n  \
         reduced_containment_acknowledged: true\n",
        state_dir.display(),
        noop.display()
    );
    let config_path = state_dir.join("config.yaml");
    fs::write(&config_path, yaml).expect("write config");
    config_path
}

/// Seed `state_meta.json` with a fresh `last_tick_finished` and a future
/// `next_allowed_poll_at` so a tick would take the cadence gate-skip
/// path (exit 0, no GitHub poll) rather than hitting the network.
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

/// Spawn the built `caduceus` binary with *args*. `RUST_LOG` is removed
/// so the default filter is deterministic regardless of the host.
fn run_binary(config: &Path, args: &[&str]) -> std::process::Output {
    let hermes_home = config.parent().expect("config has a parent").join("hermes");
    Command::new(env!("CARGO_BIN_EXE_caduceus"))
        .env("CADUCEUS_CONFIG", config)
        .env("HERMES_HOME", &hermes_home)
        .env_remove("RUST_LOG")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn caduceus")
}

/// Sorted names of every entry in the state directory.
fn state_entries(state_dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(state_dir)
        .expect("read state dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn bare_invocation_prints_help_and_exits_zero() {
    let dir = tempdir("bare-help");
    let config = write_config(&dir);
    seed_recent_tick(&dir);

    let before_entries = state_entries(&dir);
    let before_meta = fs::read(dir.join("state_meta.json")).expect("read seeded state_meta.json");

    let output = run_binary(&config, &[]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bare `caduceus` must exit 0 (help); got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status,
    );
    assert!(
        stdout.contains("Usage:"),
        "bare `caduceus` must print the clap help to stdout; got: {stdout:?}"
    );
    for name in SUBCOMMANDS {
        assert!(
            stdout.contains(name),
            "help output must list the `{name}` subcommand; got: {stdout:?}"
        );
    }
    assert!(
        stderr.is_empty(),
        "bare `caduceus` must write nothing to stderr; got: {stderr:?}"
    );

    // No tick evidence: the state directory gained no entries, the seeded
    // meta was not rewritten (a tick persists `last_tick_started` /
    // `last_tick_finished`), and the earliest tick artifact is absent.
    assert_eq!(
        state_entries(&dir),
        before_entries,
        "bare `caduceus` must not create state-dir entries"
    );
    assert_eq!(
        fs::read(dir.join("state_meta.json")).expect("read state_meta.json"),
        before_meta,
        "bare `caduceus` must not rewrite state_meta.json"
    );
    for artifact in [
        "processor.log",
        "state.json",
        "state.db",
        "queue",
        "claims",
        "repos",
        "runs",
    ] {
        assert!(
            !dir.join(artifact).exists(),
            "bare `caduceus` must not create `{artifact}` in the state dir"
        );
    }
}

#[test]
fn bare_invocation_help_shape_matches_help_flag() {
    let dir = tempdir("bare-help-shape");
    let config = write_config(&dir);

    let bare = run_binary(&config, &[]);
    let help = run_binary(&config, &["--help"]);

    for (label, output) in [("bare", &bare), ("--help", &help)] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "`caduceus {label}` must exit 0; got {:?}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        for name in SUBCOMMANDS {
            assert!(
                stdout.contains(name),
                "`{label}` help output must list the `{name}` subcommand; got: {stdout:?}"
            );
        }
    }
}

#[test]
fn explicit_run_still_takes_tick_path() {
    let dir = tempdir("bare-help-run");
    let config = write_config(&dir);
    seed_recent_tick(&dir);

    let output = run_binary(&config, &["run"]);
    assert!(
        output.status.success(),
        "`caduceus run` must exit 0 on a gate-skipped tick; got {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        dir.join("processor.log").is_file(),
        "`caduceus run` must still initialise logging (issue #386)",
    );
}

#[test]
fn setup_help_disambiguates_hermes_wrapper() {
    // Issue #417: `caduceus setup` (config generator) and
    // `hermes caduceus setup` (build + bridge seeding) share a word.
    // The binary's long help names the wrapper command; the
    // subcommand list keeps the one-line `about`.
    let dir = tempdir("setup-help");
    let config = write_config(&dir);

    let setup_help = run_binary(&config, &["setup", "--help"]);
    assert!(
        setup_help.status.success(),
        "`caduceus setup --help` must exit 0; got {:?}\nstderr: {}",
        setup_help.status,
        String::from_utf8_lossy(&setup_help.stderr),
    );
    let stdout = String::from_utf8_lossy(&setup_help.stdout);
    assert!(
        stdout.contains("Hermes-managed installs should use"),
        "setup --help must name the wrapper subcommand; got: {stdout:?}"
    );
    assert!(
        stdout.contains("hermes caduceus setup"),
        "setup --help must name `hermes caduceus setup`; got: {stdout:?}"
    );

    let top_help = run_binary(&config, &["--help"]);
    let top_stdout = String::from_utf8_lossy(&top_help.stdout);
    assert!(
        top_stdout.contains("Generate minimal non-secret configuration"),
        "the subcommand list must keep setup's one-line about; got: {top_stdout:?}"
    );
    assert!(
        !top_stdout.contains("Hermes-managed installs"),
        "the long about must not leak into the subcommand list; got: {top_stdout:?}"
    );
}
