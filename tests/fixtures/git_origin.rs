//! Disposable local Git origin fixture.
//!
//! Used by Phase 2.6 ("Harden every Git invocation") and Phase 7.5
//! ("Release canary") to exercise the daemon's `git fetch`, `git
//! push`, and `git clone` paths against a real on-disk bare
//! repository without ever touching github.com. The bare repo
//! lives in a `tempfile::TempDir`, which means the test process
//! removes it on drop — there's no manual cleanup.
//!
//! The contract surface from `CONTRACTS.md`:
//!
//! * **CI-002** — fixtures MUST be hermetic and MUST NOT
//!   require production credentials. `LocalOrigin` honours both:
//!   the URL is `file://`, no SSH keys are configured, and no
//!   GitHub credentials are read.
//! * **RUN-001 / RUN-004** — the daemon's `GitRunner` must
//!   scrub credential env vars before invoking git. The
//!   `LocalOrigin::scrubbed_env` helper returns the env a real
//!   runner would build, so tests can assert the scrubber does
//!   not break legitimate local-remote operations.
//!
//! `LocalOrigin` is the v1.0 reusable form of the helper that
//! `tests/repo/worktree_create_test.rs` ships inline. Tests that
//! need a bare repo, a working clone, or a push/fetch round trip
//! should construct one `LocalOrigin` and call the helpers.
//!
//! Each helper test binary builds in isolation, so individual
//! methods (e.g. `receive_push`) only appear "used" in the
//! binary that imports them. Same rationale as `github.rs`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Owns a temporary bare `git init --bare` repo with an empty
/// `main` branch. Clone from `uri()`, push to it via
/// `uri()`/`file://`, fetch from it — all paths exercise real
/// `git` subprocesses the daemon would invoke in production.
pub struct LocalOrigin {
    _dir: TempDir,
    bare: PathBuf,
    /// The OID of `main`'s tip after `init`. Updated each time a
    /// test pushes a new commit so callers can assert the head
    /// moved.
    head_oid: String,
}

impl LocalOrigin {
    /// Build a fresh bare repository under a unique tempdir and
    /// write one empty commit onto `main`. Returns once the bare
    /// repo is ready to clone or fetch from.
    pub fn init(label: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix(&format!("caduceus-origin-{label}-"))
            .tempdir()
            .expect("tempdir create");
        let bare = dir.path().join("origin.git");
        run(Command::new("git").arg("init").arg("--bare").arg(&bare));

        // Force the bare repo's default branch to `main` so the
        // daemon's `refs/remotes/origin/HEAD` lookup works the
        // same way it does against github.com.
        run(Command::new("git").current_dir(&bare).args([
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]));

        let tree_oid = String::from_utf8(
            run_output(Command::new("git").current_dir(&bare).args([
                "hash-object",
                "-w",
                "-t",
                "tree",
                "/dev/null",
            ]))
            .stdout,
        )
        .expect("hash-object utf8")
        .trim()
        .to_string();
        let commit_oid = String::from_utf8(
            run_output(Command::new("git").current_dir(&bare).args([
                "commit-tree",
                &tree_oid,
                "-m",
                "initial",
            ]))
            .stdout,
        )
        .expect("commit-tree utf8")
        .trim()
        .to_string();
        run(Command::new("git").current_dir(&bare).args([
            "update-ref",
            "refs/heads/main",
            &commit_oid,
        ]));

        Self {
            _dir: dir,
            bare,
            head_oid: commit_oid,
        }
    }

    /// Path to the bare repository directory on disk. Tests
    /// that need to inspect `objects/` or `refs/` directly reach
    /// for this.
    pub fn path(&self) -> &Path {
        &self.bare
    }

    /// `file://` URL the daemon should put in `remote.origin.url`.
    /// The host segment is empty (per RFC 8089 §E.2.1 for
    /// `file://` URLs without a host), which is exactly what
    /// `tests/repo/worktree_create_test.rs` does today.
    pub fn uri(&self) -> String {
        format!("file://{}", self.bare.display())
    }

    /// Current OID on `main` in the bare repo. Updated by
    /// [`Self::push_commit`] and [`Self::receive_push`].
    pub fn head_oid(&self) -> &str {
        &self.head_oid
    }

