//! Contract smoke tests for the locally built reference worker image
//! (task 7.1; spec WR-2/WR-3/WR-4).
//!
//! Builds `plugin-assets/worker-reference-image` with `docker build
//! --pull` once and runs the contract scenarios against the image: the
//! canonical-env helper, the result writer (honoring
//! `CADUCEUS_RESULT_PATH`, the default path, `--status failure`, and
//! unwritable targets), every certification probe, and the arbitrary
//! `--entrypoint` argv contract.
//!
//! The test skips with a printed notice when Docker is unavailable, so
//! hosts without a daemon still run the rest of the suite. Set
//! `CADUCEUS_SKIP_OCI_SMOKE=1` to skip explicitly (same pattern as
//! `CADUCEUS_SKIP_LIFECYCLE`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use caduceus::executor::sandbox_spec::CANONICAL_ENV_KEYS;
use caduceus::github::issue::IssueKey;
use caduceus::worker::parse_result_file;
use tempfile::TempDir;

const IMAGE_TAG: &str = "caduceus-worker-reference:smoke-test";
const CONTAINERFILE_DIR: &str = "plugin-assets/worker-reference-image";

/// Canonical environment every container run below receives, mirroring
/// what the OCI executor injects (`CANONICAL_ENV_KEYS`).
const CANONICAL_ENV: &[(&str, &str)] = &[
    ("CADUCEUS_RUN_ID", "smoke-run"),
    ("CADUCEUS_ISSUE_ID", "smoke-issue"),
    ("CADUCEUS_ISSUE_NUMBER", "1"),
    ("CADUCEUS_ISSUE_REPO", "owner/repo"),
    ("CADUCEUS_ISSUE_TITLE", "Smoke title"),
    ("CADUCEUS_ISSUE_BODY", "Smoke body"),
    ("CADUCEUS_ISSUE_LABELS_JSON", "[\"smoke\"]"),
    ("CADUCEUS_CONTEXT_JSON", "{}"),
    ("CADUCEUS_BRANCH_NAME", "main"),
    ("CADUCEUS_WORKTREE_PATH", "/workspace"),
    ("CADUCEUS_RESULT_PATH", "/output/worker-result.json"),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn skip_reason() -> Option<&'static str> {
    if std::env::var("CADUCEUS_SKIP_OCI_SMOKE").as_deref() == Ok("1") {
        return Some("CADUCEUS_SKIP_OCI_SMOKE=1");
    }
    let info = Command::new("docker").arg("info").output();
    match info {
        Ok(output) if output.status.success() => None,
        _ => Some("docker daemon is unavailable"),
    }
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .expect("run docker")
}

/// Assert the docker invocation succeeded; on failure print the full
/// stdout/stderr for diagnosis.
fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstatus: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Assert the docker invocation failed (non-zero container exit).
fn assert_failed(output: &Output, what: &str) {
    assert!(
        !output.status.success(),
        "{what} must fail but exited successfully:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("container stdout is UTF-8")
}

/// Base `docker run` args modeling the executor sandbox: read-only
/// rootfs, bounded /tmp tmpfs, /workspace and /output bind mounts, and
/// (for the restricted runs) no network.
fn run_args(workspace: &Path, output: &Path, network: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--read-only".into(),
        "--tmpfs".into(),
        "/tmp:size=64m".into(),
    ];
    if let Some(mode) = network {
        args.push("--network".into());
        args.push(mode.into());
    }
    args.push("-v".into());
    args.push(format!("{}:/workspace:rw", workspace.display()));
    args.push("-v".into());
    args.push(format!("{}:/output:rw", output.display()));
    args
}

/// Build the reference image once for the whole smoke run.
fn build_image() {
    let context = repo_root().join(CONTAINERFILE_DIR);
    let output = docker(&[
        "build",
        "--pull",
        "-f",
        &context.join("Containerfile").to_string_lossy(),
        "-t",
        IMAGE_TAG,
        &context.to_string_lossy(),
    ]);
    assert_ok(&output, "docker build of the reference image");
}

