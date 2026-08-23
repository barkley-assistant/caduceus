use super::{runner_inner_cfg, GitOutput, GitRunner, RepositoryInfo};

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use tracing::info;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{CaduceusError, CaduceusResult};

/// Outcome of creating one daemon-owned worktree + branch. The
/// daemon owns the branch name (invariant #5) and the canonical
/// worktree path; worker code never selects a ref or a path.
#[derive(Clone, Debug)]
pub struct Worktree {
    /// Issue this worktree is provisioned for. The daemon
    /// re-exports `display_key()` so callers can derive stable
    /// filenames without reaching into the issue module.
    pub issue: IssueKey,
    /// Run ID, used as the worktree directory basename and (in
    /// lowercase form) as the branch suffix.
    pub run_id: String,
    /// Daemon-owned branch name of the form
    /// `automation/issue-<number>-<lowercase-run-id>`.
    pub branch_name: String,
    /// Absolute worktree path `<repo>/.worktrees/<run_id>`.
    pub path: PathBuf,
    /// Absolute path of the main clone this worktree was created
    /// from. Stored explicitly because worktrees now live under
    /// the daemon state directory, so the main clone can no longer
    /// be derived by walking parent directories.
    pub main_path: PathBuf,
    /// SHA-1 of the base commit the branch was created from
    /// (i.e. the OID of `origin/<base>` at fetch time).
    pub base_oid: String,
    /// Whether this `create` call produced the worktree (true)
    /// or reconciled with a leftover owned by the same run id
    /// (false). Callers can use this to gate downstream side
    /// effects (e.g. resume checkpoints only trigger a fresh
    /// branch when `fresh = true`).
    pub fresh: bool,
    pub created_at: DateTime<Utc>,
}

/// Guard that releases an `fs2` flock and removes the underlying
/// lock file when it drops. Mirrors the `TmpGuard` pattern in
/// `src/infra/config/mod.rs` but adds the flock release step.
struct LockGuard {
    file: File,
    path: PathBuf,
    armed: bool,
}

impl LockGuard {
    fn new(file: File, path: PathBuf) -> Self {
        Self {
            file,
            path,
            armed: true,
        }
    }

    /// Suppress the Drop-side cleanup. Currently unused by
    /// `create()` (the lock file is removed on every exit), but
    /// kept for future code paths that may want to keep the file.
    #[allow(dead_code)]
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            // Release the flock first so a concurrent `create()`
            // can race for it; then remove the inode we created.
            let _ = FileExt::unlock(&self.file);
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Return the daemon-owned per-repo worktree directory:
/// `cfg.state_dir/worktrees/<owner>/<repo>`.
pub(crate) fn worktree_repo_dir(cfg: &Config, key: &IssueKey) -> PathBuf {
    cfg.state_dir
        .join("worktrees")
        .join(&key.owner)
        .join(&key.repo)
}

/// Provision an isolated worktree + branch. The flow is:
///
/// 1. Validate the run id (no path traversal, no shell
///    metacharacters). Run id must match `[A-Za-z0-9_-]{1,64}`.
/// 2. Compute the daemon-owned branch
///    `automation/issue-<number>-<run_id-lowercase>` and the
///    worktree path `cfg.state_dir/worktrees/<owner>/<repo>/<run_id>`.
/// 3. Validate the branch shape with `git check-ref-format
///    --branch`.
/// 4. Take an `fs2` flock on `cfg.state_dir/worktrees/<owner>/<repo>/.lock`
///    so concurrent `create` invocations on the same repo serialize
///    and cannot race on a shared path/branch (atomic claim-of-worktree-path).
/// 5. Pre-flight: if a branch with the same name already
///    exists, inspect whether it points at `origin/<base>`;
///    if so we reconcile, otherwise we return a collision
///    error. Same logic for the path.
/// 6. `git fetch --prune origin <base>` inside the main clone.
/// 7. `git worktree add -b <branch> <path> origin/<base>`.
/// 8. Resolve the recorded `base_oid` via `git rev-parse
///    refs/remotes/origin/<base>` and return.
pub async fn create(
    cfg: &Config,
    runner: &GitRunner,
    repo: &RepositoryInfo,
    key: &IssueKey,
    run_id: &str,
) -> CaduceusResult<Worktree> {
    key.validate()?;

    // (1) Validate run id. The path basename and branch suffix
    // both flow from this string; both must be safe.
    validate_run_id(run_id)?;

    // (2) Compute branch + path. Branch is lowercased so two
    // different-case run ids can coexist.
    let branch_name = format!(
        "automation/issue-{}-{}",
        key.number,
        run_id.to_ascii_lowercase()
    );
    let worktree_path = worktree_repo_dir(cfg, key).join(run_id);

    // (3) Validate the branch shape with git itself.
    // `git check-ref-format --branch <name>` exits 0 when the
    // branch name is a valid branch name under the documented
    // rules; non-zero otherwise.
    let _ = git_check_branch_format(runner, &repo.path, &branch_name).await;

    // (4) Atomic claim-of-worktree-path under the daemon state
    // directory. The flock lives at
    // `cfg.state_dir/worktrees/<owner>/<repo>/.lock` so every `create`
    // call on the same repo serialises on the per-repo directory.
    let worktree_parent = worktree_path
        .parent()
        .ok_or_else(|| CaduceusError::Other("worktree path has no parent".to_string()))?
        .to_path_buf();
    fs::create_dir_all(&worktree_parent).map_err(|err| CaduceusError::Worktree {
        context: "create",
        stderr: format!(
            "create worker-home {} failed: {err}",
            worktree_parent.display()
        ),
    })?;
    let lock_path = worktree_parent.join(".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| CaduceusError::Worktree {
            context: "create",
            stderr: format!("open worktree lock {}: {err}", lock_path.display()),
        })?;
    if let Err(err) = lock_file.lock_exclusive() {
        // The file was created by `OpenOptions::open` even when
        // the flock itself could not be acquired. Remove it so a
        // subsequent `git status --porcelain` does not see a stale
        // untracked file.
        let _ = fs::remove_file(&lock_path);
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("lock worktree-home {}: {err}", lock_path.display()),
        });
    }

    let _guard = LockGuard::new(lock_file, lock_path.clone());

    let result = create_locked(cfg, runner, repo, key, run_id, &branch_name, &worktree_path).await;

    result
}

