//! Gated live engine tests for the OCI sandbox isolation invariants
//! (design D8, issue #244).
//!
//! Every test in this file is gated behind the
//! `CADUCEUS_RUN_ISOLATION_TESTS` env var and is expected to be
//! **ignored** in CI without it:
//!
//! ```text
//! #[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
//! ```
//!
//! The tests drive a real Docker/Podman engine through the typed
//! pipeline (`resolve` → `render`) and assert in-container facts:
//!
//! - `oci_mount_enumeration_two_writable_surfaces` — the named
//!   mount-enumeration test consumed by task I12: the writable
//!   host-backed set is exactly `{/workspace, /output}`, the tmpfs
//!   set is bounded `{/tmp, /dev/shm}`, and the pseudo-filesystems
//!   are engine-managed kernel mounts rather than writable host
//!   binds.
//! - per-mode identity canaries (rootful Docker, rootless Docker,
//!   Podman `keep-id`).
//! - adversarial `.git` shadow read/write tests.
//! - the live adversarial certification cases (issue #252): host
//!   sentinel / daemon-state / other-repo unreachability, writable
//!   surface contract, capabilities + no-new-privileges, runtime
//!   socket + device absence, resource boundaries (memory OOM,
//!   pids EAGAIN, CPU throttle, tmpfs + /dev/shm bounds), network
//!   none + unrestricted-not-host, lifecycle timeout/cancellation
//!   cleanup, crash/restart orphan reconciliation, heartbeat
//!   liveness, wrong-digest rejection, and image neutrality.
//!
//! The CI `oci-live-certification` job (`.github/workflows/ci.yml`)
//! sets `CADUCEUS_RUN_ISOLATION_TESTS=1` and
//! `CADUCEUS_LIVE_TEST_IMAGE=<digest-pinned reference image>` and runs
//! this file via nextest `--run-ignored all`, making this suite the
//! merge-blocking live security gate for executor/security-relevant
//! changes (issue #252). The checklist→test mapping is asserted by
//! `tests/executor/certification_mapping_test.rs` and documented in
//! `docs/certification/oci-certification.md`.

use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caduceus::executor::engine_probe::{parse_engine_mode, GIT_SHADOW_FILE_CONTENT};
use caduceus::executor::oci_engine::OciImageAdapter;
use caduceus::executor::oci_image::acquire_image_with_adapter;
use caduceus::executor::oci_lifecycle::{run_oci_lifecycle, LifecycleTimeouts, OciAdapter};
use caduceus::executor::oci_platform::host_platform;
use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, EngineMode, GitShadowKind, SandboxEngine};
use caduceus::infra::config::{Config, OciPullPolicy, SandboxNetwork};
use caduceus::infra::error::CaduceusError;
use caduceus::readiness::{run_live_with_options, ProbeOptions, ReadinessVerdict};
use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Build the live fixture: a real worktree (with a `.git` artifact of
/// the requested kind), facts with the real worktree owner, and the
/// rendered `create` argv whose worker command dumps or mutates
/// state. The engine mode is detected from the real engine so the
/// per-mode canaries can skip (not fail) on a host running a
/// different mode.
struct LiveFixture {
    _tmp: TempDir,
    cfg: Config,
    argv: Vec<String>,
    engine: SandboxEngine,
    engine_mode: EngineMode,
    worktree: PathBuf,
    shadow_host: PathBuf,
    run_id: String,
}

fn engine_binary(engine: SandboxEngine) -> String {
    engine.binary_name().to_string()
}

/// Build an `ExecutorSpec` fixture mirroring the live runtime facts.
fn executor_spec_for(
    runtime: &caduceus::executor::sandbox_spec::RuntimeFacts,
) -> caduceus::executor::ExecutorSpec {
    caduceus::executor::ExecutorSpec {
        self_exe: PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: caduceus::github::issue::IssueKey::parse(&runtime.target)
                .expect("fixture target parses as issue key"),
            title: "Fix login bug".to_string(),
            body: "Steps to reproduce".to_string(),
            labels: vec!["bug".to_string()],
            branch_name: "caduceus/owner/repo#1".to_string(),
        }),
        worktree: runtime.worktree.clone(),
        run_id: runtime.run_id.clone(),
        context_json: "{}".to_string(),
        worker_command: runtime.worker_command.clone(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    }
}

/// The host's actual engine and mode. Returns `None` when no engine
/// binary is usable — the caller skips with a notice. The
/// `CADUCEUS_LIVE_TEST_ENGINE` env var (docker|podman) forces a
/// specific engine so the nightly Podman leg runs against Podman even
/// on runners that also have Docker.
fn detect_engine() -> Option<(SandboxEngine, EngineMode)> {
    let forced = std::env::var("CADUCEUS_LIVE_TEST_ENGINE").ok();
    let order: &[SandboxEngine] = match forced.as_deref() {
        Some("podman") => &[SandboxEngine::Podman],
        Some("docker") => &[SandboxEngine::Docker],
        _ => &[SandboxEngine::Docker, SandboxEngine::Podman],
    };
    for &engine in order {
        let bin = engine_binary(engine);
        let format_arg = match engine {
            SandboxEngine::Docker => "{{.SecurityOptions}}",
            SandboxEngine::Podman => "{{.Host.Security.Rootless}}",
        };
        let out = Command::new(&bin)
            .args(["info", "--format", format_arg])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                if let Ok((mode, _remap)) = parse_engine_mode(engine, &stdout) {
                    return Some((engine, mode));
                }
            }
        }
    }
    None
}

fn make_worktree(worktree: &Path, kind: GitShadowKind) {
    std::fs::create_dir_all(worktree).expect("create worktree");
    match kind {
        GitShadowKind::File => {
            let mut f = std::fs::File::create(worktree.join(".git")).expect("create .git file");
            f.write_all(b"gitdir: /real/main/.git/worktrees/leaky-pointer\n")
                .expect("write .git pointer");
        }
        GitShadowKind::Dir => {
            std::fs::create_dir_all(worktree.join(".git")).expect("create .git dir");
            std::fs::write(worktree.join(".git").join("HEAD"), "ref: refs/heads/main\n")
                .expect("write .git/HEAD");
        }
        GitShadowKind::Absent => {}
    }
}

