//! Task 4.5 acceptance tests for the worktree GC.
//!
//! These tests exercise the contract in `CONTRACTS.md` and
//! `planning/caduceus-v0.1/tasks/4.5-implement-safe-worktree-gc.md`:
//!
//! * Multiple repositories: each is enumerated independently.
//! * Old active worktrees are retained (younger than the
//!   threshold).
//! * Symlinks are rejected.
//! * Unregistered orphan directories are removed when
//!   they are old, inactive, and not symlinks.
//! * `--dry-run` produces no mutations.
//! * `git worktree prune` clears stale registration metadata.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use caduceus::config::{Config, LoadContext, RawConfig};
use caduceus::issue::IssueKey;
use caduceus::worktree::{create as create_worktree, gc, GitRunner, RepositoryInfo, Worktree};
use chrono::Utc;

fn info_for(path: &Path) -> RepositoryInfo {
    RepositoryInfo {
        path: path.to_path_buf(),
        base_branch: "main".to_string(),
        remote_url: "file://localhost/tmp".to_string(),
    }
}

fn drive<F: std::future::Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(f)
}

fn make_worktree(cfg: &Config, key: IssueKey, run_id: &str) -> Worktree {
    let info = info_for(&cfg.workdir_base.join(&key.owner).join(&key.repo));
    let runner = Arc::new(GitRunner::new(cfg));
    drive(create_worktree(cfg, &runner, &info, &key, run_id)).expect("create worktree")
}

fn sh_out(dir: &Path, op: &str, args: &[&str]) -> String {
    let out = Command::new(op)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|err| panic!("{op} {args:?}: {err}"));
    assert!(
        out.status.success(),
        "{op} {args:?} failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_bare(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    let _ = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .arg(dir)
        .output()
        .expect("git init");
}

fn init_clone(bare: &Path, clone: &Path) {
    fs::create_dir_all(clone).expect("mkdir");
    let out = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(bare)
        .arg(clone)
        .output()
        .expect("git clone");
    assert!(out.status.success(), "clone failed");
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Tester"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["checkout", "-q", "-b", "main"])
        .current_dir(clone)
        .output();
    fs::write(clone.join("README.md"), "base\n").expect("write");
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "init"])
        .current_dir(clone)
        .output();
    let _ = Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(clone)
        .output();
}

/// Build a `Config` whose `workdir_base` is the test's
/// `base.path()` (the parent of the `state/` dir). This
/// anchors the daemon's path math to where
/// `init_clone` actually writes the clone, so
/// `info_for(&cfg.workdir_base/<owner>/<repo>)` resolves to
/// a real on-disk path. The config is also written to a
/// `caduceus.toml` so the GC's `Config::load()` can read
/// the watched-repos list when it walks the heartbeat
/// path-to-worktree mapping.
fn gc_test_config(base: &Path, watched: Vec<String>) -> Config {
    let state_dir = base.join("state");
    fs::create_dir_all(state_dir.join("claims")).expect("claims");
    fs::create_dir_all(state_dir.join("runs")).expect("runs");
    let raw = RawConfig {
        watched_repos: Some(watched.clone()),
        worker_command: Some(vec!["/bin/true".to_string()]),
        state_dir: Some(state_dir),
        workdir_base: Some(base.to_path_buf()),
        ..Default::default()
    };
    let ctx = LoadContext {
        plugin_root: Some(base.to_path_buf()),
        ..Default::default()
    };
    let cfg = Config::from_raw(raw, &ctx).expect("config");
    // Write a caduceus.yaml so `Config::load_from` can find
    // it during the GC run. The daemon uses Hermes
    // resolution by default, but the GC's heartbeat walker
    // falls back to `$CADUCEUS_CONFIG` which we set below
    // for the test process.
    let toml_path = base.join("caduceus.yaml");
    let toml_text = format!(
        "state_dir: \"{}\"\nworkdir_base: \"{}\"\nworker_command:\n  - \"/bin/true\"\nwatched_repos:\n  - \"{}\"\n",
        cfg.state_dir.display(),
        cfg.workdir_base.display(),
        watched[0]
    );
    fs::write(&toml_path, toml_text).expect("write config");
    cfg
}