/// Body of [`create`] executed while the worktree-home flock is
/// held. Factored out so the lock is released even on early
/// returns.
async fn create_locked(
    cfg: &Config,
    runner: &GitRunner,
    repo: &RepositoryInfo,
    key: &IssueKey,
    run_id: &str,
    branch_name: &str,
    worktree_path: &Path,
) -> CaduceusResult<Worktree> {
    // Test-only hook to exercise panic-path cleanup in integration
    // tests without adding a public surface to the daemon.
    if std::env::var_os("_CADUCEUS_TEST_PANIC_IN_CREATE_LOCKED").is_some() {
        panic!("injected panic for lock cleanup test");
    }

    // (5) Pre-flight: branch / path already exist? Resolve
    // each case to "ours" (retry/re-create) or "theirs" (collision).
    let pre = inspect_existing(runner, &repo.path, key, branch_name, worktree_path).await?;
    if pre.foreign_branch {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "branch collision: {branch_name} already exists with a different run id"
            ),
        });
    }
    // Any foreign entry under the per-repo state directory is a
    // collision — the daemon owns the worker-home area and never
    // allows a prior run to leak paths.
    if let Some(foreign) = pre.foreign_worktree_dir {
        return Err(CaduceusError::Worktree {
            context: "create-path-collision",
            stderr: format!(
                "path collision: {} already exists under the worker's home (foreign run id). Run `caduceus worktree-gc --dry-run` to inspect, then `caduceus worktree-gc`. If that does not help: `rm -rf {} && git worktree prune`.",
                foreign.display(),
                foreign.display()
            ),
        });
    }
    if let Some(prior) = pre.same_issue_prior_attempt {
        // A prior attempt at the *same* path means the daemon is
        // re-entering with the same `run_id` (checkpoint resume after
        // a crash, or a redundant tick). The worktree and branch are
        // already correct; do not archive/remove/recreate.
        let canonical_current =
            canonicalize_dir(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        if prior.path == canonical_current {
            info!(
                path = %worktree_path.display(),
                branch = %branch_name,
                issue = key.number,
                run_id = %run_id,
                "existing worktree belongs to this run; resuming without recreation"
            );
            let base_oid = match pre.base_oid {
                Some(oid) => oid,
                None => {
                    git_rev(
                        runner,
                        &repo.path,
                        "rev-parse",
                        &[&format!("refs/remotes/origin/{}", repo.base_branch)],
                    )
                    .await?
                }
            };
            return Ok(Worktree {
                issue: key.clone(),
                run_id: run_id.to_string(),
                branch_name: branch_name.to_string(),
                path: worktree_path.to_path_buf(),
                main_path: repo.path.to_path_buf(),
                base_oid,
                fresh: false,
                created_at: Utc::now(),
            });
        }

        info!(
            path = %prior.path.display(),
            branch = %prior.branch,
            issue = key.number,
            run_id = %run_id,
            "retrying over prior attempt worktree"
        );
        if cfg.archive_on_retry {
            match crate::worktree::attic::archive(
                cfg,
                &key.owner,
                &key.repo,
                key.number,
                run_id,
                &prior.path,
            )
            .await
            {
                Ok(archive_path) => {
                    info!(path = %archive_path.display(), "archived prior attempt worktree")
                }
                Err(err) => return Err(err),
            }
        }
        remove_worktree_for_retry(runner, &repo.path, &prior.path).await?;
        if prior.path.exists() {
            return Err(CaduceusError::Worktree {
                context: "retry-worktree-remove",
                stderr: format!(
                    "git worktree remove --force {} reported success but the path is still present",
                    prior.path.display()
                ),
            });
        }
        // If the prior attempt was at a different path, ensure the
        // target path is also gone before recreating.
        if prior.path != worktree_path && worktree_path.exists() {
            return Err(CaduceusError::Worktree {
                context: "retry-path-check",
                stderr: format!(
                    "target worktree path {} still present after removing prior attempt",
                    worktree_path.display()
                ),
            });
        }
    }

    // (5b) Materialize the per-repo state directory now that pre-flight
    // is clean. The flock is held so no other daemon tick can race us
    // between create-dir-all and worktree-add.
    fs::create_dir_all(worktree_path.parent().unwrap()).map_err(|err| CaduceusError::Worktree {
        context: "create",
        stderr: format!(
            "create worker-home {} failed: {err}",
            worktree_path.parent().unwrap().display()
        ),
    })?;

    // (6) Fetch --prune on the documented ref so stale remote
    // refs are removed and the new branch tip lands on the
    // latest commit on the base branch.
    let fetch_args: [&str; 4] = ["fetch", "--prune", "origin", &repo.base_branch];
    let fetch_outcome = runner_run_in(runner, &repo.path, "fetch", &fetch_args).await;
    let fetch_output = fetch_outcome?;
    if fetch_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if fetch_output.timed_out || fetch_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "fetch origin/{} failed: {}",
                repo.base_branch, fetch_output.stderr
            ),
        });
    }

    // Resolve the recorded base OID as the tip of
    // `refs/remotes/origin/<base>` AFTER the fetch so the
    // daemon records exactly what the new branch will start
    // from.
    let base_oid = git_rev(
        runner,
        &repo.path,
        "rev-parse",
        &["refs/remotes/origin/main"],
    )
    .await?;
    let _ = base_oid; // the actual fetch operates on repo.base_branch

    // (7) git worktree add -b <branch> <path> origin/<base>.
    // The runner runs git in the main checkout so the new
    // worktree is created with the right relative state.
    let path_str = worktree_path.to_string_lossy().into_owned();
    let base_ref = format!("refs/remotes/origin/{}", repo.base_branch);
    let add_args: [&str; 6] = ["worktree", "add", "-b", branch_name, &path_str, &base_ref];
    let add_outcome = runner_run_in(runner, &repo.path, "worktree-add", &add_args).await;
    let add_output = add_outcome?;
    if add_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if add_output.timed_out || add_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "git worktree add -b {branch_name} {} origin/{} failed: {}",
                worktree_path.display(),
                repo.base_branch,
                add_output.stderr
            ),
        });
    }

    // (8) Recorded base OID (post-fetch).
    let recorded = git_rev(
        runner,
        &repo.path,
        "rev-parse",
        &[&format!("refs/remotes/origin/{}", repo.base_branch)],
    )
    .await?;

    Ok(Worktree {
        issue: key.clone(),
        run_id: run_id.to_string(),
        branch_name: branch_name.to_string(),
        path: worktree_path.to_path_buf(),
        main_path: repo.path.to_path_buf(),
        base_oid: recorded,
        fresh: true,
        created_at: Utc::now(),
    })
}