/// Monotonic run-id counter: every fixture gets a distinct container
/// name so nextest's parallel execution never collides (nextest runs
/// each test in its own process, so the counter is combined with the
/// process id).
static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_run_id() -> String {
    format!(
        "live-iso-{}-{}",
        std::process::id(),
        RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn live_fixture(kind: GitShadowKind, container_script: &str) -> LiveFixture {
    live_fixture_with(kind, container_script, |_| {})
}

/// [`live_fixture`] with a config adjustment hook (network mode,
/// timeouts, …) applied before resolution.
fn live_fixture_with(
    kind: GitShadowKind,
    container_script: &str,
    adjust: impl FnOnce(&mut Config),
) -> LiveFixture {
    let (engine, engine_mode) =
        detect_engine().expect("no usable Docker/Podman engine for gated live tests");
    // Root the fixture on the filesystem the test process runs from
    // (the crate root, normally ext4). A tmpfs-backed /tmp would make
    // the bind mounts' /proc/mounts fstype `tmpfs` and defeat the
    // host-backed classification below.
    let tmp = TempDir::new_in(".").expect("tempdir under crate root");
    let mut cfg = Config::test_defaults(tmp.path());
    // Digest-pinned image override for hosts with a pre-pulled image;
    // defaults to the test_defaults placeholder (fine for pure argv
    // assertions, but a live `create` needs a resolvable digest).
    // `CADUCEUS_LIVE_TEST_IMAGE` is the global reference every live
    // test runs against. Only the image-neutrality certification test
    // (`image_neutrality_custom_unrelated_image_live`) overrides its
    // own sandbox image with `CADUCEUS_LIVE_NEUTRALITY_IMAGE` via the
    // `adjust` hook below.
    if let Ok(image) = std::env::var("CADUCEUS_LIVE_TEST_IMAGE") {
        assert!(
            image.contains("@sha256:"),
            "CADUCEUS_LIVE_TEST_IMAGE must be digest-pinned, got: {image}"
        );
        cfg.sandbox.as_mut().expect("sandbox").image = image;
    }
    adjust(&mut cfg);
    // The daemon creates its state roots owner-only; the readiness gate
    // rejects group/other-writable state dirs, so the fixture pins the
    // modes explicitly (the sandbox umask may not produce 0700).
    for root in [&cfg.state_dir, &cfg.repo_storage_root, &cfg.workdir_base] {
        std::fs::create_dir_all(root).expect("create state root");
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
            .expect("chmod state root owner-only");
    }
    let run_id = next_run_id();
    let worktree = cfg.workdir_base.join("owner").join("repo").join(&run_id);
    make_worktree(&worktree, kind);

    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: run_id.to_string(),
        target: "owner/repo#1".to_string(),
        worker_command: vec![
            "sh".to_string(),
            "-c".to_string(),
            container_script.to_string(),
        ],
        worktree: worktree.clone(),
        output_dir: cfg.state_dir.join("oci-runs").join(&run_id).join("output"),
        daemon_id: "live-test-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: std::fs::metadata(&worktree).expect("worktree stat").uid(),
        worktree_gid: std::fs::metadata(&worktree).expect("worktree stat").gid(),
        engine_mode,
        git_shadow_kind: kind,
        git_shadow_host: cfg
            .state_dir
            .join("oci-runs")
            .join(&run_id)
            .join("git-shadow"),
    };
    // Emulate the daemon-owned shadow artifact the pre-flight would
    // create (the tests drive the engine directly, not the executor).
    std::fs::create_dir_all(
        runtime
            .git_shadow_host
            .parent()
            .expect("shadow host has a parent"),
    )
    .expect("create run dir");
    if kind != GitShadowKind::Absent {
        if matches!(kind, GitShadowKind::File) {
            std::fs::write(&runtime.git_shadow_host, GIT_SHADOW_FILE_CONTENT)
                .expect("create shadow file");
        } else {
            std::fs::create_dir_all(&runtime.git_shadow_host).expect("create shadow dir");
        }
    }
    std::fs::create_dir_all(&runtime.output_dir).expect("create output dir");

    let spec = resolve(cfg.sandbox(), &runtime, &executor_spec_for(&runtime))
        .expect("live facts must resolve");
    let argv = render(&spec, engine);
    LiveFixture {
        _tmp: tmp,
        cfg,
        argv,
        engine,
        engine_mode,
        worktree,
        shadow_host: runtime.git_shadow_host.clone(),
        run_id: run_id.to_string(),
    }
}

fn run_or_die(cmd: &mut Command, what: &str) -> std::process::Output {
    let out = cmd.output().expect(what);
    if !out.status.success() {
        panic!(
            "{what} failed: status {:?}\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    out
}

/// `create` → `start` → `wait` → (`logs`) → `rm -f`. Returns
/// `(exit_code, combined_logs)`.
fn run_container(fx: &LiveFixture) -> (i64, String) {
    let bin = engine_binary(fx.engine);
    // `fx.argv` already begins with the engine binary + `create`; skip
    // the argv's own binary token when spawning.
    run_or_die(Command::new(&bin).args(&fx.argv[1..]), "engine create");
    run_or_die(
        Command::new(&bin).args(["start", &fx.run_id]),
        "engine start",
    );
    let wait = run_or_die(Command::new(&bin).args(["wait", &fx.run_id]), "engine wait");
    // `docker wait` can race the daemon's OOM cleanup: an OOM-killed
    // container may already be marked "removing"/"dead", in which case
    // `wait` reports 0 while the recorded state still carries the real
    // exit code. `docker inspect` is the authoritative source, so the
    // exit code is read back from there whenever possible.
    let mut code: i64 = String::from_utf8_lossy(&wait.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    let inspected = Command::new(&bin)
        .args([
            "inspect",
            "--format",
            "{{.State.ExitCode}} {{.State.OOMKilled}} {{.State.Status}}",
            &fx.run_id,
        ])
        .output();
    if let Ok(out) = inspected {
        // The format string emits three space-separated fields
        // (`ExitCode OOMKilled Status`); only the first is the exit
        // code, so split before parsing.
        if let Some(first) = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
        {
            if let Ok(recorded) = first.parse::<i64>() {
                if recorded != 0 || code == 0 {
                    code = recorded;
                }
            }
        }
    }
    let logs = Command::new(&bin)
        .args(["logs", &fx.run_id])
        .output()
        .expect("engine logs");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    // Best-effort teardown.
    let _ = Command::new(&bin).args(["rm", "-f", &fx.run_id]).output();
    (code, combined)
}

fn wait_for(predicate: impl Fn() -> bool, budget: Duration, what: &str) -> bool {
    let started = Instant::now();
    while started.elapsed() < budget {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = what;
    false
}

// ---------------------------------------------------------------------------
// Named mount-enumeration test (consumed by task I12)
// ---------------------------------------------------------------------------

/// Exercise the same live readiness gate used by dispatch against a real
/// engine and the configured digest-pinned image.
#[tokio::test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
async fn oci_readiness_gate_accepts_live_engine_and_image() {
    if std::env::var("CADUCEUS_LIVE_TEST_IMAGE").is_err() {
        println!("skipped: set CADUCEUS_LIVE_TEST_IMAGE to a digest-pinned image");
        return;
    }
    let fx = live_fixture(GitShadowKind::Absent, "true");
    for path in [
        &fx.cfg.state_dir,
        &fx.cfg.repo_storage_root,
        &fx.cfg.workdir_base,
    ] {
        std::fs::create_dir_all(path).expect("create readiness root");
    }
    let mut cfg = fx.cfg.clone();
    cfg.sandbox.as_mut().expect("sandbox").reserved_host_disk_mb = 0;
    let report = run_live_with_options(
        &cfg,
        &ProbeOptions {
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            engine_binary: Some(PathBuf::from(engine_binary(fx.engine))),
        },
    )
    .await;
    assert_eq!(report.verdict, ReadinessVerdict::Ready, "{report:?}");
    assert!(report.verified_image.is_some());
}

/// I12 consumption point: run a real container whose command dumps
/// `/proc/mounts`; assert the writable host-backed set ==
/// `{/workspace, /output}` (the engine's own `/etc` metadata binds
/// excluded), the tmpfs set == bounded `{/tmp, /dev/shm}`, and the
/// pseudo-filesystems (`/proc`, `/sys`, `/dev`) are engine-managed
/// kernel mounts, not writable host-backed binds.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn oci_mount_enumeration_two_writable_surfaces() {
    let fx = live_fixture(
        GitShadowKind::File,
        "cat /proc/mounts; id -u > /workspace/canary",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");

    // Parse the /proc/mounts dump printed by the container.
    let mut mounts: Vec<(String, String, String)> = Vec::new(); // (target, fstype, opts)
    for line in logs.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 4 && fields[1].starts_with('/') {
            mounts.push((
                fields[1].to_string(),
                fields[2].to_string(),
                fields[3].to_string(),
            ));
        }
    }
    assert!(
        !mounts.is_empty(),
        "expected a /proc/mounts dump, got: {logs}"
    );

    // Pseudo/kernel filesystems are never host-backed bind surfaces.
    // A `size=`-bearing tmpfs at a daemon-declared target is a bounded
    // tmpfs; size-less or masked-path tmpfs entries are engine-managed
    // ephemera (this fixture roots the workdir on the crate
    // filesystem, so a bind never reports fstype tmpfs here).
    const KERNEL_FS: &[&str] = &[
        "proc", "sysfs", "devtmpfs", "devpts", "mqueue", "cgroup", "cgroup2", "shm", "nsfs",
    ];
    let is_kernel_fs = |fs: &str| KERNEL_FS.contains(&fs);

    // Daemon-declared bounded tmpfs: /tmp and /dev/shm are tmpfs, rw,
    // and sized to the configured `tmpfs_mb`/`shm_mb` bounds (engines
    // print the size in k or b units).
    let size_bytes = |opts: &str| -> Option<u64> {
        for opt in opts.split(',') {
            if let Some(raw) = opt.strip_prefix("size=") {
                if let Some(kb) = raw.strip_suffix('k') {
                    return kb.parse::<u64>().ok().map(|v| v * 1024);
                }
                if let Some(mb) = raw.strip_suffix('m') {
                    return mb.parse::<u64>().ok().map(|v| v * 1024 * 1024);
                }
                if let Some(b) = raw.strip_suffix('b') {
                    return b.parse::<u64>().ok();
                }
            }
        }
        None
    };
    let tmpfs_mb = fx.cfg.sandbox().resources.tmpfs_mb;
    let shm_mb = fx.cfg.sandbox().resources.shm_mb;
    for (target, want_mb) in [("/tmp", tmpfs_mb), ("/dev/shm", shm_mb)] {
        let entry = mounts
            .iter()
            .find(|(t, _, _)| t == target)
            .unwrap_or_else(|| panic!("{target} must be mounted; dump: {logs}"));
        assert_eq!(entry.1, "tmpfs", "{target} must be tmpfs; dump: {logs}");
        assert!(
            entry.2.split(',').any(|o| o == "rw"),
            "{target} must be writable; dump: {logs}"
        );
        assert_eq!(
            size_bytes(&entry.2),
            Some(want_mb * 1024 * 1024),
            "{target} tmpfs bound must equal the configured {want_mb}m; dump: {logs}"
        );
    }
    // No other top-level tmpfs target exists beyond the daemon-declared
    // pair and the engine's own /dev.
    let other_top_level_tmpfs: Vec<&str> = mounts
        .iter()
        .filter(|(t, fs, _)| fs == "tmpfs" && t.matches('/').count() == 1)
        .map(|(t, _, _)| t.as_str())
        .filter(|t| *t != "/tmp" && *t != "/dev/shm" && *t != "/dev")
        .collect();
    assert!(
        other_top_level_tmpfs.is_empty(),
        "unexpected top-level tmpfs targets: {other_top_level_tmpfs:?}; dump: {logs}"
    );

    // Writable host-backed set: a real host filesystem (not kernel
    // fs, not tmpfs) mounted rw. The engine's own
    // `/etc/{hosts,hostname,resolv.conf}` metadata binds are excluded
    // — they are engine-managed, not daemon-declared surfaces.
    let is_host_fs =
        |fs: &str| fs == "ext4" || fs == "xfs" || fs == "btrfs" || fs == "zfs" || fs == "overlay";
    let mut writable: Vec<String> = mounts
        .iter()
        .filter(|(target, fs, opts)| {
            is_host_fs(fs) && opts.split(',').any(|o| o == "rw") && !target.starts_with("/etc/")
        })
        .map(|(target, _, _)| target.clone())
        .collect();
    writable.sort();
    writable.dedup();
    assert_eq!(
        writable,
        vec!["/output".to_string(), "/workspace".to_string()],
        "writable host-backed set must be exactly {{/workspace, /output}}; \
         full dump: {logs}"
    );

    // The `.git` shadow must be a read-only host-backed mount.
    let shadow_entry = mounts
        .iter()
        .find(|(target, _, _)| target == "/workspace/.git")
        .unwrap_or_else(|| panic!("/workspace/.git must be mounted; dump: {logs}"));
    assert!(
        !shadow_entry.2.split(',').any(|o| o == "rw"),
        "/workspace/.git must never be writable; entry: {shadow_entry:?}"
    );

    // Pseudo-filesystems present and kernel-managed (never writable
    // host binds).
    for (pseudo, expected_fs) in [("/proc", "proc"), ("/sys", "sysfs")] {
        let entry = mounts
            .iter()
            .find(|(target, _, _)| target == pseudo)
            .unwrap_or_else(|| panic!("{pseudo} must be mounted; dump: {logs}"));
        assert_eq!(
            entry.1, expected_fs,
            "{pseudo} must be the kernel pseudo-filesystem; dump: {logs}"
        );
    }
    let dev = mounts
        .iter()
        .find(|(target, _, _)| target == "/dev")
        .unwrap_or_else(|| panic!("/dev must be mounted; dump: {logs}"));
    assert!(
        is_kernel_fs(&dev.1) || dev.1 == "tmpfs",
        "/dev must be a kernel-managed mount (devtmpfs or the engine's \
         mode-755 tmpfs), got fstype {}; dump: {logs}",
        dev.1
    );
}

// ---------------------------------------------------------------------------
// Per-mode identity canaries
// ---------------------------------------------------------------------------

/// Rootful Docker: the worker runs as `--user <owner-uid>:<gid>`, so
/// a file the container writes into `/workspace` appears host-side
/// owned by the worktree owner, and host finalize (commit/push from
/// the same worktree) would succeed. Skips with a notice when the
/// host engine is not rootful Docker.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn rootful_docker_identity_canary() {
    let fx = live_fixture(
        GitShadowKind::File,
        "id -u > /workspace/canary && id -g >> /workspace/canary",
    );
    if fx.engine != SandboxEngine::Docker || fx.engine_mode != EngineMode::Rootful {
        eprintln!("skipping: host engine is not rootful Docker");
        return;
    }
    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");

    let canary = fx.worktree.join("canary");
    assert!(
        wait_for(|| canary.is_file(), Duration::from_secs(5), "canary"),
        "container must have written the canary into /workspace"
    );
    let meta = std::fs::metadata(&canary).expect("canary stat");
    assert_eq!(
        meta.uid(),
        std::fs::metadata(&fx.worktree)
            .expect("worktree stat")
            .uid(),
        "host-side canary owner must equal the worktree owner"
    );
    // Host finalize would run from this worktree: prove git is usable
    // on files the worker wrote (the worktree's `.git` is untouched).
    let git_status = Command::new("git")
        .args(["-C", fx.worktree.to_str().expect("utf8 worktree"), "status"])
        .output()
        .expect("git status");
    eprintln!(
        "host git status (expected to fail on the fake worktree; the \
         point is the worktree itself is intact): {:?}",
        git_status.status
    );
}

/// Rootless Docker: no `--user` in the rendered argv (assertable
/// pre-run) and the same ownership canary proving container root is
/// NOT host root — the file lands owned by the unprivileged engine
/// user (== worktree owner), never by host root (uid 0). Skips when
/// the host engine is not rootless Docker.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn rootless_docker_identity_canary() {
    let fx = live_fixture(
        GitShadowKind::File,
        "id -u > /workspace/canary && id -u >> /output/engine-user",
    );
    if fx.engine != SandboxEngine::Docker || fx.engine_mode != EngineMode::Rootless {
        eprintln!("skipping: host engine is not rootless Docker");
        return;
    }
    // Pre-run argv assertion: rootless Docker emits no --user.
    assert!(
        !fx.argv.contains(&"--user".to_string()),
        "rootless docker must not emit --user, got: {:?}",
        fx.argv
    );

    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");

    let canary = fx.worktree.join("canary");
    assert!(
        wait_for(|| canary.is_file(), Duration::from_secs(5), "canary"),
        "container must have written the canary"
    );
    let meta = std::fs::metadata(&canary).expect("canary stat");
    assert_ne!(
        meta.uid(),
        0,
        "container root must not map to host root under rootless docker"
    );
    assert_eq!(
        meta.uid(),
        std::fs::metadata(&fx.worktree)
            .expect("worktree stat")
            .uid(),
        "canary owner must equal the engine user (== worktree owner)"
    );
}

/// Podman rootless: `--userns keep-id` present and in-container
/// `id -u` == the daemon/worktree owner uid. Skips when the host
/// engine is not Podman.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn podman_keep_id_identity_canary() {
    let fx = live_fixture(GitShadowKind::File, "id -u");
    if fx.engine != SandboxEngine::Podman {
        eprintln!("skipping: host engine is not Podman");
        return;
    }
    let pos = fx
        .argv
        .iter()
        .position(|a| a == "--userns")
        .expect("--userns must be present for podman");
    assert_eq!(fx.argv[pos + 1], "keep-id", "plain keep-id, no uid=/gid=");

    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");
    let in_container_uid: u32 = logs
        .trim()
        .parse()
        .expect("container `id -u` output must be a uid");
    assert_eq!(
        in_container_uid,
        std::fs::metadata(&fx.worktree)
            .expect("worktree stat")
            .uid(),
        "keep-id must make the in-container uid equal the daemon/worktree owner"
    );
}

// ---------------------------------------------------------------------------
// Adversarial `.git` shadow tests
// ---------------------------------------------------------------------------

/// A worker reading `/workspace/.git` sees only the sentinel shadow —
/// the real `gitdir:` pointer content is unreachable.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn git_shadow_read_sees_only_shadow() {
    let fx = live_fixture(GitShadowKind::File, "cat /workspace/.git");
    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "read must succeed; logs: {logs}");
    assert_eq!(
        logs, GIT_SHADOW_FILE_CONTENT,
        "worker must see only the sentinel shadow content"
    );
    assert!(
        !logs.contains("/real/main"),
        "the real gitdir path must never be reachable through the shadow"
    );
}