fn backdate_to_older_than(path: &Path, days: i64) {
    // The worktree path's mtime is what the GC compares
    // against `older_than_days`. We set the mtime to `days`
    // before now using `touch -t`. The `touch -t` format is
    // `[[CC]YY]MMDDhhmm[.ss]` — no `T` separator.
    let past = Utc::now() - chrono::Duration::days(days);
    let stamp = past.format("%Y%m%d%H%M.%S").to_string();
    let out = Command::new("touch")
        .args(["-t", &stamp])
        .arg(path)
        .output()
        .expect("touch");
    assert!(
        out.status.success(),
        "touch -t failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn gc_removes_old_unreferenced_worktree() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 1,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let run_id = "run-old".to_string();
    let wt = make_worktree(&cfg, key.clone(), &run_id);
    backdate_to_older_than(&wt.path, 30);
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 1, "one worktree should be removed");
    assert!(!wt.path.exists(), "worktree path should be gone");
}

#[test]
fn gc_retains_recent_worktrees() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 2,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-recent");
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 0);
    assert!(wt.path.exists(), "fresh worktree should remain");
}

#[test]
fn gc_retains_worktree_with_active_claim() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 3,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-claimed");
    backdate_to_older_than(&wt.path, 30);
    // Write a claim file referencing the worktree path so
    // the GC treats it as in-use.
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.display_key().as_bytes());
    let digest = hex(&h.finalize());
    let claims_dir = base.path().join("state").join("claims");
    let claim_path = claims_dir.join(format!("{digest}.claim"));
    let body = serde_json::json!({
        "version": 1,
        "key": key,
        "run_id": "run-claimed",
        "pid": 4_000_000_u32,
        "process_start_identity": "<boot>:0",
        "started_at": Utc::now(),
        "worktree_path": wt.path,
    });
    fs::write(&claim_path, serde_json::to_vec(&body).unwrap()).expect("write claim");

    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 0, "an in-use worktree must not be removed");
    assert!(wt.path.exists());
}

#[test]
fn gc_retains_worktree_with_fresh_heartbeat() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 4,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-hb");
    backdate_to_older_than(&wt.path, 30);
    // A fresh heartbeat under runs/ for the same run id.
    // The GC infers the worktree path from the run_id by
    // looking for `<workdir_base>/<owner>/<repo>/.worktrees/run-hb`.
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::File::options()
        .create(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(
            base.path()
                .join("state")
                .join("runs")
                .join("run-hb.heartbeat"),
        )
        .expect("heartbeat");

    // The GC reads `Config::load()` to discover the
    // watched-repos list. Point `CADUCEUS_CONFIG` at the
    // file we wrote in `gc_test_config` so the lookup
    // succeeds.
    let toml = base.path().join("caduceus.yaml");
    // SAFETY: tests run single-threaded; setting an env
    // var here does not race with any other test code.
    unsafe { std::env::set_var("CADUCEUS_CONFIG", &toml) };
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    unsafe { std::env::remove_var("CADUCEUS_CONFIG") };
    assert_eq!(removed, 0, "fresh heartbeat must keep worktree");
    assert!(wt.path.exists());
}

#[test]
fn gc_rejects_symlink_orphan() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    // Build a symlink in .worktrees/ that points to an
    // existing, old directory.
    let worktrees_dir = clone.join(".worktrees");
    fs::create_dir_all(&worktrees_dir).expect("worktrees");
    let target = base.path().join("target");
    fs::create_dir_all(&target).expect("target");
    backdate_to_older_than(&target, 30);
    let link = worktrees_dir.join("evil-link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 0, "symlinks must not be removed");
    assert!(link.exists(), "symlink must remain on disk");
}

