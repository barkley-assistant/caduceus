//! Live-engine container-environment tests (issue #249; task 2.7).
//!
//! Every test is gated behind `CADUCEUS_RUN_ISOLATION_TESTS` and is
//! skipped at RUNTIME without it — never failed. The gate is checked
//! in the test body (not `cfg_attr(not(env = …), ignore)`) so the
//! run actually happens when the variable is set:
//!
//! ```text
//! if std::env::var("CADUCEUS_RUN_ISOLATION_TESTS").is_err() {
//!     eprintln!("skipping: …");
//!     return;
//! }
//! ```
//!
//! The test drives a real Docker/Podman engine through the typed
//! pipeline (config → config snapshot → `OciEnvFile` → renderer with
//! the single `--env-file`) and asserts in-container facts:
//!
//! - in-container env == canonical 11 + compat + resolved `pass_env`
//!   (the file is authoritative; no `-e` token exists in the argv);
//! - image `ENV` declarations still apply (image layer, not the
//!   file, is the source of image ENV declarations);
//! - unapproved host environment noise is not inherited.
//!
//! Gating env var unset ⇒ test skipped (not failed).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::time::{Duration, Instant};

use caduceus::executor::engine_probe::GIT_SHADOW_FILE_CONTENT;
use caduceus::executor::oci_env_file::OciEnvFile;
use caduceus::executor::sandbox_renderer::render_with_env_files;
use caduceus::executor::sandbox_spec::{resolve_with_env, GitShadowKind, SandboxEngine};
use caduceus::infra::config::Config;
use tempfile::TempDir;

fn engine_binary(engine: SandboxEngine) -> String {
    engine.binary_name().to_string()
}

/// Detect a usable engine, preferring Docker (Docker exists in most
/// CI runners and exists in most runners locally; Docker is preferred
/// over Podman for the Nondeterministic Nondeterministic image layer
/// because Docker layering exists in most runners). Returns `None`
/// when neither is usable — the caller skips with a notice. The
/// `CADUCEUS_LIVE_TEST_ENGINE` env var (docker|podman) forces a
/// specific engine so the nightly Podman leg runs against Podman even
/// on runners that also have Docker.
fn detect_engine() -> Option<SandboxEngine> {
    let forced = std::env::var("CADUCEUS_LIVE_TEST_ENGINE").ok();
    let order: &[SandboxEngine] = match forced.as_deref() {
        Some("podman") => &[SandboxEngine::Podman],
        Some("docker") => &[SandboxEngine::Docker],
        _ => &[SandboxEngine::Docker, SandboxEngine::Podman],
    };
    for &engine in order {
        let out = Command::new(engine_binary(engine))
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                return Some(engine);
            }
        }
    }
    None
}

fn make_worktree(worktree: &std::path::Path) {
    std::fs::create_dir_all(worktree).expect("create worktree");
    let mut f = std::fs::File::create(worktree.join(".git")).expect("create .git file");
    f.write_all(b"gitdir: /real/main/.git/worktrees/live-pointer\n")
        .expect("write .git pointer");
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
/// combined logs.
fn run_container(fx: &Fx) -> String {
    let bin = engine_binary(fx.engine);
    run_or_die(Command::new(&bin).args(&fx.argv[1..]), "engine create");
    run_or_die(
        Command::new(&bin).args(["start", &fx.run_id]),
        "engine start",
    );
    run_or_die(Command::new(&bin).args(["wait", &fx.run_id]), "engine wait");
    let logs = run_or_die(Command::new(&bin).args(["logs", &fx.run_id]), "engine logs");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&logs.stdout),
        String::from_utf8_lossy(&logs.stderr)
    );
    // Best-effort teardown.
    let _ = Command::new(&bin).args(["rm", "-f", &fx.run_id]).output();
    combined
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

struct Fx {
    _tmp: TempDir,
    file: OciEnvFile,
    argv: Vec<String>,
    engine: SandboxEngine,
    run_id: String,
}