#[test]
fn reference_image_contract_smoke() {
    if let Some(reason) = skip_reason() {
        println!("skipped: {reason}");
        return;
    }

    build_image();

    let ws = TempDir::new().expect("workspace tempdir");
    let out = TempDir::new().expect("output tempdir");

    // ------------------------------------------------------------------
    // WR-2: contract helper reads the canonical CADUCEUS_* environment.
    // ------------------------------------------------------------------
    let names = run_container(
        ws.path(),
        out.path(),
        None,
        &["/usr/local/bin/caduceus-env.sh", "--names-only"],
    );
    assert_ok(&names, "caduceus-env.sh --names-only");
    let names_out = stdout(&names);
    let actual: BTreeSet<&str> = names_out.lines().collect();
    let expected: BTreeSet<&str> = CANONICAL_ENV_KEYS.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "--names-only must mirror CANONICAL_ENV_KEYS"
    );
    let mut sorted: Vec<&str> = CANONICAL_ENV_KEYS.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        names_out.lines().collect::<Vec<_>>(),
        sorted,
        "--names-only output must be sorted"
    );

    let pairs = run_container(
        ws.path(),
        out.path(),
        None,
        &["/usr/local/bin/caduceus-env.sh"],
    );
    assert_ok(&pairs, "caduceus-env.sh");
    let pairs_out = stdout(&pairs);
    let pair_lines: BTreeSet<&str> = pairs_out.lines().collect();
    assert_eq!(pair_lines.len(), CANONICAL_ENV_KEYS.len());
    assert!(pair_lines.contains("CADUCEUS_RUN_ID=smoke-run"));

    // A multi-line CADUCEUS_ISSUE_BODY (legitimate in real worker
    // runs, see src/worker/worker_contract.rs) must still print one
    // `NAME=VALUE` line per canonical variable, with the body's
    // newlines collapsed onto its own line.
    let multi_body = "First body line\nSecond body line\nThird body line";
    let multi = run_container_with_body(ws.path(), out.path(), multi_body);
    assert_ok(&multi, "caduceus-env.sh with a multi-line issue body");
    let multi_out = stdout(&multi);
    let multi_lines: Vec<&str> = multi_out.lines().collect();
    assert_eq!(
        multi_lines.len(),
        CANONICAL_ENV_KEYS.len(),
        "one output line per canonical variable despite a multi-line \
         body:\n{multi_out}"
    );
    let keys: BTreeSet<&str> = multi_lines
        .iter()
        .map(|line| line.split('=').next().expect("NAME=VALUE line"))
        .collect();
    let expected_keys: BTreeSet<&str> = CANONICAL_ENV_KEYS.iter().copied().collect();
    assert_eq!(
        keys, expected_keys,
        "every canonical key appears exactly once:\n{multi_out}"
    );
    assert!(
        multi_lines
            .contains(&"CADUCEUS_ISSUE_BODY=First body line Second body line Third body line"),
        "multi-line body collapsed onto its own line:\n{multi_out}"
    );

    // A missing canonical variable must exit non-zero and name it.
    let missing = run_container_missing_var(ws.path(), out.path());
    assert_failed(&missing, "caduceus-env.sh with a missing variable");
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("CADUCEUS_RESULT_PATH"),
        "missing-variable diagnostic must name the missing variable: {stderr}"
    );

    // ------------------------------------------------------------------
    // WR-3: result writer honors CADUCEUS_RESULT_PATH with a
    // schema-valid document (validated by the daemon's own parser).
    // ------------------------------------------------------------------
    let issue = IssueKey::parse("owner/repo#1").expect("valid issue key");

    let write = run_container(
        ws.path(),
        out.path(),
        None,
        &["/usr/local/bin/write-result.sh"],
    );
    assert_ok(&write, "write-result.sh honoring CADUCEUS_RESULT_PATH");
    let result_path = out.path().join("worker-result.json");
    assert!(
        result_path.exists(),
        "result document written to CADUCEUS_RESULT_PATH"
    );
    let parsed = parse_result_file(&result_path, &issue).expect("schema-valid success document");
    assert_eq!(parsed.status, caduceus::worker::WorkerStatus::Success);
    assert!(!parsed.summary.is_empty());
    assert!(!parsed.commit_message.is_empty());
    assert!(!parsed.pull_request_title.is_empty());

    // Default result path: CADUCEUS_RESULT_PATH unset ->
    // /output/worker-result.json.
    std::fs::remove_file(&result_path).expect("remove honored-path result");
    let default_write = run_container_default_path(ws.path(), out.path());
    assert_ok(&default_write, "write-result.sh default path");
    assert!(
        result_path.exists(),
        "default path written to /output/worker-result.json"
    );
    let parsed =
        parse_result_file(&result_path, &issue).expect("default-path document schema-valid");
    assert_eq!(parsed.status, caduceus::worker::WorkerStatus::Success);

    // --status failure stays schema-valid.
    let failure_write = run_container(
        ws.path(),
        out.path(),
        None,
        &["/usr/local/bin/write-result.sh", "--status", "failure"],
    );
    assert_ok(&failure_write, "write-result.sh --status failure");
    let parsed = parse_result_file(&result_path, &issue).expect("failure document schema-valid");
    assert_eq!(parsed.status, caduceus::worker::WorkerStatus::Failure);

    // Unwritable target: non-zero exit, no partial file.
    let ro = TempDir::new().expect("read-only output tempdir");
    let ro_write = run_container_with_result_path(
        ws.path(),
        ro.path(),
        None,
        &["/usr/local/bin/write-result.sh"],
        "/output/worker-result.json",
        ReadOnlyOutput::Yes,
    );
    assert_failed(&ro_write, "write-result.sh on an unwritable target");
    let leftovers: Vec<_> = std::fs::read_dir(ro.path())
        .expect("read read-only output dir")
        .collect();
    assert!(
        leftovers.is_empty(),
        "no partial file may remain on a failed write: {leftovers:?}"
    );

    // ------------------------------------------------------------------
    // WR-4: certification probes.
    // ------------------------------------------------------------------
    std::fs::write(ws.path().join("sentinel.txt"), "smoke-sentinel-42\n").expect("write sentinel");

    // Restricted sandbox (network none) — every probe passes.
    let sentinel = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "sentinel-read"],
    );
    assert_ok(&sentinel, "sentinel-read");
    assert!(
        stdout(&sentinel).contains("PASS sentinel-read:")
            && stdout(&sentinel).contains("smoke-sentinel-42"),
        "sentinel-read single-line pass report: {}",
        stdout(&sentinel)
    );

    let mount = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "mount-probe"],
    );
    assert_ok(&mount, "mount-probe");
    assert!(
        stdout(&mount).contains("PASS mount-probe:"),
        "mount-probe pass report: {}",
        stdout(&mount)
    );

    let hog = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "resource-hog", "1", "1024"],
    );
    assert_ok(&hog, "resource-hog");
    assert!(
        stdout(&hog).contains("PASS resource-hog:"),
        "resource-hog pass report: {}",
        stdout(&hog)
    );

    let net_none = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "network-probe", "none"],
    );
    assert_ok(&net_none, "network-probe none");
    assert!(
        stdout(&net_none).contains("PASS network-probe: mode=none"),
        "network-probe none pass report: {}",
        stdout(&net_none)
    );

    // Default network — auto reports reachability either way.
    let net_auto = run_container(
        ws.path(),
        out.path(),
        None,
        &["/usr/local/bin/worker-probe", "network-probe", "auto"],
    );
    assert_ok(&net_auto, "network-probe auto");
    assert!(
        stdout(&net_auto).contains("PASS network-probe: mode=auto"),
        "network-probe auto pass report: {}",
        stdout(&net_auto)
    );

    // Arbitrary --entrypoint argv replaces the image default command.
    let entrypoint = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--entrypoint",
            "/bin/sh",
            IMAGE_TAG,
            "-c",
            "echo arbitrary-argv-ok",
        ])
        .output()
        .expect("run arbitrary entrypoint");
    assert_ok(&entrypoint, "arbitrary --entrypoint argv");
    assert_eq!(stdout(&entrypoint).trim(), "arbitrary-argv-ok");

    // Probes stay reachable at fixed paths without the dispatcher.
    let direct = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/probes/sentinel-read.sh"],
    );
    assert_ok(&direct, "sentinel-read at its fixed path");

    // Probe failure paths exit non-zero with a diagnostic.
    std::fs::remove_file(ws.path().join("sentinel.txt")).expect("remove sentinel");
    let sentinel_missing = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "sentinel-read"],
    );
    assert_failed(&sentinel_missing, "sentinel-read with no sentinel");
    assert!(
        String::from_utf8_lossy(&sentinel_missing.stderr).contains("not found"),
        "sentinel-read diagnostic: {}",
        String::from_utf8_lossy(&sentinel_missing.stderr)
    );

    let net_unrestricted_under_none = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &[
            "/usr/local/bin/worker-probe",
            "network-probe",
            "unrestricted",
        ],
    );
    assert_failed(
        &net_unrestricted_under_none,
        "network-probe unrestricted under network none",
    );

    let hog_over_bound = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "resource-hog", "999", "1"],
    );
    assert_failed(&hog_over_bound, "resource-hog beyond its cpu bound");
    assert!(
        String::from_utf8_lossy(&hog_over_bound.stderr).contains("bound"),
        "resource-hog bound diagnostic: {}",
        String::from_utf8_lossy(&hog_over_bound.stderr)
    );

    let unknown_probe = run_container(
        ws.path(),
        out.path(),
        Some("none"),
        &["/usr/local/bin/worker-probe", "frobnicate"],
    );
    assert_failed(&unknown_probe, "worker-probe with an unknown subcommand");
}