#[test]
fn gc_removes_unregistered_orphan_directory() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    // Create an unregistered, old orphan under .worktrees/.
    let worktrees_dir = clone.join(".worktrees");
    fs::create_dir_all(&worktrees_dir).expect("worktrees");
    let orphan = worktrees_dir.join("unregistered");
    fs::create_dir_all(&orphan).expect("orphan");
    backdate_to_older_than(&orphan, 30);
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 1);
    assert!(!orphan.exists(), "orphan should be removed");
}

#[test]
fn gc_dry_run_does_not_mutate() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 7,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-dry");
    backdate_to_older_than(&wt.path, 30);
    let removed = drive(gc(&cfg, 7, true)).expect("dry-run gc");
    assert_eq!(removed, 0, "dry-run returns 0");
    assert!(wt.path.exists(), "dry-run must not remove the worktree");
}

#[test]
fn gc_enumerates_multiple_repositories() {
    let base = tempfile::tempdir().expect("base");
    let mut watched = Vec::new();
    for (owner, name) in [("owner-a", "a"), ("owner-b", "b")] {
        let bare = base.path().join(format!("{owner}.git"));
        let clone = base.path().join(owner).join(name);
        fs::create_dir_all(&clone).expect("clone dir");
        init_bare(&bare);
        init_clone(&bare, &clone);
        watched.push(format!("{owner}/{name}"));
    }
    let cfg = gc_test_config(base.path(), watched);

    let mut created = Vec::new();
    for (owner, name, num) in [("owner-a", "a", 100), ("owner-b", "b", 200)] {
        let key = IssueKey {
            owner: owner.to_string(),
            repo: name.to_string(),
            number: num,
        };
        let wt = make_worktree(&cfg, key.clone(), "run-x");
        backdate_to_older_than(&wt.path, 30);
        created.push(wt);
    }

    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 2, "both worktrees should be removed");
    for wt in &created {
        assert!(!wt.path.exists());
    }
}

#[test]
fn gc_leaves_files_outside_worktrees_dir_untouched() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    // Put a decoy directory at the root of the clone.
    let decoy = clone.join("decoy");
    fs::create_dir_all(&decoy).expect("decoy");
    fs::write(decoy.join("file"), "x").expect("write");
    backdate_to_older_than(&decoy, 30);

    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 0);
    assert!(decoy.exists(), "decoy outside .worktrees/ must remain");
}

#[test]
fn gc_unknown_nested_branch_name_is_irrelevant() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 11,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-deep");
    backdate_to_older_than(&wt.path, 30);
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 1);
}

#[test]
fn gc_skips_missing_main_clone() {
    let base = tempfile::tempdir().expect("base");
    let cfg = gc_test_config(base.path(), vec!["ghost/repo".to_string()]);
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 0);
}

#[test]
fn gc_uses_remove_for_safe_unregistration() {
    let base = tempfile::tempdir().expect("base");
    let bare = base.path().join("owner.git");
    let clone = base.path().join("owner").join("r");
    fs::create_dir_all(&clone).expect("clone dir");

    init_bare(&bare);
    init_clone(&bare, &clone);

    let key = IssueKey {
        owner: "owner".to_string(),
        repo: "r".to_string(),
        number: 12,
    };
    let cfg = gc_test_config(base.path(), vec!["owner/r".to_string()]);
    let wt = make_worktree(&cfg, key.clone(), "run-meta");
    backdate_to_older_than(&wt.path, 30);
    let pre = sh_out(&clone, "git", &["worktree", "list", "--porcelain"]);
    assert!(pre.contains(wt.path.to_str().unwrap()));
    let removed = drive(gc(&cfg, 7, false)).expect("gc");
    assert_eq!(removed, 1);
    let post = sh_out(&clone, "git", &["worktree", "list", "--porcelain"]);
    assert!(!post.contains(wt.path.to_str().unwrap()));
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