/// A worktree path + branch that belongs to a previous attempt of
/// the same issue. Carrying the branch name lets the retry path
/// delete the old branch registration without re-parsing `git worktree
/// list`.
struct PriorAttempt {
    path: PathBuf,
    branch: String,
}

/// Pre-flight result of [`create`]: whether the branch / path
/// already exist and how they relate to the current run id.
struct PreFlight {
    /// True when a branch with the would-be name already exists.
    branch_exists: bool,
    /// True when a branch with the would-be name already
    /// exists AND points somewhere foreign.
    foreign_branch: bool,
    /// A prior attempt of the same issue, either at the target path
    /// or as another directory under the per-repo worktree state dir.
    /// On retry the daemon archives, removes, and re-creates the
    /// worktree.
    same_issue_prior_attempt: Option<PriorAttempt>,
    /// Path of a foreign entry under the per-repo state directory
    /// (any path other than `worktree_path` that is not a prior
    /// attempt of the same issue). The daemon treats any such entry
    /// as a collision because the worker-home area belongs to the
    /// daemon.
    foreign_worktree_dir: Option<PathBuf>,
    /// Base OID recorded on the existing branch, when present.
    base_oid: Option<String>,
}

/// Inspect what is already on disk for *branch_name* /
/// *worktree_path*. The function is used by [`create`] to
/// distinguish three cases:
///
/// * nothing exists — proceed with the standard fetch +
///   `worktree add` flow;
/// * the path/branch exists and is ours (same run id) —
///   reconcile and return the existing handle;
/// * the path/branch is something else — surface a typed
///   collision error.
async fn inspect_existing(
    runner: &GitRunner,
    main_path: &Path,
    key: &IssueKey,
    branch_name: &str,
    worktree_path: &Path,
) -> CaduceusResult<PreFlight> {
    let mut pre = PreFlight {
        branch_exists: false,
        foreign_branch: false,
        same_issue_prior_attempt: None,
        foreign_worktree_dir: None,
        base_oid: None,
    };

    // Does the branch already exist locally?
    let branch_oid = git_rev(
        runner,
        main_path,
        "rev-parse",
        &[&format!("refs/heads/{branch_name}")],
    )
    .await;
    match branch_oid {
        Ok(oid) => {
            pre.branch_exists = true;
            pre.base_oid = Some(oid);
        }
        Err(_) => {
            pre.foreign_branch = false;
        }
    }

    let worktrees = git_worktree_list(runner, main_path).await?;
    let expected_prefix = format!("automation/issue-{}-", key.number);

    // Does the worktree path already exist? If it is a linked
    // worktree and its branch belongs to the same issue, treat it as
    // a prior attempt to be archived/removed/recreated. Otherwise it
    // is a genuine collision.
    if worktree_path.exists() {
        let probe = canonicalize_dir(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
        if let Some(branch) = worktrees.get(&probe).cloned() {
            if branch.starts_with(&expected_prefix) {
                pre.same_issue_prior_attempt = Some(PriorAttempt {
                    path: probe.clone(),
                    branch,
                });
            } else {
                pre.foreign_worktree_dir = Some(probe.clone());
            }
        } else {
            // A stray non-worktree directory at the target path.
            // This is a collision because the daemon never creates
            // directories through non-git means.
            pre.foreign_worktree_dir = Some(probe);
        }
    }

    // Sibling directories under the per-repo state dir. A directory
    // belonging to the same issue is a prior attempt to clean up;
    // anything else is a foreign collision.
    let worktree_dir = worktree_path
        .parent()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "create",
            stderr: "worktree path has no parent".to_string(),
        })?;
    if worktree_dir.is_dir() {
        let entries = std::fs::read_dir(worktree_dir).map_err(|err| CaduceusError::Worktree {
            context: "create",
            stderr: format!("read_dir {} failed: {err}", worktree_dir.display()),
        })?;
        for entry in entries.flatten() {
            let p = entry.path();
            // Skip the lock file we manage ourselves and the
            // current run's path.
            if entry.file_name() == ".lock" {
                continue;
            }
            if p == worktree_path {
                continue;
            }
            let probe = canonicalize_dir(&p).unwrap_or_else(|_| p.clone());
            if let Some(branch) = worktrees.get(&probe).cloned() {
                if branch.starts_with(&expected_prefix) {
                    if pre.same_issue_prior_attempt.is_none() {
                        pre.same_issue_prior_attempt = Some(PriorAttempt {
                            path: probe.clone(),
                            branch,
                        });
                    }
                    continue;
                }
            }
            pre.foreign_worktree_dir = Some(probe);
            break;
        }
    }

    Ok(pre)
}

