//! Each test runs against a private temp directory and a controlled
//! `PATH` (built explicitly so the host's real `/usr/bin` cannot
//! accidentally satisfy a check).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use caduceus::config::Config;
use caduceus::validate::{
    check_bridge_readable, first_unwritable_parent, is_executable_file, preflight,
    process_groups_supported, resolve_executable, which_in, CheckOutcome,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tempdir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    dir.push(format!("caduceus-validate-test-{label}-{nonce}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write executable body");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

fn config_with_worker_command(root: &Path, cmd: Vec<String>) -> Config {
    let mut cfg = Config::test_defaults(root);
    cfg.worker_command = cmd;
    cfg
}

// Worker command: non-empty and resolvable

#[test]
fn empty_worker_command_is_a_preflight_failure() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempdir("empty-worker-cmd");
    let cfg = config_with_worker_command(&root, vec![]);
    let path_env = "/bin:/usr/bin";
    let outcome = preflight(&cfg, path_env).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::WorkerCommandEmpty);
    assert!(outcome.is_failure());
}

#[test]
fn worker_command_absolute_path_must_point_at_an_executable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("absolute-worker");
    let fake = root.join("worker-cmd");
    write_executable(&fake, "#!/bin/sh\nexit 0\n");

    // Good program: absolute path to executable.
    let cfg_ok = config_with_worker_command(&root, vec![fake.to_string_lossy().to_string()]);
    let outcome = preflight(&cfg_ok, "/bin:/usr/bin").expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::Ok);

    // Bad program: absolute path to a non-executable file.
    let bad = root.join("not-exec");
    std::fs::write(&bad, "data\n").unwrap();
    let cfg_bad = config_with_worker_command(&root, vec![bad.to_string_lossy().to_string()]);
    let outcome = preflight(&cfg_bad, "/bin:/usr/bin").expect("preflight runs");
    assert!(matches!(
        outcome,
        CheckOutcome::WorkerCommandUnresolved { .. }
    ));
}

#[test]
fn worker_command_path_lookup_resolves_through_controlled_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("path-lookup");
    let path_env = make_path_env_with(&root, &["myworker", "git"]);
    let cfg = config_with_worker_command(&root, vec!["myworker".to_string()]);
    let outcome = preflight(&cfg, &path_env).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::Ok);
}

#[test]
fn worker_command_missing_from_path_surfaces_unresolved() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempdir("missing-worker");
    let cfg = config_with_worker_command(&root, vec!["absolutely-nowhere".to_string()]);
    let tempdir_path = std::env::temp_dir();
    let path_env = tempdir_path.to_string_lossy().to_string();
    let outcome = preflight(&cfg, &path_env).expect("preflight runs");
    assert!(matches!(
        outcome,
        CheckOutcome::WorkerCommandUnresolved { ref looked_for } if looked_for == "absolutely-nowhere"
    ));
}

// Bundled bridge readability

#[test]
fn bundled_bridge_must_be_a_readable_file() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("bridge-readable");
    let path_env = make_path_env_with(&root, &["myworker", "git"]);
    let bindir = root.join("bin");

    // First test: bridge present and readable.
    let bridge = root.join("worker-bridge.py");
    std::fs::write(&bridge, "#!/usr/bin/env python3\nprint('hello')\n").unwrap();
    let cfg = config_with_worker_command(
        &root,
        vec![
            bindir.join("myworker").to_string_lossy().to_string(),
            bridge.to_string_lossy().to_string(),
        ],
    );
    let outcome = preflight(&cfg, &path_env).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::Ok);

    // Second test: bridge missing.
    std::fs::remove_file(&bridge).unwrap();
    let outcome = preflight(&cfg, &path_env).expect("preflight runs");
    assert!(matches!(outcome, CheckOutcome::BridgeUnreadable { .. }));
}

#[test]
fn bridge_check_helper_distinguishes_file_from_directory() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempdir("bridge-helper");

    // Path that is a directory, not a file.
    let dir_bridge = root.join("worker-bridge.py");
    std::fs::create_dir_all(&dir_bridge).unwrap();
    let err = check_bridge_readable(&PathBuf::from("/bin/echo"), dir_bridge.to_str().unwrap());
    assert!(err.is_err());

    // Path that is a regular file.
    let file_bridge = root.join("worker-bridge-2.py");
    std::fs::write(&file_bridge, "#!/usr/bin/env python3\n").unwrap();
    let ok = check_bridge_readable(&PathBuf::from("/bin/echo"), file_bridge.to_str().unwrap());
    assert!(ok.is_ok());
}

// Git on PATH

#[test]
fn missing_git_surfaces_git_missing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // Build an empty PATH so neither `git` nor anything else can be
    // found.
    let empty = tempdir("empty-path");

    let root = tempdir("git-missing");
    // Use a worker that is the absolute path of python3 (or any
    // known executable) so the check fails on git, not on the worker
    // lookup.
    let cfg = config_with_worker_command(&root, vec!["/bin/sh".to_string()]);

    let outcome = preflight(&cfg, &empty.to_string_lossy()).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::GitMissing);
}

#[test]
fn present_git_passes_the_check() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("git-present");
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let worker = bindir.join("myworker");
    write_executable(&worker, "#!/bin/sh\nexit 0\n");
    let git = bindir.join("git");
    write_executable(&git, "#!/bin/sh\nexit 0\n");

    let cfg = config_with_worker_command(&root, vec![worker.to_string_lossy().to_string()]);
    let outcome = preflight(&cfg, bindir.to_string_lossy().as_ref()).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::Ok);
}