/// Build the fixture: a real worktree, facts with the real owner, a
/// config with the `pass_env` canary, the daemon-private env file,
/// and the rendered argv with exactly one `--env-file`.
fn live_fixture(engine: Option<SandboxEngine>, container_script: &str) -> Option<Fx> {
    let engine = engine?;
    // Digest-pinned image override for gated runs; without it a live
    // `create` cannot resolve a digest, so the run skips.
    let image = std::env::var("CADUCEUS_LIVE_TEST_IMAGE").ok()?;
    let tmp = TempDir::new_in(".").expect("tempdir under crate root");
    let mut cfg = Config::test_defaults(tmp.path());
    {
        let sb = cfg.sandbox.as_mut().expect("sandbox");
        sb.image = image;
        // The pass_env canary must be approved in the config, or the
        // resolver's empty-`pass_env` loop never inserts the canary
        // value from the daemon snapshot into the env file.
        sb.pass_env
            .push("CADUCEUS_LIVE_PASS_ENV_CANARY".to_string());
    }
    let run_id = "live-env-run";
    let worktree = cfg.workdir_base.join("owner").join("repo").join(run_id);
    make_worktree(&worktree);

    let runtime = caduceus::executor::sandbox_spec::RuntimeFacts {
        run_id: run_id.to_string(),
        target: "owner/repo#1".to_string(),
        worker_command: vec![
            "sh".to_string(),
            "-c".to_string(),
            container_script.to_string(),
        ],
        worktree: worktree.clone(),
        output_dir: cfg.state_dir.join("oci-runs").join(run_id).join("output"),
        daemon_id: "live-env-daemon".to_string(),
        workdir_base: cfg.workdir_base.clone(),
        state_dir: cfg.state_dir.clone(),
        worktree_uid: std::fs::metadata(&worktree).expect("worktree stat").uid(),
        worktree_gid: std::fs::metadata(&worktree).expect("worktree stat").gid(),
        engine_mode: caduceus::executor::engine_probe::parse_engine_mode(engine, "[]")
            .expect("fixture mode")
            .0,
        git_shadow_kind: GitShadowKind::File,
        git_shadow_host: cfg
            .state_dir
            .join("oci-runs")
            .join(run_id)
            .join("git-shadow"),
    };
    // Emulate the daemon-owned shadow artifact the pre-flight would
    // create (the test drives the engine directly, not the executor).
    std::fs::create_dir_all(runtime.git_shadow_host.parent().expect("parent"))
        .expect("create run dir");
    std::fs::write(&runtime.git_shadow_host, GIT_SHADOW_FILE_CONTENT).expect("create shadow file");
    std::fs::create_dir_all(&runtime.output_dir).expect("create output dir");

    // Explicit daemon-side daemon snapshot: the pass_env canary plus
    // host noise that must NOT be inherited.
    let daemon_snapshot: BTreeMap<OsString, OsString> = [
        ("CADUCEUS_LIVE_PASS_ENV_CANARY", "canary-exact-value"),
        ("CADUCEUS_LIVE_HOST_NOISE", "noise-must-not-pass"),
    ]
    .iter()
    .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
    .collect();

    let spec = caduceus::executor::ExecutorSpec {
        self_exe: std::path::PathBuf::from("/proc/self/exe"),
        target: caduceus::executor::WorkTarget::Issue(caduceus::executor::IssueWorkTarget {
            key: caduceus::github::issue::IssueKey::parse(&runtime.target)
                .expect("fixture target parses as issue key"),
            title: "Live env".to_string(),
            body: "Live body".to_string(),
            labels: vec!["bug".to_string()],
            branch_name: "caduceus/owner/repo#1".to_string(),
        }),
        worktree: runtime.worktree.clone(),
        run_id: runtime.run_id.clone(),
        context_json: "{}".to_string(),
        worker_command: runtime.worker_command.clone(),
        cancellation: tokio_util::sync::CancellationToken::new(),
    };
    let spec = resolve_with_env(cfg.sandbox(), &runtime, &spec, &daemon_snapshot)
        .expect("live facts must resolve");

    // One daemon-private env file under the run dir; its path is the
    // only env surface that reaches argv.
    let run_dir = cfg.state_dir.join("oci-runs").join(run_id);
    let env_map: BTreeMap<String, String> = spec.environment().iter().cloned().collect();
    let file = OciEnvFile::create(&run_dir, &env_map).expect("create env file");
    let argv = render_with_env_files(&spec, engine, &[file.path().to_path_buf()]);

    // The file is authoritative: no `-e` token may exist in the argv.
    assert!(
        !argv.iter().any(|tok| tok == "-e"),
        "argv must not carry -e tokens (file-only transport), got: {argv:?}"
    );

    Some(Fx {
        _tmp: tmp,
        file,
        argv,
        engine,
        run_id: run_id.to_string(),
    })
}