// ---------------------------------------------------------------------------
// Container run helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ReadOnlyOutput {
    No,
    Yes,
}

/// `-e NAME=VALUE` args for the canonical environment, with
/// CADUCEUS_RESULT_PATH set to *result_path* (default the canonical
/// `/output/worker-result.json`).
fn canonical_env_args(result_path: &str) -> Vec<String> {
    let mut args = Vec::new();
    for (name, value) in CANONICAL_ENV {
        args.push("-e".to_string());
        if *name == "CADUCEUS_RESULT_PATH" {
            args.push(format!("{name}={result_path}"));
        } else {
            args.push(format!("{name}={value}"));
        }
    }
    args
}

/// `-e NAME=VALUE` args for every canonical variable except
/// CADUCEUS_RESULT_PATH (missing-variable and default-path scenarios).
fn canonical_env_args_without_result_path() -> Vec<String> {
    let mut args = Vec::new();
    for (name, value) in CANONICAL_ENV {
        if *name == "CADUCEUS_RESULT_PATH" {
            continue;
        }
        args.push("-e".to_string());
        args.push(format!("{name}={value}"));
    }
    args
}

/// `-e NAME=VALUE` args for every canonical variable, with
/// CADUCEUS_ISSUE_BODY replaced by *body* (and CADUCEUS_RESULT_PATH
/// set to *result_path*, default the canonical
/// `/output/worker-result.json`).
fn canonical_env_args_with_body(result_path: &str, body: &str) -> Vec<String> {
    let mut args = Vec::new();
    for (name, value) in CANONICAL_ENV {
        args.push("-e".to_string());
        let value = if *name == "CADUCEUS_ISSUE_BODY" {
            body
        } else if *name == "CADUCEUS_RESULT_PATH" {
            result_path
        } else {
            value
        };
        args.push(format!("{name}={value}"));
    }
    args
}