/// A worker writing `/workspace/.git` is rejected (read-only mount)
/// and the host worktree `.git` is byte-identical after the run.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn git_shadow_write_rejected() {
    let fx = live_fixture(GitShadowKind::File, "echo pwned > /workspace/.git");
    let host_dot_git = fx.worktree.join(".git");
    let before = std::fs::read(&host_dot_git).expect("read host .git");

    let (_, _logs) = run_container(&fx);

    let after = std::fs::read(&host_dot_git).expect("read host .git after run");
    assert_eq!(
        before, after,
        "host worktree .git must be byte-identical after a write attempt"
    );
    // And the shadow on the daemon side still holds the sentinel.
    let shadow = std::fs::read_to_string(&fx.shadow_host).expect("read shadow");
    assert_eq!(shadow, GIT_SHADOW_FILE_CONTENT);
    // The write must have failed inside the container (read-only
    // mount ⇒ non-zero shell exit).
    let fx2 = live_fixture(
        GitShadowKind::File,
        "if (echo pwned > /workspace/.git) 2>/dev/null; then exit 7; fi; exit 0",
    );
    let (code2, logs2) = run_container(&fx2);
    assert_ne!(code2, 7, "writing /workspace/.git must fail; logs: {logs2}");
}

// ---------------------------------------------------------------------------
// Directory-shadow variant
// ---------------------------------------------------------------------------

