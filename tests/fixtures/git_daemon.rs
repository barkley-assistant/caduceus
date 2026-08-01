//! Disposable `git daemon` origin fixture.
//!
//! Serves a bare repo over the `git://` protocol on 127.0.0.1 so
//! the daemon's `git fetch` / `git push` exercise real git
//! subprocesses without github.com. The host `127.0.0.1` matches
//! the wiremock `api_base` host, so `validate_origin_host` accepts
//! the origin (this is why a `git daemon` is needed rather than a
//! `file://` `LocalOrigin`).
//!
//! Shared by `tests/daemon/per_claim_test.rs` and
//! `tests/integration/release_canary_test.rs`. Each consumer wires
//! the fixture in via `#[path = "fixtures/mod.rs"] mod fixtures;`
//! and imports what it uses. The `#![allow(dead_code)]` covers
//! methods only one consumer binary exercises — same rationale as
//! `github.rs` and `git_origin.rs`.

#![allow(dead_code)]

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

/// Owns a `git daemon` subprocess serving a bare repo at
/// `git://127.0.0.1:<port>/<owner>/<repo>`. The child is killed on
/// drop before the tempdir is removed.
pub struct GitDaemon {
    _root: TempDir,
    bare: PathBuf,
    port: u16,
    child: std::process::Child,
    owner: String,
    repo: String,
}

impl GitDaemon {
    /// Create the bare repo, seed an empty commit on `main`, and start
    /// `git daemon` on a free 127.0.0.1 port. `owner`/`repo` form the
    /// `git://` path the daemon's clone should use.
    ///
    /// If the daemon fails to become ready, the spawned child is killed
    /// before this function panics, so no orphan `git daemon` leaks.
    pub fn start(label: &str, owner: &str, repo: &str) -> Self {
        let root = TempDir::with_prefix(format!("caduceus-origin-{label}-")).expect("origin");
        let gitroot = root.path().join("gitroot");
        let bare = gitroot.join(owner).join(repo);
        fs::create_dir_all(&bare).expect("mkdir bare");
        init_bare_with_empty_main(&bare);

        // Enable push over the git protocol.
        git_in(&bare, &["config", "daemon.receivepack", "true"]);
        git_in(&bare, &["config", "daemon.uploadarch", "true"]);

        let port = free_port_127();
        let log_path = root.path().join("git-daemon.log");
        let log = fs::File::create(&log_path).expect("create daemon log");
        // Guard the child so a readiness panic kills it instead of
        // orphaning a `git daemon` that holds no TempDir owner yet.
        let mut child = {
            struct KillOnDrop(Option<std::process::Child>);
            impl Drop for KillOnDrop {
                fn drop(&mut self) {
                    if let Some(mut c) = self.0.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            }
            KillOnDrop(Some(
                Command::new("git")
                    .args([
                        "daemon",
                        "--reuseaddr",
                        "--listen=127.0.0.1",
                        &format!("--port={port}"),
                        &format!("--base-path={}", gitroot.display()),
                        "--export-all",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone().expect("clone log")))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap_or_else(|e| panic!("spawn git daemon: {e}")),
            ))
        };

        // Wait for the daemon to accept a connection so the clone below
        // does not race the bind. Also confirm the child has not already
        // exited (a bind error would make it die before readiness).
        wait_for_port_127(port, Duration::from_secs(5));
        match child.0.as_mut().unwrap().try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                let log = fs::read_to_string(&log_path).unwrap_or_default();
                panic!("git daemon exited before becoming ready (status {status}); log:\n{log}");
            }
            Err(e) => panic!("git daemon readiness try_wait: {e}"),
        }

        let child = child.0.take().expect("child present after readiness");
        Self {
            _root: root,
            bare,
            port,
            child,
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }

    /// `git://127.0.0.1:<port>/<owner>/<repo>` — the URL the daemon's
    /// clone should use as `remote.origin.url`.
    pub fn uri(&self) -> String {
        format!("git://127.0.0.1:{}/{}/{}", self.port, self.owner, self.repo)
    }

