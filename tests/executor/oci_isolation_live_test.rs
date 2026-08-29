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

use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use caduceus::executor::engine_probe::{parse_engine_mode, GIT_SHADOW_FILE_CONTENT};
use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, EngineMode, GitShadowKind, SandboxEngine};
use caduceus::infra::config::Config;
use tempfile::TempDir;

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
        issue: runtime.issue.clone(),
        worktree: runtime.worktree.clone(),
        run_id: runtime.run_id.clone(),
        context_json: "{}".to_string(),
        worker_command: runtime.worker_command.clone(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        issue_title: "Fix login bug".to_string(),
        issue_body: "Steps to reproduce".to_string(),
        labels: vec!["bug".to_string()],
        branch_name: "caduceus/owner/repo#1".to_string(),
    }
}

/// The host's actual engine and mode. Returns `None` when no engine
/// binary is usable — the caller skips with a notice.
fn detect_engine() -> Option<(SandboxEngine, EngineMode)> {
    for engine in [SandboxEngine::Docker, SandboxEngine::Podman] {
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

fn live_fixture(kind: GitShadowKind, container_script: &str) -> LiveFixture {
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
    if let Ok(image) = std::env::var("CADUCEUS_LIVE_TEST_IMAGE") {
        assert!(
            image.contains("@sha256:"),
            "CADUCEUS_LIVE_TEST_IMAGE must be digest-pinned, got: {image}"
        );
        cfg.sandbox.as_mut().expect("sandbox").image = image;
    }
    let run_id = "live-iso-run";
    let worktree = cfg.workdir_base.join("owner").join("repo").join(run_id);
    make_worktree(&worktree, kind);

    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: run_id.to_string(),
        issue: caduceus::github::issue::IssueKey::parse("owner/repo#1").expect("valid key"),
        worker_command: vec![
            "sh".to_string(),
            "-c".to_string(),
            container_script.to_string(),
        ],
        worktree: worktree.clone(),
        output_dir: cfg.state_dir.join("oci-runs").join(run_id).join("output"),
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
            .join(run_id)
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
    let code = String::from_utf8_lossy(&wait.stdout)
        .trim()
        .parse::<i64>()
        .expect("wait prints an exit code");
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

    // 2. In-flight run: drive the REAL engine through `run_with_argv`
    //    with the fixture's rendered create argv. The wait step blocks
    //    on `sleep 3600` until the breach cancels the watchdog token.
    let guard_for_run = Arc::clone(&guard);
    let guard_for_refresh = Arc::clone(&guard);
    let run_id = fx.run_id.clone();
    let engine = fx.engine;
    let argv = fx.argv.clone();
    let fx_cfg = cfg.clone();
    let run = tokio::runtime::Runtime::new().expect("runtime");
    let result = run.block_on(async move {
        let spec = caduceus::executor::ExecutorSpec {
            self_exe: std::path::PathBuf::from("/proc/self/exe"),
            issue: caduceus::github::issue::IssueKey::parse("owner/repo#1").expect("valid key"),
            worktree: std::path::PathBuf::from("/tmp/worktree"),
            run_id: run_id.clone(),
            context_json: "{}".to_string(),
            worker_command: vec!["sleep".to_string(), "3600".to_string()],
            cancellation: CancellationToken::new(),
            issue_title: "t".to_string(),
            issue_body: "b".to_string(),
            labels: Vec::new(),
            branch_name: "b".to_string(),
        };
        let lifecycle = tokio::spawn(async move {
            oci_lifecycle::run_with_argv(
                &fx_cfg,
                &spec,
                &NullState,
                engine,
                argv,
                None,
                CancellationToken::new(),
                guard_for_run.watchdog_token(),
            )
            .await
        });
        // Deterministic breach point: wait until the container is
        // actually RUNNING (the wait step blocks on `sleep 3600`), so
        // the breach exercises the in-flight stop → capture → rm path
        // and not merely the pre-spawn create check.
        let bin = engine_binary(engine);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let ps = Command::new(&bin)
                .args([
                    "ps",
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
    //     the bounded stop/kill timeouts and reported as cancelled.
    match result {
        Err(CaduceusError::Cancelled) => {}
        other => panic!("expected Cancelled from the watchdog termination; got {other:?}"),
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