/// A directory `.git` yields an empty read-only directory shadow: the
/// worker cannot reach the real HEAD content.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn git_shadow_dir_variant_is_empty_and_read_only() {
    let fx = live_fixture(
        GitShadowKind::Dir,
        "ls -A /workspace/.git; if (echo x > /workspace/.git/HEAD) 2>/dev/null; then exit 7; fi; exit 0",
    );
    let (code, logs) = run_container(&fx);
    assert_ne!(
        code, 7,
        "writing into the dir shadow must fail; logs: {logs}"
    );
    assert_eq!(
        logs.trim(),
        "",
        "the dir shadow must be empty; logs: {logs}"
    );
    let host_head = fx.worktree.join(".git").join("HEAD");
    assert_eq!(
        std::fs::read_to_string(&host_head).expect("host HEAD"),
        "ref: refs/heads/main\n",
        "host .git/HEAD must be untouched"
    );
}

// ---------------------------------------------------------------------------
// Disk-fill watchdog (issue #245, task 13.2)
// ---------------------------------------------------------------------------

/// The host disk-pressure watchdog terminates an in-flight OCI run via
/// the existing stop path and refuses new dispatch with
/// `OciDiskPressure` — WITHOUT actually filling the host disk: the
/// reserve is set ABOVE the sampled free space, so the state-dir
/// filesystem is observed as already breached. Recovery-with-margin is
/// asserted on the pure transition the guard runs.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn disk_pressure_watchdog_terminates_in_flight_and_refuses_new_dispatch() {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use caduceus::executor::oci_lifecycle;
    use caduceus::infra::disk::{
        sample_free_bytes, transition, watchdog_paths, DiskPressureGuard, DiskSample,
        PressureState, DISK_HYSTERESIS_BYTES,
    };
    use caduceus::infra::error::CaduceusError;
    use caduceus::state::oci_run::{ContainerRunRow, OciLifecycleState, OciRunState};

    struct NullState;
    impl OciRunState for NullState {
        fn insert(&self, _row: &ContainerRunRow) -> Result<(), caduceus::error::CaduceusError> {
            Ok(())
        }
        fn update_state(
            &self,
            _run_id: &str,
            _state: &OciLifecycleState,
        ) -> Result<(), caduceus::error::CaduceusError> {
            Ok(())
        }
        fn list_pending_reconciliation(
            &self,
        ) -> Result<Vec<ContainerRunRow>, caduceus::error::CaduceusError> {
            Ok(Vec::new())
        }
        fn get(
            &self,
            _run_id: &str,
        ) -> Result<Option<ContainerRunRow>, caduceus::error::CaduceusError> {
            Ok(None)
        }
        fn delete(&self, _run_id: &str) -> Result<(), caduceus::error::CaduceusError> {
            Ok(())
        }
    }

    let fx = live_fixture(GitShadowKind::File, "sleep 3600");

    // 1. Sample the real filesystem hosting the state dir and set the
    //    reserve ABOVE current free space — breach without filling.
    let samples = sample_free_bytes(&watchdog_paths(&fx.cfg)).expect("sample");
    assert!(!samples.is_empty(), "state dir must sample");
    let free = samples[0].free_bytes;
    let device_id = samples[0].device_id;
    let state_dir = fx.cfg.state_dir.clone();
    let reserved_mb = free / (1024 * 1024) + 64; // 64 MiB above free space
    let reserved_bytes = reserved_mb * 1024 * 1024;
    let mut cfg = fx.cfg.clone();
    cfg.sandbox.as_mut().expect("sandbox").reserved_host_disk_mb = reserved_mb;
    let guard = Arc::new(DiskPressureGuard::from_config(&cfg));
    assert!(guard.enabled());

    // 2. In-flight run: drive the REAL engine through the canonical lifecycle
    //    with the fixture's rendered create argv. The wait step blocks
    //    on `sleep 3600` until the breach cancels the watchdog token.
    let guard_for_run = Arc::clone(&guard);
    let guard_for_refresh = Arc::clone(&guard);
    let run_id = fx.run_id.clone();
    let engine = fx.engine;
    let argv = fx.argv.clone();
    let worktree = fx.worktree.clone();
    let shadow_host = fx.shadow_host.clone();
    let engine_mode = fx.engine_mode;
    let fx_cfg = cfg.clone();
    let run = tokio::runtime::Runtime::new().expect("runtime");
    let result = run.block_on(async move {
        let spec = caduceus::executor::ExecutorSpec {
            self_exe: std::path::PathBuf::from("/proc/self/exe"),
            target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
                key: caduceus::github::issue::IssueKey::parse("owner/repo#1").expect("valid key"),
                title: "t".to_string(),
                body: "b".to_string(),
                labels: Vec::new(),
                branch_name: "b".to_string(),
            }),
            worktree: std::path::PathBuf::from("/tmp/worktree"),
            run_id: run_id.clone(),
            context_json: "{}".to_string(),
            worker_command: vec!["sleep".to_string(), "3600".to_string()],
            cancellation: CancellationToken::new(),
        };
        let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
            run_id: run_id.clone(),
            target: spec.target.display(),
            worker_command: spec.worker_command.clone(),
            worktree: worktree.clone(),
            output_dir: fx_cfg
                .state_dir
                .join("oci-runs")
                .join(&run_id)
                .join("output"),
            daemon_id: "live-test-daemon".to_string(),
            workdir_base: fx_cfg.workdir_base.clone(),
            state_dir: fx_cfg.state_dir.clone(),
            worktree_uid: std::fs::metadata(&worktree).expect("worktree stat").uid(),
            worktree_gid: std::fs::metadata(&worktree).expect("worktree stat").gid(),
            engine_mode,
            git_shadow_kind: GitShadowKind::File,
            git_shadow_host: shadow_host,
        };
        let resolved = resolve(fx_cfg.sandbox(), &runtime, &spec).expect("sandbox resolves");
        let state = Arc::new(NullState);
        let adapter = oci_lifecycle::OciAdapter::new(
            engine,
            state,
            fx_cfg.state_dir.clone(),
            runtime.daemon_id,
            spec.target.display(),
            "live-test-command-sha".to_string(),
            argv,
            None,
        );
        let lifecycle = tokio::spawn(async move {
            oci_lifecycle::run_oci_lifecycle(
                &resolved,
                &adapter,
                &oci_lifecycle::LifecycleTimeouts::from_config(&fx_cfg),
                CancellationToken::new(),
                guard_for_run.watchdog_token(),
            )
            .await
        });
        // Deterministic breach point: wait until the container is
        // actually RUNNING (the wait step blocks on `sleep 3600`), so
        // the breach exercises the in-flight stop → capture → rm path
        // and not merely the pre-spawn create check. `ps -a` tolerates
        // a slow start under host load.
        let bin = engine_binary(engine);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let ps = Command::new(&bin)
                .args([
                    "ps",
                    "-a",
                    "--filter",
                    &format!("name={}", run_id),
                    "--format",
                    "{{.ID}}",
                ])
                .output()
                .expect("engine ps");
            if !String::from_utf8_lossy(&ps.stdout).trim().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "container never reached the running state"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // The watchdog breaches on this refresh.
        guard_for_refresh.refresh(&samples);
        assert!(guard_for_refresh.watchdog_token().is_cancelled());
        lifecycle.await.expect("lifecycle task joins")
    });

    // (a) The in-flight run was terminated via the stop path within
    //     the bounded stop/kill timeouts and reported as a
    //     disk-pressure termination (the supervisor outcome carries
    //     the flag rather than surfacing as plain cancellation).
    match result {
        Ok(outcome) => assert!(
            outcome.disk_pressure,
            "watchdog breach must report disk pressure; got {outcome:?}"
        ),
        other => panic!("expected disk-pressure termination outcome; got {other:?}"),
    }
    // (a') The container is really gone from the engine.
    let bin = engine_binary(engine);
    let ps = Command::new(&bin)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={}", fx.run_id),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("engine ps");
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "container must be removed after the breach; got: {}",
        String::from_utf8_lossy(&ps.stdout)
    );
    // (a'') The bounded diagnostic capture was persisted for the
    // terminated run (stop → capture → rm ran in order).
    let engine_log = state_dir
        .join("oci-runs")
        .join(&fx.run_id)
        .join("engine.log");
    assert!(
        engine_log.exists(),
        "engine.log must be captured on the watchdog termination path"
    );

    // (b) New dispatch is refused with the typed error carrying the
    //     breaching path/device/free/reserved facts.
    let err = guard.try_acquire_oci().expect_err("breached guard refuses");
    match err {
        CaduceusError::OciDiskPressure {
            path,
            device_id: err_device,
            free_bytes: err_free,
            reserved_bytes: err_reserved,
        } => {
            assert_eq!(path, state_dir.display().to_string());
            assert_eq!(err_device, device_id);
            assert_eq!(err_free, free);
            assert_eq!(err_reserved, reserved_bytes);
        }
        other => panic!("expected OciDiskPressure; got {other:?}"),
    }

    // (c) TrustedHost work is structurally unaffected: the executor
    //     factory never wires the guard into TrustedHostExecutor, and
    //     a breached guard only cancels its own watchdog tokens.
    let unrelated = CancellationToken::new();
    assert!(!unrelated.is_cancelled());

    // (d) Recovery requires the margin — exactly the pure transition
    //     the guard computes each refresh. (The real filesystem was
    //     never filled, so the live free space stays below the
    //     inflated reserve; the recovery math is asserted here.)
    let breached = PressureState::Breached {
        device_id,
        path: state_dir.display().to_string(),
        free_bytes: free,
        reserved_bytes,
    };
    let still = transition(
        &breached,
        &[DiskSample {
            device_id,
            free_bytes: reserved_bytes + DISK_HYSTERESIS_BYTES,
            representative_path: state_dir.clone(),
        }],
        reserved_bytes,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(still, breached, "exact floor does not re-enable");
    let recovered = transition(
        &breached,
        &[DiskSample {
            device_id,
            free_bytes: reserved_bytes + DISK_HYSTERESIS_BYTES + 1,
            representative_path: state_dir.clone(),
        }],
        reserved_bytes,
        DISK_HYSTERESIS_BYTES,
    );
    assert_eq!(recovered, PressureState::Healthy);
}