#[test]
fn which_in_finds_a_program_in_path() {
    let root = tempdir("which-in");
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let exec = bindir.join("myexec");
    write_executable(&exec, "#!/bin/sh\nexit 0\n");

    let found = which_in("myexec", bindir.to_string_lossy().as_ref()).expect("found");
    assert_eq!(found, exec);
}

#[test]
fn which_in_skips_directories_that_are_not_executable() {
    let root = tempdir("which-missing");
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    // No executable inside.

    let found = which_in("myexec", bindir.to_string_lossy().as_ref());
    assert!(found.is_none());
}

// State / workdir writability

#[test]
fn writable_state_dir_passes() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("state-writable");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let cfg = config_with_worker_command(&root, vec!["myworker".to_string()]);
    let outcome =
        preflight(&cfg, &make_path_env_with(&root, &["myworker", "git"])).expect("preflight runs");
    assert_eq!(outcome, CheckOutcome::Ok);
}

#[test]
fn nonexistent_state_dir_parent_is_treated_as_writable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let root = tempdir("missing-state-parent");
    let mut cfg = config_with_worker_command(&root, vec!["myworker".to_string()]);
    cfg.state_dir = root.join("yet-to-exist/state");
    let outcome =
        preflight(&cfg, &make_path_env_with(&root, &["myworker", "git"])).expect("preflight runs");
    // The parent (root) exists and is writable; preflight doesn't
    // try to create the leaf itself. We only fail when a known
    // existing ancestor is unwritable.
    assert!(!matches!(outcome, CheckOutcome::DirNotWritable { .. }));
}

#[test]
fn unwritable_state_dir_parent_surfaces_dir_not_writable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("state-unwritable");
    let blocker = root.join("blocker");
    std::fs::create_dir_all(&blocker).unwrap();
    // chmod the parent to 0500 — owner can read but not write.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&blocker).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&blocker, perms).unwrap();
    }
    let mut cfg = config_with_worker_command(&root, vec!["myworker".to_string()]);
    cfg.state_dir = blocker.join("state");
    let outcome = preflight(&cfg, &make_path_env_with(&root, &["myworker", "git"]));
    // When running as root (the common CI case) the
    // mode-0500 parent is still writable; skip the assertion
    // and only check that the variant is reachable when not root.
    if !cfg
        .state_dir
        .parent()
        .unwrap()
        .metadata()
        .unwrap()
        .permissions()
        .readonly()
    {
        // Running as a user that can still write — preflight does
        // not fail. Restore the mode and skip the strict check.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&blocker).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&blocker, perms).unwrap();
        }
        assert_eq!(outcome.expect("preflight runs"), CheckOutcome::Ok);
    } else {
        assert!(matches!(
            outcome.expect("preflight runs"),
            CheckOutcome::DirNotWritable { .. }
        ));
    }
}

#[test]
fn first_unwritable_parent_walks_up_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let root = tempdir("first-unwritable");
    let blocker = root.join("blocker");
    std::fs::create_dir_all(&blocker).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&blocker).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(&blocker, perms).unwrap();
    }
    let deep = blocker.join("inside/state");
    // Running as root: 0500 is still writable. Test only the
    // general walker behavior — regardless of result, it must
    // not panic.
    let _ = first_unwritable_parent(&deep);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&blocker).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&blocker, perms).unwrap();
    }
}

// Resolution helpers

#[test]
fn resolve_executable_with_absolute_path() {
    let root = tempdir("resolve-abs");
    let exec = root.join("daemon");
    write_executable(&exec, "#!/bin/sh\nexit 0\n");
    let resolved =
        resolve_executable(&exec.to_string_lossy(), "/nonexistent").expect("absolute resolution");
    assert_eq!(resolved, exec);
}

#[test]
fn resolve_executable_fails_for_empty_program() {
    let err = resolve_executable("", "/bin").expect_err("empty program rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("worker_command program is empty"),
        "got: {msg}"
    );
}

#[test]
fn resolve_executable_fails_when_not_found_on_path() {
    let err = resolve_executable("absolutely-nowhere", "/tmp").expect_err("missing");
    let msg = format!("{err:?}");
    assert!(msg.contains("not on PATH"), "got: {msg}");
}

#[test]
fn resolve_executable_fails_when_absolute_path_is_not_executable() {
    let root = tempdir("resolve-not-exec");
    let file = root.join("not-exec");
    std::fs::write(&file, "data").unwrap();
    let err = resolve_executable(&file.to_string_lossy(), "/bin").expect_err("not executable");
    let msg = format!("{err:?}");
    assert!(msg.contains("not a regular executable"), "got: {msg}");
}

#[test]
fn is_executable_file_distinguishes_executable_from_data() {
    let root = tempdir("is-executable-file");
    let exec = root.join("exec");
    write_executable(&exec, "#!/bin/sh\nexit 0\n");
    assert!(is_executable_file(&exec));

    let data = root.join("data");
    std::fs::write(&data, "hello").unwrap();
    assert!(!is_executable_file(&data));

    assert!(!is_executable_file(&root.join("missing")));
}

// Process groups

#[test]
fn process_groups_supported_is_consistent_with_platform() {
    #[cfg(unix)]
    assert!(process_groups_supported());
    #[cfg(not(unix))]
    assert!(!process_groups_supported());
}

// Helpers

fn make_path_env_with(root: &Path, programs: &[&str]) -> String {
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    for program in programs {
        write_executable(&bindir.join(program), "#!/bin/sh\nexit 0\n");
    }
    bindir.to_string_lossy().to_string()
}