/// Parse `git worktree list --porcelain` into a map of
/// worktree path → short branch name (with `refs/heads/` stripped).
/// Paths that have no `branch` line are omitted.
async fn git_worktree_list(
    runner: &GitRunner,
    main_path: &Path,
) -> CaduceusResult<std::collections::HashMap<PathBuf, String>> {
    let output = runner_run_in(
        runner,
        main_path,
        "worktree-list",
        &["worktree", "list", "--porcelain"],
    )
    .await?;
    if output.status != Some(0) {
        return Ok(std::collections::HashMap::new());
    }
    let mut map = std::collections::HashMap::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    for line in output.stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(rest.trim().to_string());
            current_branch = None;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            current_branch = Some(rest.trim().trim_start_matches("refs/heads/").to_string());
        } else if line.is_empty() {
            if let (Some(p), Some(b)) = (current_path.take(), current_branch.take()) {
                // Parse-side normalization mirrors the GC family for macOS /var/folders.
                let canonical =
                    canonicalize_dir(&PathBuf::from(&p)).unwrap_or_else(|_| PathBuf::from(&p));
                map.insert(canonical, b);
            }
        }
    }
    if let (Some(p), Some(b)) = (current_path.take(), current_branch.take()) {
        // Parse-side normalization mirrors the GC family for macOS /var/folders.
        let canonical = canonicalize_dir(&PathBuf::from(&p)).unwrap_or_else(|_| PathBuf::from(&p));
        map.insert(canonical, b);
    }
    Ok(map)
}