// ---------------------------------------------------------------------------
// Live adversarial certification suite (issue #252)
// ---------------------------------------------------------------------------
//
// Each case drives the REAL engine through the typed
// resolve → render → lifecycle pipeline and asserts worker-side
// failure of a hostile action, or the daemon-side state that proves
// containment. The checklist→test mapping is asserted by
// `tests/executor/certification_mapping_test.rs` and documented in
// `docs/certification/oci-certification.md`.

/// A host-side sentinel planted outside every allowed mount (the
/// daemon state dir) must be unreachable from inside the container:
/// the only host-backed surfaces are `/workspace` and `/output`.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn host_sentinel_unreachable_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "for p in /state/sentinel.txt /var/lib/caduceus/sentinel.txt \
         /workspace/../../sentinel.txt /output/../../sentinel.txt /sentinel.txt; do \
         if [ -e \"$p\" ]; then echo \"VISIBLE:$p\"; exit 9; fi; done; exit 0",
    );
    let sentinel = fx.cfg.state_dir.join("sentinel.txt");
    std::fs::write(&sentinel, b"host-secret-sentinel\n").expect("plant host sentinel");

    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");
    assert!(
        !logs.contains("VISIBLE:"),
        "the host sentinel must never be reachable; logs: {logs}"
    );
    assert_eq!(
        std::fs::read(&sentinel).expect("read sentinel"),
        b"host-secret-sentinel\n",
        "the host sentinel must be untouched"
    );
}

/// The daemon state directory and other repositories under the same
/// `workdir_base` are outside the allowed mounts and must be
/// invisible from inside the container.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn daemon_state_and_other_repos_unreachable_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "for p in /state /var/lib/caduceus /owner /owner/other-repo \
         /workspace/../../owner/other-repo /output/../../owner/other-repo; do \
         if [ -e \"$p\" ]; then echo \"VISIBLE:$p\"; exit 9; fi; done; exit 0",
    );
    // A neighbor repository the worker must never see.
    let neighbor = fx.cfg.workdir_base.join("owner").join("other-repo");
    std::fs::create_dir_all(&neighbor).expect("create neighbor repo");
    let neighbor_marker = neighbor.join("marker.txt");
    std::fs::write(&neighbor_marker, b"other-repo-marker\n").expect("write neighbor marker");
    // Daemon-side state marker (outside every mount).
    let state_marker = fx.cfg.state_dir.join("daemon-state.json");
    std::fs::write(&state_marker, b"{\"secret\":true}\n").expect("write state marker");

    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");
    assert!(
        !logs.contains("VISIBLE:"),
        "daemon state and other repos must never be visible; logs: {logs}"
    );
    assert_eq!(
        std::fs::read(&neighbor_marker).expect("read marker"),
        b"other-repo-marker\n",
        "the neighbor repo must be untouched"
    );
    assert_eq!(
        std::fs::read(&state_marker).expect("read state marker"),
        b"{\"secret\":true}\n",
        "the daemon state must be untouched"
    );
}

/// The workspace stays writable, the rootfs is read-only (EROFS),
/// and the output/result path works — the frozen writable-surface
/// contract (I4).
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn workspace_writable_rootfs_readonly_output_writes_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "echo ws-canary > /workspace/ws-canary.txt; \
         echo out-canary > /output/out-canary.txt; \
         if (echo escape > /etc/escape-attempt.txt) 2>/dev/null; then exit 9; fi; exit 0",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "workspace+output writes must succeed and rootfs writes must fail; logs: {logs}"
    );
    let ws = fx.worktree.join("ws-canary.txt");
    assert_eq!(
        std::fs::read_to_string(&ws).expect("workspace canary"),
        "ws-canary\n",
        "the workspace write must land host-side"
    );
    let out = fx
        .cfg
        .state_dir
        .join("oci-runs")
        .join(&fx.run_id)
        .join("output")
        .join("out-canary.txt");
    assert_eq!(
        std::fs::read_to_string(&out).expect("output canary"),
        "out-canary\n",
        "the output write must land host-side"
    );
}

/// `--cap-drop ALL` and `no-new-privileges` are verified from inside
/// the container: CapEff (and CapBnd) are empty and NoNewPrivs holds.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn capabilities_absent_no_new_privileges_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "grep CapEff /proc/self/status; grep CapBnd /proc/self/status; \
         grep NoNewPrivs /proc/self/status; exit 0",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "container must exit cleanly; logs: {logs}");

    let cap_field = |name: &str| {
        logs.lines()
            .find_map(|line| line.strip_prefix(name).map(str::trim))
            .unwrap_or_else(|| panic!("{name} missing from /proc/self/status; logs: {logs}"))
    };
    let eff =
        u64::from_str_radix(cap_field("CapEff:").trim_start_matches("0x"), 16).expect("CapEff hex");
    assert_eq!(
        eff, 0,
        "CapEff must be empty after --cap-drop ALL; logs: {logs}"
    );
    let bnd =
        u64::from_str_radix(cap_field("CapBnd:").trim_start_matches("0x"), 16).expect("CapBnd hex");
    assert_eq!(
        bnd, 0,
        "CapBnd must be empty after --cap-drop ALL; logs: {logs}"
    );
    assert_eq!(
        cap_field("NoNewPrivs:"),
        "1",
        "no-new-privileges must hold; logs: {logs}"
    );
}