    /// Path to the bare repository directory on disk.
    pub fn path(&self) -> &Path {
        &self.bare
    }

    /// Number of refs under `refs/heads/` in the bare repo. Used to
    /// prove a scheduled tick pushed exactly one new branch.
    pub fn head_refs(&self) -> Vec<String> {
        let out = Command::new("git")
            .current_dir(&self.bare)
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .output()
            .expect("for-each-ref");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    /// Count commits on `refs/heads/main`.
    pub fn main_commit_count(&self) -> usize {
        rev_list_count(&self.bare, "refs/heads/main")
    }

    /// Count commits on `branch` that are not on `main` (the new work
    /// a scheduled tick pushed).
    pub fn branch_commits_beyond_main(&self, branch: &str) -> usize {
        let out = Command::new("git")
            .current_dir(&self.bare)
            .args(["rev-list", "--count", branch, "^refs/heads/main"])
            .output()
            .expect("rev-list count");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0)
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rev_list_count(bare: &Path, refspec: &str) -> usize {
    let out = Command::new("git")
        .current_dir(bare)
        .args(["rev-list", "--count", refspec])
        .output()
        .expect("rev-list --count");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or(0)
}

/// `git init --bare` at `path`, then seed an empty commit on `main`.
pub fn init_bare_with_empty_main(path: &Path) {
    git_in(path, &["init", "--bare"]);
    git_in(path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let tree = String::from_utf8(
        Command::new("git")
            .current_dir(path)
            .args(["hash-object", "-w", "-t", "tree", "/dev/null"])
            .output()
            .expect("hash-object")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    let commit = String::from_utf8(
        Command::new("git")
            .current_dir(path)
            .args(["commit-tree", &tree, "-m", "initial"])
            .output()
            .expect("commit-tree")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    git_in(path, &["update-ref", "refs/heads/main", &commit]);
}

pub fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git spawn");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "git {:?} in {} failed ({}); stderr:\n{}",
            args,
            dir.display(),
            output.status,
            stderr
        );
    }
}

/// Grab a free TCP port on 127.0.0.1 by briefly binding, then drop the
/// listener so `git daemon` can take it.
pub fn free_port_127() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Poll until a TCP connection to `127.0.0.1:port` succeeds (the daemon
/// is accepting). Panics after `timeout`.
pub fn wait_for_port_127(port: u16, timeout: Duration) {
    let deadline = SystemTime::now() + timeout;
    while SystemTime::now() < deadline {
        if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("git daemon did not accept on 127.0.0.1:{port} within {timeout:?}");
}

/// Clone the bare origin into `workdir_base/<owner>/<repo>` so the
/// daemon's `find_main_clone` discovers it with `remote.origin.url` =
/// the origin's `git://` URI.
pub fn clone_main(workdir_base: &Path, origin_uri: &str, owner: &str, repo: &str) -> PathBuf {
    let main_path = workdir_base.join(owner).join(repo);
    fs::create_dir_all(workdir_base).expect("mkdir workdir_base");
    let output = Command::new("git")
        .args([
            "clone",
            "-b",
            "main",
            origin_uri,
            &main_path.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git clone");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "git clone of disposable origin failed ({}); stderr:\n{}",
            output.status, stderr
        );
    }
    main_path
}

/// Spawn `cmd` and wait up to `timeout`, returning `(exit_code, stdout,
/// stderr)`. Uses `SystemTime` for the deadline. Panics if the process
/// does not exit in time (after killing it).
pub fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    label: &str,
) -> (i32, String, String) {
    use std::io::Read;
    let mut child = cmd.spawn().unwrap_or_else(|e| panic!("spawn {label}: {e}"));
    let deadline = SystemTime::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if SystemTime::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{label} did not exit within {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("{label} try_wait: {e}"),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    (status.code().unwrap_or(-1), stdout, stderr)
}