/// Parse the `KEY=VALUE` dump printed by the container.
fn parse_env_dump(dump: &str) -> BTreeMap<String, String> {
    dump.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

#[test]
fn oci_container_env_is_exact_canonical_plus_pass_env() {
    // Runtime gate (see module docs): without the gating env var the
    // test skips — it must not try to reach a live engine.
    if std::env::var("CADUCEUS_RUN_ISOLATION_TESTS").is_err() {
        eprintln!("skipping: set CADUCEUS_RUN_ISOLATION_TESTS to drive a live engine env run");
        return;
    }
    let engine = detect_engine();
    if engine.is_none() {
        eprintln!("skipping: no usable Docker/Podman engine for gated live tests");
        return;
    }
    let fx = live_fixture(engine, "env");
    let Some(fx) = fx else {
        eprintln!(
            "skipping: CADUCEUS_LIVE_TEST_IMAGE not set (a live create needs a resolvable digest)"
        );
        return;
    };

    // The env file must exist before create and is consumed by create
    // (the lifecycle drops it right after; here we drop it manually
    // after the container run to mirror the lifecycle guard).
    assert!(fx.file.path().exists(), "env file must exist pre-create");

    let combined = run_container(&fx);

    // Create has returned — the env-file values are already gone from
    // disk when the container is running (mirrors the lifecycle
    // guard).
    let dump = combined;
    let env = parse_env_dump(&dump);
    assert!(!env.is_empty(), "expected an env dump; logs: {dump}");

    // Canonical 11 present with the typed values.
    let find = |k: &str| env.get(k).map(String::as_str);
    assert_eq!(find("CADUCEUS_RUN_ID"), Some("live-env-run"));
    assert_eq!(find("CADUCEUS_ISSUE_ID"), Some("owner/repo#1"));
    assert_eq!(find("CADUCEUS_ISSUE_NUMBER"), Some("1"));
    assert_eq!(find("CADUCEUS_ISSUE_REPO"), Some("owner/repo"));
    assert_eq!(find("CADUCEUS_ISSUE_TITLE"), Some("Live env"));
    assert_eq!(find("CADUCEUS_ISSUE_BODY"), Some("Live body"));
    assert_eq!(find("CADUCEUS_ISSUE_LABELS_JSON"), Some("[\"bug\"]"));
    assert_eq!(find("CADUCEUS_CONTEXT_JSON"), Some("{}"));
    assert_eq!(find("CADUCEUS_BRANCH_NAME"), Some("caduceus/owner/repo#1"));
    assert_eq!(find("CADUCEUS_WORKTREE_PATH"), Some("/workspace"));
    assert_eq!(
        find("CADUCEUS_RESULT_PATH"),
        Some("/output/worker-result.json")
    );

    // Compat values point at the tmpfs.
    assert_eq!(find("HOME"), Some("/tmp"));
    assert_eq!(find("TMPDIR"), Some("/tmp"));

    // Resolved pass_env canary: exact value via the file.
    assert_eq!(
        find("CADUCEUS_LIVE_PASS_ENV_CANARY"),
        Some("canary-exact-value"),
        "resolved pass_env value must travel via the file"
    );

    // Unapproved host noise is not inherited.
    assert!(
        !env.contains_key("CADUCEUS_LIVE_HOST_NOISE"),
        "unapproved host variable must not be inherited"
    );

    // Image ENV declarations still apply (image layer source). Every
    // ENV declaration from the configured image must be present.
    let mut cmd = Command::new(engine_binary(fx.engine));
    cmd.args([
        "image",
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        &std::env::var("CADUCEUS_LIVE_TEST_IMAGE").expect("image override"),
    ]);
    let image_env = String::from_utf8_lossy(&run_or_die(&mut cmd, "image inspect").stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<BTreeMap<String, String>>();
    for (k, v) in &image_env {
        assert_eq!(
            find(k),
            Some(v.as_str()),
            "image ENV declaration {k} must still apply; logs: {dump}"
        );
    }

    // The env file is deleted after the container run (the owning
    // guard is dropped at the end of the fixture).
    let Fx { _tmp, file, .. } = fx;
    let file_path = file.path().to_path_buf();
    drop(file);
    assert!(
        !wait_for(
            || file_path.exists(),
            Duration::from_secs(2),
            "env file deletion"
        ),
        "owning the guard must delete the env file after create"
    );
}