/// The runtime socket is absent inside the container and device
/// creation (mknod) is denied — CAP_MKNOD is dropped.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn runtime_socket_and_device_absent_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "for p in /var/run/docker.sock /run/docker.sock /var/run/docker.sock.raw \
         /run/containerd/containerd.sock; do \
         if [ -e \"$p\" ]; then echo \"SOCK:$p\"; exit 9; fi; done; \
         if (mknod /tmp/cdev c 1 3) 2>/dev/null; then echo DEVICE-MKNOD-SUCCEEDED; exit 9; fi; \
         exit 0",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "the runtime socket must be absent and mknod denied; logs: {logs}"
    );
    assert!(
        !logs.contains("SOCK:"),
        "the runtime socket must never be visible; logs: {logs}"
    );
    assert!(
        !logs.contains("DEVICE-MKNOD-SUCCEEDED"),
        "mknod must fail with capabilities dropped; logs: {logs}"
    );
}

/// A memory hog far beyond the sandbox memory bound must be
/// OOM-killed by the cgroup (exit 137), proving the bound is enforced.
///
/// The bound is lowered to the config minimum (64 MiB) so the hog needs
/// only a few hundred MiB of host memory at the breach point. The hog
/// grows one shell variable by doubling inside PID 1 — no pipes, no
/// spool files, and no helper process — so the in-cgroup OOM victim is
/// necessarily the shell itself (exit 137), never a child whose death
/// would let the script exit 0. On memory-constrained runners the
/// larger default hog could trip the *host* OOM killer (which picks
/// victims across cgroups) instead of the deterministic in-cgroup kill.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn memory_hog_oom_live() {
    // Doubling from a non-empty seed: 2^28 bytes (~256 MiB) would be
    // the ceiling if the limit were not enforced; the 64 MiB cgroup
    // bound is breached around iteration 26, killing PID 1.
    let script = "s=a; i=0; while [ $i -lt 28 ]; do i=$((i+1)); s=$s$s; done; exit 0";
    let fx = live_fixture_with(GitShadowKind::File, script, |cfg| {
        cfg.sandbox.as_mut().expect("sandbox").resources.memory_mb = 64;
    });
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 137,
        "the memory hog must be OOM-killed (exit 137); logs: {logs}"
    );
}

/// A fork bomb with children that stay alive must hit the cgroup
/// `--pids-limit=256` boundary. Busybox `sh` treats a refused fork as
/// fatal: it prints `can't fork` (EAGAIN) and aborts the script with
/// status 2. Either way the concurrent process count stays far below
/// the bomb's 400-child target and the host is untouched.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn fork_bomb_eagain_live() {
    let script = "i=0; ok=0; while [ $i -lt 400 ]; do i=$((i+1)); sh -c 'sleep 1' & if [ $? -eq 0 ]; then ok=$((ok+1)); else echo FORK-FAILED; break; fi; done; echo forked=$ok; wait; exit 0";
    let fx = live_fixture(GitShadowKind::File, script);
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 2,
        "busybox aborts with status 2 when the pids cgroup refuses a fork; logs: {logs}"
    );
    assert!(
        logs.contains("can't fork"),
        "the bomb must observe EAGAIN at the pids boundary; logs: {logs}"
    );
}

/// `--cpus=2` caps the cgroup at two CPUs. Eight spinners demand
/// eight, so the CFS quota must throttle: `cpu.stat` throttled_usec
/// increases while the container runs — enforcement, not argv.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn cpu_burn_throttled_live() {
    let script = "before=$(awk '/throttled_usec/ {print $2}' /sys/fs/cgroup/cpu.stat \
                  2>/dev/null || echo 0); cat /sys/fs/cgroup/cpu.max; \
                  i=0; while [ $i -lt 8 ]; do i=$((i+1)); ( while :; do :; done ) & done; \
                  sleep 4; \
                  after=$(awk '/throttled_usec/ {print $2}' /sys/fs/cgroup/cpu.stat \
                  2>/dev/null || echo 0); \
                  echo throttled_before=$before throttled_after=$after; \
                  [ \"$after\" -gt \"$before\" ] && exit 0 || exit 9";
    let fx = live_fixture(GitShadowKind::File, script);
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "the CPU burn must be throttled by the cgroup quota; logs: {logs}"
    );
    assert!(
        logs.contains("200000 100000"),
        "cpu.max must carry the configured 2-CPU quota; logs: {logs}"
    );
}

/// Writes beyond the bounded `/tmp` tmpfs (256m) must fail ENOSPC.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn tmpfs_bounded_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "dd if=/dev/zero of=/tmp/big bs=1M count=300 2>/dev/null; \
         rc=$?; rm -f /tmp/big; [ $rc -ne 0 ] && exit 0 || exit 9",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "writing beyond the /tmp tmpfs bound must fail ENOSPC; logs: {logs}"
    );
}

/// Writes beyond the bounded `/dev/shm` tmpfs (64m) must fail ENOSPC.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn dev_shm_bounded_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "dd if=/dev/zero of=/dev/shm/big bs=1M count=128 2>/dev/null; \
         rc=$?; rm -f /dev/shm/big; [ $rc -ne 0 ] && exit 0 || exit 9",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "writing beyond the /dev/shm tmpfs bound must fail ENOSPC; logs: {logs}"
    );
}

/// `--network none` blocks all egress (default network mode).
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn network_none_unreachable_live() {
    let fx = live_fixture(
        GitShadowKind::File,
        "if wget -T 3 -O /dev/null http://1.1.1.1/ 2>/dev/null; then \
         echo NET-REACHED; exit 9; fi; exit 0",
    );
    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "--network none must block egress; logs: {logs}");
    assert!(
        !logs.contains("NET-REACHED"),
        "no egress may exist under --network none; logs: {logs}"
    );
}

/// Unrestricted networking works (egress succeeds) AND is the
/// engine's isolated bridge — never host networking.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn unrestricted_not_host_live() {
    let fx = live_fixture_with(
        GitShadowKind::File,
        "wget -T 5 -O /dev/null http://1.1.1.1/ && echo EGRESS-OK; \
         ip route 2>/dev/null | head -3; exit 0",
        |cfg| {
            cfg.sandbox.as_mut().expect("sandbox").network = SandboxNetwork::Unrestricted;
        },
    );
    // Structural assertion: host networking is unrepresentable.
    assert!(
        !fx.argv.iter().any(|a| a == "host" || a == "--network=host"),
        "unrestricted must never render host networking; argv: {:?}",
        fx.argv
    );
    let pos = fx
        .argv
        .iter()
        .position(|a| a == "--network")
        .expect("--network must be rendered");
    assert_eq!(fx.argv[pos + 1], "bridge", "unrestricted renders bridge");

    let (code, logs) = run_container(&fx);
    assert_eq!(code, 0, "unrestricted egress must work; logs: {logs}");
    assert!(
        logs.contains("EGRESS-OK"),
        "unrestricted mode must reach the network; logs: {logs}"
    );
}

/// A lifecycle worker timeout must clean the container up (stop →
/// rm) and capture engine logs for diagnosis.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn timeout_cleans_container_live() {
    let fx = live_fixture(GitShadowKind::File, "sleep 3600");
    let harness = lifecycle_harness(&fx, vec!["sleep".to_string(), "3600".to_string()]);
    let argv = render(&harness.spec, fx.engine);
    let state: Arc<dyn OciRunState> = Arc::new(MemState::default());
    let adapter = OciAdapter::new(
        fx.engine,
        Arc::clone(&state),
        fx.cfg.state_dir.clone(),
        "live-test-daemon".to_string(),
        harness.spec_exec.target.display(),
        "live-command-sha".to_string(),
        argv,
        None,
    );
    // Engine-command bound: 10s, >= the 5s production default
    // (DEFAULT_SANDBOX_KILL_TIMEOUT_SECONDS) so a cold first
    // `docker create` on a shared CI runner (which must resolve
    // the digest-pinned reference) cannot flake the suite.
    let timeouts = LifecycleTimeouts {
        worker_timeout: Duration::from_secs(3),
        stop_grace: Duration::from_secs(1),
        kill_timeout: Duration::from_secs(10),
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let outcome = runtime
        .block_on(run_oci_lifecycle(
            &harness.spec,
            &adapter,
            &timeouts,
            CancellationToken::new(),
            CancellationToken::new(),
        ))
        .expect("lifecycle resolves");
    assert!(
        outcome.timed_out,
        "worker timeout must fire; outcome: {outcome:?}"
    );
    assert!(!outcome.cancelled, "timeout must not report cancellation");

    assert_no_container(&fx, "after the worker timeout");
    let engine_log = fx
        .cfg
        .state_dir
        .join("oci-runs")
        .join(&fx.run_id)
        .join("engine.log");
    assert!(
        engine_log.exists(),
        "engine.log must be captured on the timeout path"
    );
}