/// Run caduceus-env.sh (full mode) with a custom issue body.
fn run_container_with_body(workspace: &Path, output: &Path, body: &str) -> Output {
    let mut cmd = Command::new("docker");
    cmd.args(run_args(workspace, output, None));
    cmd.args(canonical_env_args_with_body(
        "/output/worker-result.json",
        body,
    ));
    cmd.arg(IMAGE_TAG).args(["/usr/local/bin/caduceus-env.sh"]);
    cmd.output().expect("run multi-line-body caduceus-env")
}

fn run_container(workspace: &Path, output: &Path, network: Option<&str>, argv: &[&str]) -> Output {
    run_container_inner(workspace, output, network, argv, None, ReadOnlyOutput::No)
}

fn run_container_default_path(workspace: &Path, output: &Path) -> Output {
    // Same as run_container but with CADUCEUS_RESULT_PATH unset.
    let mut cmd = Command::new("docker");
    cmd.args(run_args(workspace, output, None));
    cmd.args(canonical_env_args_without_result_path());
    cmd.arg(IMAGE_TAG).args(["/usr/local/bin/write-result.sh"]);
    cmd.output().expect("run default-path write-result")
}

fn run_container_missing_var(workspace: &Path, output: &Path) -> Output {
    // Every canonical variable except CADUCEUS_RESULT_PATH.
    let mut cmd = Command::new("docker");
    cmd.args(run_args(workspace, output, None));
    cmd.args(canonical_env_args_without_result_path());
    cmd.arg(IMAGE_TAG).args(["/usr/local/bin/caduceus-env.sh"]);
    cmd.output().expect("run missing-var caduceus-env")
}

fn run_container_with_result_path(
    workspace: &Path,
    output: &Path,
    network: Option<&str>,
    argv: &[&str],
    result_path: &str,
    read_only_output: ReadOnlyOutput,
) -> Output {
    run_container_inner(
        workspace,
        output,
        network,
        argv,
        Some(result_path),
        read_only_output,
    )
}

fn run_container_inner(
    workspace: &Path,
    output: &Path,
    network: Option<&str>,
    argv: &[&str],
    result_path: Option<&str>,
    read_only_output: ReadOnlyOutput,
) -> Output {
    let mut args = run_args(workspace, output, network);
    if matches!(read_only_output, ReadOnlyOutput::Yes) {
        // Rebuild the /output mount as read-only for the unwritable
        // target scenario; run_args used `:rw`.
        let index = args
            .iter()
            .position(|arg| arg == &format!("{}:/output:rw", output.display()))
            .expect("/output mount present");
        args[index] = format!("{}:/output:ro", output.display());
    }
    args.extend(canonical_env_args(
        result_path.unwrap_or("/output/worker-result.json"),
    ));
    let mut cmd = Command::new("docker");
    cmd.args(&args);
    cmd.arg(IMAGE_TAG);
    cmd.args(argv);
    cmd.output().expect("run container")
}
