//! Dynamic identity resolution tests (design D2/D3).
//!
//! Two layers, both pure and ungated:
//!
//! 1. `parse_engine_mode` — the engine-mode decision logic pinned
//!    against the synthetic `docker info` / `podman info` stdout
//!    shapes, including the fail-closed garbage case.
//! 2. The 4-case identity matrix — `resolve` + `render` from
//!    synthetic ownership facts with a non-1000 owner (`4242:4242`),
//!    asserting the rendered argv per engine×mode and that a literal
//!    `1000:1000` is never emitted.

use caduceus::executor::sandbox_renderer::render;
use caduceus::executor::sandbox_spec::{resolve, EngineMode, SandboxEngine};
use caduceus::infra::config::Config;
use caduceus::infra::error::CaduceusError;

mod support;

/// A config with the given engine, plus the resolved spec + rendered
/// argv for the default fixture facts (owner `4242:4242`).
fn rendered_for(engine: SandboxEngine, mode: EngineMode) -> (Config, Vec<String>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::test_defaults(tmp.path());
    cfg.sandbox.as_mut().expect("sandbox").engine = engine;
    let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
    let mut runtime = support::runtime_facts(&cfg, "run-001", &worktree);
    runtime.engine_mode = mode;
    let spec = resolve(cfg.sandbox(), &runtime).expect("facts must resolve");
    (cfg, render(&spec, engine))
}

// ---------------------------------------------------------------------------
// 4-case identity matrix (design D2)
// ---------------------------------------------------------------------------

/// Docker rootful ⇒ `--user <owner-uid>:<owner-gid>`, no userns.
#[test]
fn docker_rootful_uses_owner_identity() {
    let (cfg, argv) = rendered_for(SandboxEngine::Docker, EngineMode::Rootful);
    let user_pos = argv.iter().position(|a| a == "--user").expect("--user");
    assert_eq!(argv[user_pos + 1], "4242:4242");
    assert!(!argv.contains(&"--userns".to_string()));
    let _ = cfg;
}

/// Docker rootless ⇒ no `--user` at all: container root maps to the
/// unprivileged engine user via the rootless user namespace, granting
/// no host-root privilege. No userns flag either.
#[test]
fn docker_rootless_emits_no_user() {
    let (_, argv) = rendered_for(SandboxEngine::Docker, EngineMode::Rootless);
    assert!(
        !argv.contains(&"--user".to_string()),
        "rootless docker must not emit --user, got: {argv:?}"
    );
    assert!(!argv.contains(&"--userns".to_string()));
}

/// Podman rootless ⇒ plain `--userns keep-id` (no uid=/gid= mapping)
/// and no `--user`: in-container identity = the invoking user = the
/// daemon user = the worktree owner.
#[test]
fn podman_rootless_uses_plain_keep_id() {
    let (_, argv) = rendered_for(SandboxEngine::Podman, EngineMode::Rootless);
    let pos = argv.iter().position(|a| a == "--userns").expect("--userns");
    assert_eq!(argv[pos + 1], "keep-id");
    assert!(
        !argv.contains(&"--user".to_string()),
        "podman rootless must not emit --user, got: {argv:?}"
    );
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("uid=") || a.contains("gid=")),
        "keep-id must be plain (no hard-coded mapping), got: {argv:?}"
    );
}

/// Podman rootful ⇒ the rootful rule: `--user U:G`, no `--userns`.
#[test]
fn podman_rootful_follows_rootful_rule() {
    let (_, argv) = rendered_for(SandboxEngine::Podman, EngineMode::Rootful);
    let user_pos = argv.iter().position(|a| a == "--user").expect("--user");
    assert_eq!(argv[user_pos + 1], "4242:4242");
    assert!(!argv.contains(&"--userns".to_string()));
}

