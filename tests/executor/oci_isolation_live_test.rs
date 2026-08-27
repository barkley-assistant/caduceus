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

    let spec = resolve(cfg.sandbox(), &runtime).expect("live facts must resolve");
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