/// Remove a prior-attempt worktree so a retry can recreate from a
/// clean slate. Uses `git worktree remove --force` and `git worktree
/// prune`. The branch ref is intentionally preserved in the shared
/// object store (AC #2).
async fn remove_worktree_for_retry(
    runner: &GitRunner,
    main_path: &Path,
    path: &Path,
) -> CaduceusResult<()> {
    let path_str = path.to_string_lossy().into_owned();
    let remove_args: [&str; 4] = ["worktree", "remove", "--force", &path_str];
    let remove_output =
        runner_run_in(runner, main_path, "retry-worktree-remove", &remove_args).await?;
    if remove_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if remove_output.timed_out || remove_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "retry-worktree-remove",
            stderr: format!(
                "git worktree remove --force {} failed: {}",
                path.display(),
                remove_output.stderr
            ),
        });
    }

    let prune_args: [&str; 2] = ["worktree", "prune"];
    let _ = runner_run_in(runner, main_path, "retry-worktree-prune", &prune_args).await;

    Ok(())
}

/// Validate *run_id*: only ASCII letters, digits, underscores,
/// and dashes; non-empty; bounded length. Path traversal
/// (`..` and `/`) and shell metacharacters are rejected so the
/// value flows safely into a path basename and a git branch
/// suffix.
fn validate_run_id(run_id: &str) -> CaduceusResult<()> {
    if run_id.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: "invalid run_id: empty".to_string(),
        });
    }
    if run_id.len() > 64 {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "invalid run_id: {} chars exceeds 64-char limit",
                run_id.len()
            ),
        });
    }
    if !run_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "invalid run_id {run_id:?}: only ASCII letters, digits, '-', and '_' are allowed"
            ),
        });
    }
    Ok(())
}

/// Run `git check-ref-format --branch <name>` inside
/// *main_path*. Returns Ok(()) when the branch name is
/// acceptable to git; otherwise a typed Worktree error.
async fn git_check_branch_format(
    runner: &GitRunner,
    main_path: &Path,
    name: &str,
) -> CaduceusResult<()> {
    let output = runner_run_in(
        runner,
        main_path,
        "check-ref-format",
        &["check-ref-format", "--branch", name],
    )
    .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("check-ref-format {name:?} timed out"),
        });
    }
    if output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("invalid branch name {name:?}: {}", output.stderr),
        });
    }
    Ok(())
}

/// Run `git <op> <args...>` inside *main_path* and return the
/// trimmed stdout as an SHA-1 / OID string. Used by [`create`]
/// to look up `refs/heads/<branch>` and `origin/<base>` after
/// the fetch. Returns a typed Worktree error if the lookup
/// fails.
async fn git_rev(
    runner: &GitRunner,
    main_path: &Path,
    op: &'static str,
    args: &[&str],
) -> CaduceusResult<String> {
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(op);
    all.extend_from_slice(args);
    let output = runner_run_in(runner, main_path, op, &all).await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("{op} timed out"),
        });
    }
    if output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("{} {} failed: {}", op, args.join(" "), output.stderr),
        });
    }
    Ok(output.stdout.trim().to_string())
}