/// Identity never assumes UID 1000: for every supported engine×mode,
/// the hard-coded `1000:1000` value is never emitted anywhere in the
/// argv, and the resolved uid/gid equal the synthetic owner facts.
#[test]
fn identity_never_assumes_1000() {
    let cases = [
        (SandboxEngine::Docker, EngineMode::Rootful),
        (SandboxEngine::Docker, EngineMode::Rootless),
        (SandboxEngine::Podman, EngineMode::Rootless),
        (SandboxEngine::Podman, EngineMode::Rootful),
    ];
    for (engine, mode) in cases {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut cfg = Config::test_defaults(tmp.path());
        cfg.sandbox.as_mut().expect("sandbox").engine = engine;
        let worktree = cfg.workdir_base.join("owner").join("repo").join("run-001");
        let runtime = support::runtime_facts(&cfg, "run-001", &worktree);
        let spec = resolve(cfg.sandbox(), &runtime).expect("must resolve");
        assert_eq!(spec.identity().uid, 4242, "{engine:?}/{mode:?}");
        assert_eq!(spec.identity().gid, 4242, "{engine:?}/{mode:?}");

        let argv = render(&spec, engine);
        assert!(
            !argv.iter().any(|a| a.contains("1000:1000")),
            "{engine:?}/{mode:?} must never emit the hard-coded 1000:1000, got: {argv:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Engine-mode parsing (design D2)
// ---------------------------------------------------------------------------

fn parse(engine: SandboxEngine, stdout: &str) -> Result<(EngineMode, bool), CaduceusError> {
    caduceus::executor::engine_probe::parse_engine_mode(engine, stdout)
}

/// Docker rootful shapes: plain security options, and the empty list
/// some Docker versions print.
#[test]
fn parse_docker_rootful() {
    for stdout in [
        "[name=apparmor name=seccomp,profile=builtin]",
        "[name=seccomp,profile=builtin name=cgroupns]",
        "[]",
    ] {
        let (mode, userns_remap) = parse(SandboxEngine::Docker, stdout)
            .unwrap_or_else(|e| panic!("must parse {stdout:?}: {e:?}"));
        assert_eq!(mode, EngineMode::Rootful, "{stdout:?}");
        assert!(!userns_remap, "{stdout:?}");
    }
}

/// Docker rootless: `name=rootless` in the security options.
#[test]
fn parse_docker_rootless() {
    for stdout in [
        "[name=rootless]",
        "[name=rootless name=seccomp,profile=builtin]",
    ] {
        let (mode, userns_remap) = parse(SandboxEngine::Docker, stdout)
            .unwrap_or_else(|e| panic!("must parse {stdout:?}: {e:?}"));
        assert_eq!(mode, EngineMode::Rootless, "{stdout:?}");
        assert!(!userns_remap, "{stdout:?}");
    }
}

/// Docker rootful with userns-remap (with and without `=`-value):
/// rootful + userns_remap flag set — the canonical unsupported case.
#[test]
fn parse_docker_userns_remap() {
    for stdout in [
        "[name=userns-remap]",
        "[name=userns-remap,value=default]",
        "[name=apparmor name=userns-remap,value=custom]",
    ] {
        let (mode, userns_remap) = parse(SandboxEngine::Docker, stdout)
            .unwrap_or_else(|e| panic!("must parse {stdout:?}: {e:?}"));
        assert_eq!(mode, EngineMode::Rootful, "{stdout:?}");
        assert!(userns_remap, "{stdout:?}");
    }
}

/// Podman rootless/rootful: `{{.Host.Security.Rootless}}` booleans.
#[test]
fn parse_podman_boolean() {
    let (mode, userns_remap) = parse(SandboxEngine::Podman, "true").expect("must parse");
    assert_eq!(mode, EngineMode::Rootless);
    assert!(!userns_remap);

    let (mode, userns_remap) = parse(SandboxEngine::Podman, "false\n").expect("must parse");
    assert_eq!(mode, EngineMode::Rootful);
    assert!(!userns_remap);
}

/// Garbage / unparseable output is fail-closed: typed refusal with
/// `mode: None` — the daemon never guesses a mode.
#[test]
fn parse_garbage_is_fail_closed() {
    for (engine, stdout) in [
        (SandboxEngine::Docker, "not-a-security-options-list"),
        (SandboxEngine::Docker, ""),
        (SandboxEngine::Podman, "maybe"),
        (SandboxEngine::Podman, ""),
        (SandboxEngine::Podman, "[name=rootless]"),
    ] {
        match parse(engine, stdout) {
            Err(CaduceusError::OciIdentityUnsupported {
                mode: None, reason, ..
            }) => {
                assert!(
                    !reason.is_empty(),
                    "refusal must carry a reason, engine {engine:?} stdout {stdout:?}"
                );
            }
            other => panic!(
                "expected fail-closed OciIdentityUnsupported for {engine:?} \
                 stdout {stdout:?}, got: {other:?}"
            ),
        }
    }
}
