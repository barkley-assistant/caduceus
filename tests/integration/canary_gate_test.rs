//! Unit + integration tests for the release-binary canary's
//! environmental skip-gate (`tests/fixtures/doctor_report.rs`).
//!
//! The canary must SKIP — not fail — when `hermes caduceus doctor`
//! cannot run green because the host is not configured for caduceus
//! (e.g. no provider secret in the shell), while still failing when
//! the doctor report shows a real `daemon-defect`. Truncating the
//! suite via cargo's default fail-fast on an environmental canary
//! failure hides the remaining ~45 test binaries from local
//! verification runs (the "green locally, red in CI" failure mode).
//!
//! The pure classifier in `fixtures::doctor_report` carries its own
//! inline unit tests; this binary adds real-process tests proving
//! the classifier against a fake `hermes` executable, mirroring how
//! `release_canary_test.rs` shells out to a `hermes` binary.

#[path = "../fixtures/mod.rs"]
mod fixtures;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use fixtures::{classify_doctor, DoctorVerdict};

/// Write an executable shell script and return its path.
fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    // Write + sync + close before chmod/exec so the fd is fully
    // released before another thread's Command::new execs the file
    // (Linux reports ETXTBSY otherwise).
    use std::io::Write as _;
    let mut f = fs::File::create(&path).expect("create script");
    f.write_all(body.as_bytes()).expect("write script");
    f.sync_all().expect("sync script");
    drop(f);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    path
}

/// Doctor report for a missing provider secret — the exact shape the
/// plugin's `_cli_doctor --verbose` prints for `config-incomplete`.
const REPORT_CONFIG_INCOMPLETE: &[&str] = &[
    "[OK] Binary — caduceus binary is built",
    "",
    "[FAIL] Provider Secret — no provider secret name configured (checked CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, GH_TOKEN)",
    "       next action: set one of CADUCEUS_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN in the environment",
    "       detail:      no provider secret name configured (checked …)",
    "       category:    config-incomplete",
];

/// Doctor report for a stale worktree lock — a real `daemon-defect`.
const REPORT_DAEMON_DEFECT: &[&str] = &[
    "[FAIL] Worktree Lock — stale .worktrees/.lock at /x/.lock (no Caduceus daemon holds the flock)",
    "       next action: remove /x/.lock with `rm` …",
    "       detail:      workdir_base=/x; stale_count=1 held_count=0",
    "       category:    daemon-defect",
];

/// Doctor report for an unreachable Hermes gateway — `gateway-inactive`,
/// doctor exit 2.
const REPORT_GATEWAY_INACTIVE: &[&str] = &[
    "[FAIL] Cron Capability — the Hermes gateway is not reachable",
    "       category:    gateway-inactive",
];

/// Build a fake `hermes` executable that prints `lines` (one per line)
/// to stdout and exits with `code` for the `doctor` subcommand.
fn fake_hermes(dir: &std::path::Path, lines: &[&str], code: i32) -> PathBuf {
    let mut script = String::from("#!/bin/sh\n");
    script.push_str("if [ \"$1\" = caduceus ] && [ \"$2\" = doctor ]; then\n");
    for line in lines {
        // Single-quoted heredoc-free echo; lines contain no single quotes.
        script.push_str(&format!("printf '%s\\n' '{line}'\n"));
    }
    script.push_str(&format!("exit {code}\n"));
    script.push_str("fi\nexit 0\n");
    write_script(dir, "hermes", &script)
}

/// Spawn the fake hermes with `caduceus doctor --verbose`, retrying
/// briefly on `ExecutableFileBusy`: under parallel test threads, Linux
/// can transiently report ETXTBSY when exec'ing a script written
/// moments earlier in another test's tempdir.
fn spawn_doctor(hermes: &std::path::Path) -> std::process::Output {
    let mut last_err = None;
    for _ in 0..5 {
        match Command::new(hermes)
            .args(["caduceus", "doctor", "--verbose"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(output) => return output,
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last_err = Some(err);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(err) => panic!("spawn fake hermes: {err}"),
        }
    }
    panic!(
        "spawn fake hermes: still ExecutableFileBusy after retries: {:?}",
        last_err
    );
}

/// Run the fake hermes with `caduceus doctor --verbose` and classify.
fn run_and_classify(hermes: &std::path::Path) -> DoctorVerdict {
    let output = spawn_doctor(hermes);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    classify_doctor(output.status.code().unwrap_or(-1), &stdout)
}

#[test]
fn fake_hermes_doctor_config_incomplete_skips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes = fake_hermes(dir.path(), REPORT_CONFIG_INCOMPLETE, 1);
    let verdict = run_and_classify(&hermes);
    assert!(
        matches!(verdict, DoctorVerdict::Skip(_)),
        "config-incomplete-only doctor must classify as Skip, got {verdict:?}"
    );
}

#[test]
fn fake_hermes_doctor_daemon_defect_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes = fake_hermes(dir.path(), REPORT_DAEMON_DEFECT, 1);
    let verdict = run_and_classify(&hermes);
    assert!(
        matches!(verdict, DoctorVerdict::Defect(_)),
        "daemon-defect doctor must classify as Defect, got {verdict:?}"
    );
}

#[test]
fn fake_hermes_doctor_host_capability_skips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes = fake_hermes(dir.path(), REPORT_GATEWAY_INACTIVE, 2);
    let verdict = run_and_classify(&hermes);
    assert!(
        matches!(verdict, DoctorVerdict::Skip(_)),
        "exit-2 gateway-inactive doctor must classify as Skip, got {verdict:?}"
    );
}

#[test]
fn fake_hermes_doctor_healthy_passes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes = fake_hermes(dir.path(), &["[OK] Binary — caduceus binary is built"], 0);
    let verdict = run_and_classify(&hermes);
    assert_eq!(
        verdict,
        DoctorVerdict::Healthy,
        "healthy doctor must classify as Healthy"
    );
}

#[test]
fn fake_hermes_doctor_unparsable_fails_safe() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Exit 1 with output on STDERR only — the classifier (stdout-only)
    // must fail safe rather than skip.
    let path = dir.path().join("hermes");
    fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = doctor ]; then\n  echo 'something went wrong' >&2\n  exit 1\nfi\nexit 0\n",
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let verdict = run_and_classify(&path);
    assert!(
        matches!(verdict, DoctorVerdict::Defect(_)),
        "exit 1 without a parsable stdout report must classify as Defect, got {verdict:?}"
    );
}

/// The classifier only reads stdout, so the report must arrive there —
/// pin the transport contract the way the real plugin prints it.
#[test]
fn fake_hermes_reports_arrive_on_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes = fake_hermes(dir.path(), REPORT_CONFIG_INCOMPLETE, 1);
    let output = spawn_doctor(&hermes);
    assert!(
        output.stderr.is_empty(),
        "report leaked to stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1));
}