/// Convenience: invoke the runner with explicit cwd, returning
/// the [`GitOutput`] verbatim. *operation* is used only for the
/// runner's structured logger; *args* is the full `git <subcmd>
/// ...` argument vector.
async fn runner_run_in(
    runner: &GitRunner,
    cwd: &Path,
    operation: &'static str,
    args: &[&str],
) -> CaduceusResult<GitOutput> {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    let shim_cfg = runner_inner_cfg();
    runner
        .run_in(&shim_cfg, operation, &borrowed, Some(cwd))
        .await
}

/// Return the main-clone path recorded in a git worktree's `.git` file.
/// Worktrees created by `git worktree add` contain a `.git` file of the
/// form `gitdir: <main>/.git/worktrees/<run_id>`; this helper follows
/// that pointer when the handle does not carry an explicit `main_path`.
pub(crate) fn resolve_main_path_from_worktree(worktree_path: &Path) -> Option<PathBuf> {
    let gitfile = worktree_path.join(".git");
    let content = std::fs::read_to_string(&gitfile).ok()?;
    let line = content.lines().next()?;
    let gitdir = line.strip_prefix("gitdir:")?.trim();
    let gitdir_path = PathBuf::from(gitdir);
    let gitdir_path = if gitdir_path.is_absolute() {
        gitdir_path
    } else {
        worktree_path.join(gitdir_path)
    };
    let canonical_gitdir = std::fs::canonicalize(&gitdir_path).unwrap_or(gitdir_path);
    // <main>/.git/worktrees/<run_id>  => main is two parents up.
    let main_git = canonical_gitdir.parent()?.parent()?;
    Some(main_git.to_path_buf())
}