/// A cancelled lifecycle must clean the container up and report
/// `cancelled`, with the run row resolved.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn cancellation_cleans_container_live() {
    let fx = live_fixture(GitShadowKind::File, "sleep 3600");
    let harness = lifecycle_harness(&fx, vec!["sleep".to_string(), "3600".to_string()]);
    let argv = render(&harness.spec, fx.engine);
    let state: Arc<dyn OciRunState> = Arc::new(MemState::default());
    let adapter = OciAdapter::new(
        fx.engine,
        Arc::clone(&state),
        fx.cfg.state_dir.clone(),
        "live-test-daemon".to_string(),
        harness.spec_exec.target.display(),
        "live-command-sha".to_string(),
        argv,
        None,
    );
    let timeouts = LifecycleTimeouts {
        worker_timeout: Duration::from_secs(60),
        stop_grace: Duration::from_secs(1),
        kill_timeout: Duration::from_secs(10),
    };
    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();
    let bin = engine_binary(fx.engine);
    let run_id = fx.run_id.clone();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let outcome = runtime
        .block_on(async {
            let lifecycle = tokio::spawn(async move {
                run_oci_lifecycle(
                    &harness.spec,
                    &adapter,
                    &timeouts,
                    cancel_for_run,
                    CancellationToken::new(),
                )
                .await
            });
            // Wait until the container is created (start may lag under
            // host load), then cancel. `ps -a` also catches a container
            // that was created but failed to start, so the lifecycle
            // error surfaces instead of a generic timeout.
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let ps = Command::new(&bin)
                    .args([
                        "ps",
                        "-a",
                        "--filter",
                        &format!("name={run_id}"),
                        "--format",
                        "{{.ID}}",
                    ])
                    .output()
                    .expect("engine ps");
                if !String::from_utf8_lossy(&ps.stdout).trim().is_empty() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "container never reached the running state"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            cancel.cancel();
            lifecycle.await.expect("lifecycle task joins")
        })
        .expect("lifecycle resolves");
    assert!(
        outcome.cancelled,
        "cancellation must win the race; outcome: {outcome:?}"
    );
    assert!(!outcome.timed_out, "cancellation must not report timeout");

    assert_no_container(&fx, "after cancellation");
    let row = state
        .get(&fx.run_id)
        .expect("state read")
        .expect("row must exist");
    assert!(
        row.state.is_terminal(),
        "row must resolve terminal: {row:?}"
    );
}

/// A daemon crash mid-run leaves a labeled running container plus a
/// non-terminal row; a fresh daemon's startup reconciliation must
/// find the orphan and remove it (stop → rm), resolving the row.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn crash_restart_orphan_reconcile_live() {
    let fx = live_fixture(GitShadowKind::File, "sleep 3600");
    let harness = lifecycle_harness(&fx, vec!["sleep".to_string(), "3600".to_string()]);
    let argv = render(&harness.spec, fx.engine);
    let bin = engine_binary(fx.engine);
    let state: Arc<dyn OciRunState> = Arc::new(MemState::default());

    // 1. Crash point: create + start succeeded, then the daemon died
    //    before the wait step — the container is orphaned but labeled.
    run_or_die(Command::new(&bin).args(&argv[1..]), "engine create");
    run_or_die(
        Command::new(&bin).args(["start", &fx.run_id]),
        "engine start",
    );
    let now = chrono::Utc::now().to_rfc3339();
    state
        .insert(&ContainerRunRow {
            run_id: fx.run_id.clone(),
            container_id: None,
            state: OciLifecycleState::Running,
            engine: format!("{:?}", fx.engine),
            created_at: now.clone(),
            updated_at: now,
            daemon_id: "live-test-daemon".to_string(),
            issue_id: harness.spec_exec.target.display(),
            worker_command_sha256: "live-command-sha".to_string(),
        })
        .expect("insert row");
    let ps = Command::new(&bin)
        .args([
            "ps",
            "--filter",
            &format!("name={}", fx.run_id),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("engine ps");
    assert!(
        !String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "the orphan container must be running before reconciliation"
    );

    // 2. Fresh daemon startup reconciliation through the production path.
    let mut cfg = fx.cfg.clone();
    cfg.sandbox.as_mut().expect("sandbox").engine = fx.engine;
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(caduceus::executor::oci_lifecycle::reconcile_installation(
            &cfg,
            Arc::clone(&state),
            "live-test-daemon",
            CancellationToken::new(),
        ))
        .expect("reconciliation resolves");

    // 3. The orphan is gone and the row resolved.
    assert_no_container(&fx, "after crash/restart reconciliation");
    let row = state
        .get(&fx.run_id)
        .expect("state read")
        .expect("row must exist");
    assert!(
        row.state.is_terminal(),
        "the orphan row must resolve to a terminal state: {row:?}"
    );
}

/// The lifecycle heartbeat file is written after start and its
/// `updated_at` advances while the container runs (I7 frozen
/// semantics), then is cleared when the run ends.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn heartbeat_advances_during_run_live() {
    let fx = live_fixture(GitShadowKind::File, "sleep 3600");
    let harness = lifecycle_harness(&fx, vec!["sleep".to_string(), "3600".to_string()]);
    let argv = render(&harness.spec, fx.engine);
    let state: Arc<dyn OciRunState> = Arc::new(MemState::default());
    let adapter = OciAdapter::new(
        fx.engine,
        Arc::clone(&state),
        fx.cfg.state_dir.clone(),
        "live-test-daemon".to_string(),
        harness.spec_exec.target.display(),
        "live-command-sha".to_string(),
        argv,
        None,
    );
    let timeouts = LifecycleTimeouts {
        worker_timeout: Duration::from_secs(60),
        stop_grace: Duration::from_secs(1),
        kill_timeout: Duration::from_secs(10),
    };
    let heartbeat_path = fx
        .cfg
        .state_dir
        .join("runs")
        .join(&fx.run_id)
        .with_extension("heartbeat");
    let cancel = CancellationToken::new();
    let cancel_for_run = cancel.clone();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (first, second, outcome) = runtime.block_on(async {
        let lifecycle = tokio::spawn(async move {
            run_oci_lifecycle(
                &harness.spec,
                &adapter,
                &timeouts,
                cancel_for_run,
                CancellationToken::new(),
            )
            .await
        });
        // The initial heartbeat is written right after start.
        let deadline = Instant::now() + Duration::from_secs(60);
        while !heartbeat_path.exists() {
            assert!(Instant::now() < deadline, "heartbeat file never appeared");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let first = std::fs::read_to_string(&heartbeat_path).expect("read heartbeat");
        // Wait past the 5s refresh interval.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let second = std::fs::read_to_string(&heartbeat_path).expect("read heartbeat again");
        cancel.cancel();
        let outcome = lifecycle
            .await
            .expect("lifecycle task joins")
            .expect("lifecycle resolves");
        (first, second, outcome)
    });

    let updated_at = |raw: &str| -> String {
        let value: serde_json::Value = serde_json::from_str(raw).expect("heartbeat must be JSON");
        value
            .get("updated_at")
            .and_then(|v| v.as_str())
            .expect("updated_at field")
            .to_string()
    };
    assert_ne!(
        updated_at(&first),
        updated_at(&second),
        "heartbeat updated_at must advance during a live run"
    );
    assert!(
        outcome.cancelled,
        "the heartbeat run must end by cancellation; outcome: {outcome:?}"
    );
    assert!(
        !heartbeat_path.exists(),
        "the heartbeat must be cleared when the run ends"
    );
}

/// A wrong (corrupted) image digest is rejected by the production
/// acquisition path BEFORE any container is created.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn wrong_digest_rejected_before_execution_live() {
    let reference =
        std::env::var("CADUCEUS_LIVE_TEST_IMAGE").expect("CADUCEUS_LIVE_TEST_IMAGE required");
    let (repo, digest) = reference
        .rsplit_once("@sha256:")
        .expect("reference must be digest-pinned");
    // Corrupt the digest by flipping one hex digit.
    let mut chars: Vec<char> = digest.chars().collect();
    chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
    let wrong = format!("{repo}@sha256:{}", chars.iter().collect::<String>());

    let (engine, _mode) =
        detect_engine().expect("no usable Docker/Podman engine for gated live tests");
    let adapter = OciImageAdapter::new(engine);
    let host = host_platform();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(acquire_image_with_adapter(
        &adapter,
        &wrong,
        OciPullPolicy::Never,
        &host,
    ));
    let err = result.expect_err("a corrupted digest must be rejected before execution");
    assert!(
        matches!(
            err,
            CaduceusError::OciImageMissing { .. }
                | CaduceusError::OciImageDigestMismatch { .. }
                | CaduceusError::OciImageInspectFailed { .. }
        ),
        "expected a typed image rejection, got {err:?}"
    );
    // No container was created by the rejected acquisition.
    let bin = engine_binary(engine);
    let probe_name = next_run_id();
    let ps = Command::new(&bin)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={probe_name}"),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("engine ps");
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "no container may exist after a rejected image"
    );
}