    /// Clone the bare repo into `dest` and return the OID of the
    /// working clone's `HEAD`. The clone uses the bare repo's
    /// `file://` URL as `remote.origin.url` so the daemon's
    /// `validate_origin_host` sees the same shape it sees in
    /// production.
    pub fn clone_into(&self, dest: &Path) -> String {
        run(Command::new("git")
            .arg("clone")
            .arg("-b")
            .arg("main")
            .arg(self.uri())
            .arg(dest));
        self.head_oid_at(dest)
    }

    /// Convenience helper: write `content` to a new file in
    /// `working`, commit it on the current branch, push to the
    /// bare origin, and update `head_oid`. Returns the new OID.
    ///
    /// `committer_name` and `committer_email` are passed as the
    /// `-c user.name=... -c user.email=...` overrides so the
    /// fixture doesn't need a global git identity on the test
    /// host. The values are the same defaults the daemon uses
    /// when seeding a worktree (`DEFAULT_GIT_USER_NAME` and
    /// `DEFAULT_GIT_USER_EMAIL` in `src/worktree.rs`).
    pub fn push_commit(
        &mut self,
        working: &Path,
        file: &Path,
        content: &str,
        commit_message: &str,
    ) -> String {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("create file parent");
        }
        std::fs::write(file, content).expect("write content");
        let file_arg = file
            .strip_prefix(working)
            .unwrap_or(file)
            .to_string_lossy()
            .into_owned();
        run(Command::new("git")
            .current_dir(working)
            .args(["add", "--", &file_arg]));
        run(Command::new("git").current_dir(working).args([
            "-c",
            "user.name=Caduceus Test",
            "-c",
            "user.email=test@caduceus.local",
            "commit",
            "-m",
            commit_message,
        ]));
        let new_oid = self.head_oid_at(working);
        run(Command::new("git").current_dir(working).args([
            "push",
            "origin",
            "HEAD:refs/heads/main",
        ]));
        self.head_oid = new_oid.clone();
        new_oid
    }

    /// Push the working clone's `HEAD` to the bare origin without
    /// creating a new commit. Returns the OID that was pushed.
    /// Used by tests that want to drive the push path with
    /// pre-existing history.
    pub fn receive_push(&mut self, working: &Path) -> String {
        let oid = self.head_oid_at(working);
        run(Command::new("git").current_dir(working).args([
            "push",
            "origin",
            "HEAD:refs/heads/main",
        ]));
        self.head_oid = oid.clone();
        oid
    }

    /// Read the OID of the working clone's HEAD. Pure read;
    /// doesn't touch the bare repo.
    pub fn head_oid_at(&self, working: &Path) -> String {
        String::from_utf8(
            run_output(
                Command::new("git")
                    .current_dir(working)
                    .args(["rev-parse", "HEAD"]),
            )
            .stdout,
        )
        .expect("rev-parse utf8")
        .trim()
        .to_string()
    }

    /// Number of commits on `main` in the bare repo. Used by
    /// self-tests that want to assert "the push bumped the
    /// commit count by exactly one" without comparing OIDs.
    pub fn commit_count(&self) -> usize {
        let out = run_output(Command::new("git").current_dir(&self.bare).args([
            "rev-list",
            "--count",
            "refs/heads/main",
        ]))
        .stdout;
        String::from_utf8(out)
            .expect("rev-list utf8")
            .trim()
            .parse::<usize>()
            .expect("rev-list count is integer")
    }

    /// Run an arbitrary `git` command in the bare repo. Returned
    /// for tests that need a quick side effect (create a ref,
    /// write a config value) without having to shell out to
    /// `Command::new("git")` themselves. Errors panic with the
    /// failing command's stderr.
    pub fn run_in_bare(&self, args: &[&str]) {
        run(Command::new("git").current_dir(&self.bare).args(args));
    }
}

fn run(cmd: &mut Command) {
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("git spawn");
    if !status.success() {
        let stderr = cmd
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stderr).ok())
            .unwrap_or_default();
        panic!("git command failed ({}): {stderr}", status);
    }
}

fn run_output(cmd: &mut Command) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git spawn")
}