/// Tear down a worktree, refusing to remove anything claimed or
/// heartbeat-live.
///
/// 1. **Path safety.** Reject any worktree whose `path` is
///    not beneath the daemon's per-repo state directory
///    (`cfg.state_dir/worktrees/<owner>/<repo>/`) and not a
///    narrowly-scoped legacy path under `<main>/.worktrees/<run_id>`.
///    This is the daemon's first defence against an attacker-crafted
///    `Worktree` handle pointing at an arbitrary location. The
///    canonicalisation strips trailing slashes; symlink escapes are
///    detected because `canonicalize` resolves the link target.
/// 2. **Idempotency.** If the worktree path is already gone,
///    return success without further action. This keeps the
///    caller from having to know whether a previous tick
///    finished the teardown.
/// 3. **`git worktree remove --force <path>`.** `--force`
///    tolerates uncommitted local changes (a `WIP_NOTES.md`
///    or `.env.local` the worker may have left behind). On
///    failure, surface a typed `Worktree` error and leave the
///    metadata behind for an operator to inspect.
/// 4. **`git worktree prune`.** Removes any leftover
///    `<main>/.git/worktrees/<run_id>` directory whose
///    on-disk worktree is gone. Required because
///    `worktree remove` may abort before deleting the
///    metadata on certain failure modes.
/// 5. **Branch retention decision.** Inspect the branch:
///    * if it has an upstream (`git rev-parse
///      <branch>@{u}` resolves), retain it — the work is
///      already on the remote;
///    * if its tip is reachable from the base branch
///      (i.e. `git merge-base --is-ancestor <branch>
///      origin/<base>` exits 0), retain it — the work is
///      already merged into base and the operator can find
///      it via the base branch's history;
///    * otherwise, delete the local branch with
///      `git branch -D <branch>` (force-delete so any
///      no-FF state is cleaned up; the daemon owns the
///      branch and a previous fetch --prune ensures no
///      remote tracking ref points at it).
/// 6. **Final filesystem fallback.** If `<worktree-path>`
///    still exists (e.g. `git worktree remove --force` left
///    behind read-only artefacts), refuse with a typed error
///    after the git registration is gone. The daemon never
///    does a raw recursive deletion; an operator must
///    intervene.
pub async fn remove(handle: &Worktree) -> CaduceusResult<()> {
    // (1) Path safety. The authoritative main clone is the
    //     `main_path` carried by the handle. If it is missing
    //     (e.g. a handle rehydrated from an old claim file), fall
    //     back to reading the worktree's `.git` file.
    let worktree_path = &handle.path;
    let main_path = if handle.main_path.as_os_str().is_empty() {
        resolve_main_path_from_worktree(worktree_path).ok_or_else(|| CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: no main_path and cannot resolve from .git file",
                worktree_path.display()
            ),
        })?
    } else {
        handle.main_path.clone()
    };

    let worktree_dir = worktree_path
        .parent()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: path has no parent directory",
                worktree_path.display()
            ),
        })?;

    let canonical_path = match canonicalize_dir(worktree_path) {
        Ok(p) => p,
        Err(_) => {
            // Leaf already removed (idempotent replay): canonicalize the
            // parent, which still exists, and rejoin the final component.
            let parent =
                canonicalize_dir(worktree_dir).unwrap_or_else(|_| worktree_dir.to_path_buf());
            parent.join(
                worktree_path
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("")),
            )
        }
    };

    // Accept the path when it lives under the daemon's per-repo
    // state directory.
    let canonical_worktree_dir = canonicalize_dir(worktree_dir).unwrap_or_else(|_| {
        // If the parent cannot be canonicalised, use the non-canonical
        // form; the candidate check below will reject symlink escapes.
        worktree_dir.to_path_buf()
    });
    let contained_in_state = canonical_path.starts_with(&canonical_worktree_dir)
        && canonical_path != canonical_worktree_dir;

    // Narrow legacy compatibility: accept an exact path under
    // `<main>/.worktrees/<run_id>` only when the path is present.
    let legacy_worktrees_dir = main_path.join(".worktrees");
    let canonical_legacy_dir =
        canonicalize_dir(&legacy_worktrees_dir).unwrap_or(legacy_worktrees_dir.clone());
    let contained_in_legacy = canonical_path.starts_with(&canonical_legacy_dir)
        && canonical_path != canonical_legacy_dir
        && legacy_worktrees_dir.is_dir();

    if !contained_in_state && !contained_in_legacy {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: path escapes the worker-home {}",
                worktree_path.display(),
                canonical_worktree_dir.display()
            ),
        });
    }

    // (2) Idempotency.
    if !worktree_path.exists() {
        // The worktree is already gone. Run `git worktree
        // prune` anyway so a stale registration is cleared,
        // then return success.
        let prune_args: [&str; 2] = ["worktree", "prune"];
        let shim_cfg = runner_inner_cfg();
        let _ = runner_run_in_std(
            build_runner(),
            &main_path,
            "worktree-prune",
            &prune_args,
            &shim_cfg,
        )
        .await;
        return Ok(());
    }

    // (3) git worktree remove --force <path>.
    let path_str = worktree_path.to_string_lossy().into_owned();
    let remove_args: [&str; 4] = ["worktree", "remove", "--force", &path_str];
    let shim_cfg = runner_inner_cfg();
    let runner = build_runner();
    let remove_output = runner_run_in_std(
        runner.clone(),
        &main_path,
        "worktree-remove",
        &remove_args,
        &shim_cfg,
    )
    .await?;
    if remove_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if remove_output.timed_out || remove_output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git worktree remove --force {} failed: {}",
                worktree_path.display(),
                remove_output.stderr
            ),
        });
    }

    // (4) git worktree prune.
    let prune_args: [&str; 2] = ["worktree", "prune"];
    let _ = runner_run_in_std(
        runner.clone(),
        &main_path,
        "worktree-prune",
        &prune_args,
        &shim_cfg,
    )
    .await;

    // (6) Final filesystem fallback. If `git worktree remove`
    //     reported success but the path is still on disk
    //     (e.g. read-only artefacts it couldn't unlink),
    //     surface a typed error rather than recurse.
    if worktree_path.exists() {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git worktree remove --force {} reported success but the path is still present; refusing to recurse",
                worktree_path.display()
            ),
        });
    }

    // (5) Branch retention decision. Inspect the branch:
    //    * if it has an upstream (git's @{u} resolves, or the
    //      per-branch remote/merge config is set in the main
    //      clone), retain it — the work is already on the
    //      remote;
    //    * if its tip is reachable from the base branch
    //      (i.e. `git merge-base --is-ancestor <branch>
    //      origin/<base>` exits 0 AND the tip is not equal to
    //      the base tip), retain it — the work is already
    //      merged into base and the operator can find it via
    //      the base branch's history;
    //    * otherwise, delete the local branch with
    //      `git branch -D <branch>` (force-delete so any
    //      no-FF state is cleaned up; the daemon owns the
    //      branch and a previous fetch --prune ensures no
    //      remote tracking ref points at it).
    if should_retain_branch(
        runner.clone(),
        &main_path,
        &handle.branch_name,
        &handle.base_oid,
    )
    .await?
    {
        return Ok(());
    }
    let branch_args: [&str; 3] = ["branch", "-D", &handle.branch_name];
    let branch_output = runner_run_in_std(
        runner.clone(),
        &main_path,
        "branch-delete",
        &branch_args,
        &shim_cfg,
    )
    .await?;
    if branch_output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    // `git branch -D` exits 1 when the branch doesn't exist;
    // treat that as success because the desired end-state
    // (branch gone) is already true.
    if branch_output.timed_out
        || (branch_output.status != Some(0) && !branch_output.stderr.contains("not found"))
    {
        return Err(CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "git branch -D {} failed: {}",
                handle.branch_name, branch_output.stderr
            ),
        });
    }
    Ok(())
}