/// A custom unrelated worker image (not derived from the reference
/// image) runs the same worker command successfully — the sandbox is
/// image-neutral, not reference-image-coupled.
#[test]
#[cfg_attr(not(env = "CADUCEUS_RUN_ISOLATION_TESTS"), ignore)]
fn image_neutrality_custom_unrelated_image_live() {
    // This test is the ONLY consumer of the neutrality override: it
    // reads `CADUCEUS_LIVE_NEUTRALITY_IMAGE` explicitly and scopes it
    // to its own sandbox config, while the shared `live_fixture_with`
    // helper uses `CADUCEUS_LIVE_TEST_IMAGE` for every other test.
    let neutrality = std::env::var("CADUCEUS_LIVE_NEUTRALITY_IMAGE")
        .expect("CADUCEUS_LIVE_NEUTRALITY_IMAGE required for the image-neutrality certification");
    assert!(
        neutrality.contains("@sha256:"),
        "CADUCEUS_LIVE_NEUTRALITY_IMAGE must be digest-pinned, got: {neutrality}"
    );
    let reference = std::env::var("CADUCEUS_LIVE_TEST_IMAGE").unwrap_or_default();
    let fx = live_fixture_with(
        GitShadowKind::File,
        "echo neutrality-ok; id -u > /workspace/neutrality-canary; exit 0",
        |cfg| {
            cfg.sandbox.as_mut().expect("sandbox").image = neutrality.clone();
        },
    );
    let used = fx.cfg.sandbox().image.clone();
    if !reference.is_empty() {
        assert_ne!(
            used, reference,
            "the neutrality image must differ from the reference image"
        );
    }
    assert!(
        !used.contains("caduceus-worker-reference"),
        "the neutrality image must not be the reference image: {used}"
    );

    let (code, logs) = run_container(&fx);
    assert_eq!(
        code, 0,
        "unrelated image must run the worker command; logs: {logs}"
    );
    assert!(
        logs.contains("neutrality-ok"),
        "worker command must run; logs: {logs}"
    );
    let canary = fx.worktree.join("neutrality-canary");
    assert!(
        wait_for(
            || canary.is_file(),
            Duration::from_secs(5),
            "neutrality canary"
        ),
        "worker output must land in /workspace"
    );
}

// ---------------------------------------------------------------------------
// Shared helpers for the lifecycle-driven certification tests
// ---------------------------------------------------------------------------

/// In-memory [`OciRunState`] for the lifecycle tests: records rows so
/// the crash/restart reconciliation path can be asserted.
#[derive(Default)]
struct MemState {
    rows: Mutex<Vec<ContainerRunRow>>,
}

impl OciRunState for MemState {
    fn insert(&self, row: &ContainerRunRow) -> Result<(), caduceus::error::CaduceusError> {
        self.rows.lock().expect("lock").push(row.clone());
        Ok(())
    }

    fn update_state(
        &self,
        run_id: &str,
        state: &OciLifecycleState,
    ) -> Result<(), caduceus::error::CaduceusError> {
        if let Some(row) = self
            .rows
            .lock()
            .expect("lock")
            .iter_mut()
            .find(|row| row.run_id == run_id)
        {
            row.state = state.clone();
        }
        Ok(())
    }

    fn update_container_id(
        &self,
        run_id: &str,
        container_id: &str,
    ) -> Result<(), caduceus::error::CaduceusError> {
        if let Some(row) = self
            .rows
            .lock()
            .expect("lock")
            .iter_mut()
            .find(|row| row.run_id == run_id)
        {
            row.container_id = Some(container_id.to_string());
        }
        Ok(())
    }

    fn list_by_daemon_id(
        &self,
        daemon_id: &str,
    ) -> Result<Vec<ContainerRunRow>, caduceus::error::CaduceusError> {
        Ok(self
            .rows
            .lock()
            .expect("lock")
            .iter()
            .filter(|row| row.daemon_id == daemon_id)
            .cloned()
            .collect())
    }

    fn list_pending_reconciliation(
        &self,
    ) -> Result<Vec<ContainerRunRow>, caduceus::error::CaduceusError> {
        Ok(Vec::new())
    }

    fn get(&self, run_id: &str) -> Result<Option<ContainerRunRow>, caduceus::error::CaduceusError> {
        Ok(self
            .rows
            .lock()
            .expect("lock")
            .iter()
            .find(|row| row.run_id == run_id)
            .cloned())
    }

    fn delete(&self, run_id: &str) -> Result<(), caduceus::error::CaduceusError> {
        self.rows
            .lock()
            .expect("lock")
            .retain(|row| row.run_id != run_id);
        Ok(())
    }
}

/// Rebuild the `ExecutorSpec` + `RuntimeFacts` for a fixture so the
/// production lifecycle can be driven with a chosen worker command
/// (the fixture's own argv is only used by the direct `run_container`
/// helper).
struct LifecycleHarness {
    spec: caduceus::executor::sandbox_spec::SandboxSpec,
    spec_exec: caduceus::executor::ExecutorSpec,
}

fn lifecycle_harness(fx: &LiveFixture, worker_command: Vec<String>) -> LifecycleHarness {
    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: fx.run_id.clone(),
        target: "owner/repo#1".to_string(),
        worker_command: worker_command.clone(),
        worktree: fx.worktree.clone(),
        output_dir: fx
            .cfg
            .state_dir
            .join("oci-runs")
            .join(&fx.run_id)
            .join("output"),
        daemon_id: "live-test-daemon".to_string(),
        workdir_base: fx.cfg.workdir_base.clone(),
        state_dir: fx.cfg.state_dir.clone(),
        worktree_uid: std::fs::metadata(&fx.worktree)
            .expect("worktree stat")
            .uid(),
        worktree_gid: std::fs::metadata(&fx.worktree)
            .expect("worktree stat")
            .gid(),
        engine_mode: fx.engine_mode,
        git_shadow_kind: GitShadowKind::File,
        git_shadow_host: fx.shadow_host.clone(),
    };
    let spec_exec = caduceus::executor::ExecutorSpec {
        self_exe: std::path::PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: caduceus::github::issue::IssueKey::parse(&runtime.target)
                .expect("fixture target parses as issue key"),
            title: "title".to_string(),
            body: "body".to_string(),
            labels: vec!["bug".to_string()],
            branch_name: "caduceus/owner/repo#1".to_string(),
        }),
        worktree: fx.worktree.clone(),
        run_id: fx.run_id.clone(),
        context_json: "{}".to_string(),
        worker_command,
        cancellation: CancellationToken::new(),
    };
    let spec = resolve(fx.cfg.sandbox(), &runtime, &spec_exec).expect("sandbox must resolve");
    LifecycleHarness { spec, spec_exec }
}

/// Assert no engine container exists for the fixture's run id.
fn assert_no_container(fx: &LiveFixture, context: &str) {
    let bin = engine_binary(fx.engine);
    let ps = Command::new(&bin)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("name={}", fx.run_id),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .expect("engine ps");
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "container must be removed {context}; got: {}",
        String::from_utf8_lossy(&ps.stdout)
    );
}