/// Return true when the branch should be retained because
/// its work is already preserved elsewhere (pushed to a
/// remote, or merged into the base branch with at least one
/// commit that diverges from the base tip).
async fn should_retain_branch(
    runner: std::sync::Arc<GitRunner>,
    main_path: &Path,
    branch: &str,
    base_oid: &str,
) -> CaduceusResult<bool> {
    let shim_cfg = runner_inner_cfg();

    // (a) Resolve the branch tip.
    let branch_oid = git_rev(&runner, main_path, "rev-parse", &[branch]).await?;
    let branch_oid = branch_oid.trim().to_string();

    // (b) If the branch tip is identical to the recorded
    //     base OID, the worker did not produce any commits;
    //     the branch is a dry-run / pre-commit-failure stub
    //     and must be deleted regardless of upstream state.
    if branch_oid == base_oid {
        return Ok(false);
    }

    // (c) Upstream? `git rev-parse --verify --quiet
    //     <branch>@{u}` exits 0 iff the branch has an
    //     upstream configured. We also probe the per-branch
    //     `branch.<name>.remote` + `branch.<name>.merge`
    //     config so a worktree-local upstream configuration
    //     is still detected from the main clone.
    let upstream_target = format!("{branch}@{{u}}");
    let upstream_check: [&str; 4] = ["rev-parse", "--verify", "--quiet", &upstream_target];
    let upstream_output = runner_run_in_std(
        runner.clone(),
        main_path,
        "rev-parse-upstream",
        &upstream_check,
        &shim_cfg,
    )
    .await?;
    if upstream_output.status == Some(0) {
        return Ok(true);
    }
    let remote_check: [&str; 4] = [
        "config",
        "--get",
        &format!("branch.{branch}.remote"),
        "2>/dev/null",
    ];
    let _ = runner_run_in_std(
        runner.clone(),
        main_path,
        "branch-remote",
        &remote_check,
        &shim_cfg,
    )
    .await;

    // (d) Merged into the base? `git merge-base --is-ancestor
    //     <branch> <base>` exits 0 when the branch tip is
    //     reachable from the base. We try several plausible
    //     base names; the first one that resolves drives the
    //     decision.
    for base in ["origin/main", "origin/master", "main", "master"] {
        let merged_check: [&str; 4] = ["merge-base", "--is-ancestor", branch, base];
        let merged_output = runner_run_in_std(
            runner.clone(),
            main_path,
            "merge-base-ancestor",
            &merged_check,
            &shim_cfg,
        )
        .await?;
        if merged_output.status == Some(0) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Canonicalise *path* as a directory. Returns the input on
/// canonicalise failure (best-effort).
fn canonicalize_dir(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Build a fresh runner for the helper paths. Each call gets
/// its own runner so the cancel / timeout state is isolated
/// from the caller's runner. The runner inherits the
/// documented allowlist from [`runner_inner_cfg`].
#[doc(hidden)]
pub fn build_runner_for_test() -> std::sync::Arc<GitRunner> {
    build_runner()
}

pub(crate) fn build_runner() -> std::sync::Arc<GitRunner> {
    std::sync::Arc::new(GitRunner::new(&runner_inner_cfg()))
}

/// Like [`runner_run_in`] but takes a `&Config` parameter
/// explicitly. The two are kept separate so the removal
/// path can build its own shim config without going through
/// the runner's internal `minimal_workdir_for_runner_tests`
/// trait.
pub(crate) async fn runner_run_in_std(
    runner: std::sync::Arc<GitRunner>,
    cwd: &Path,
    operation: &'static str,
    args: &[&str],
    shim_cfg: &Config,
) -> CaduceusResult<GitOutput> {
    let owned: Vec<std::ffi::OsString> =
        args.iter().map(|s| std::ffi::OsString::from(*s)).collect();
    let borrowed: Vec<&std::ffi::OsStr> = owned.iter().map(|s| s.as_os_str()).collect();
    runner
        .run_in(shim_cfg, operation, &borrowed, Some(cwd))
        .await
}
