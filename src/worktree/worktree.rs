#![allow(dead_code, unused_imports)]
use super::*;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt;
use nix::unistd::pipe;
use tokio::process::Command as TokioCommand;
use url::Url;

use crate::github::issue::IssueKey;
use crate::infra::config::Config;
use crate::infra::error::{scrub, CaduceusError, CaduceusResult};

// ---------------------------------------------------------------------------
// Worktree (created by `create`, torn down by `destroy`, GCed by `gc`).
// ---------------------------------------------------------------------------

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

/// Provision an isolated worktree + branch. The flow per
/// `tasks/4.2-create-a-daemon-owned-worktree-and-branch.md` is:
///
/// 1. Validate the run id (no path traversal, no shell
///    metacharacters). Run id must match `[A-Za-z0-9_-]{1,64}`.
/// 2. Compute the daemon-owned branch
///    `automation/issue-<number>-<run_id-lowercase>` and the
///    worktree path `<repo>/.worktrees/<run_id>`.
/// 3. Validate the branch shape with `git check-ref-format
///    --branch` (per task spec).
/// 4. Take an `fs2` flock on `<repo>/.worktrees/.lock` so
///    concurrent `create` invocations on the same main clone
///    serialize and cannot race on a shared path/branch
///    (atomic claim-of-worktree-path).
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

    // (2) Compute branch + path. Branch is lowercased per the
    // task packet; path keeps the original case so two
    // different-case run ids can coexist.
    let branch_name = format!(
        "automation/issue-{}-{}",
        key.number,
        run_id.to_ascii_lowercase()
    );
    let worktree_path = repo.path.join(".worktrees").join(run_id);

    // (3) Validate the branch shape with git itself (per task
    // spec). `git check-ref-format --branch <name>` exits 0
    // when the branch name is a valid branch name under the
    // documented rules; non-zero otherwise.
    git_check_branch_format(runner, &repo.path, &branch_name).await?;

    // (4) Atomic claim-of-worktree-path under the worker-home
    // area. The flock lives at `<repo>/.worktrees/.lock` so
    // every `create` call on the same main clone serialises on
    // a directory that's already in the worktree-parent path.
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
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!("lock worktree-home {}: {err}", lock_path.display()),
        });
    }

    let result = create_locked(cfg, runner, repo, key, run_id, &branch_name, &worktree_path).await;

    // Release the flock regardless of outcome. `fs2::FileExt`
    // documents that the lock is released on close; explicit
    // `unlock` here keeps the lock held file usable for
    // further flock-based coordination in Phase 5/7.
    let _ = FileExt::unlock(&lock_file);
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
    let _ = cfg;

    // (5) Pre-flight: branch / path already exist? Resolve
    // each case to "ours" (reconcile) or "theirs" (collision).
    let pre = inspect_existing(runner, &repo.path, branch_name, worktree_path).await?;
    if pre.foreign_branch {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "branch collision: {branch_name} already exists with a different run id"
            ),
        });
    }
    if pre.foreign_path {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "path collision: {} already exists with a different run id",
                worktree_path.display()
            ),
        });
    }
    // Any foreign entry under `.worktrees/` is a collision —
    // the daemon owns the worker-home area and never allows a
    // prior run to leak paths.
    if let Some(foreign) = pre.foreign_worktree_dir {
        return Err(CaduceusError::Worktree {
            context: "create",
            stderr: format!(
                "path collision: {} already exists under the worker's home (foreign run id)",
                foreign.display()
            ),
        });
    }
    if pre.owned {
        if let Some(base_oid) = pre.base_oid {
            // Idempotent re-entry into the same run id: return
            // the existing handle so callers can resume.
            return Ok(Worktree {
                issue: key.clone(),
                run_id: run_id.to_string(),
                branch_name: branch_name.to_string(),
                path: worktree_path.to_path_buf(),
                base_oid,
                fresh: false,
                created_at: pre.created_at.unwrap_or_else(Utc::now),
            });
        }
    }

    // (5b) Materialize the worker-home area now that pre-flight
    // is clean. The flock is held so no other daemon tick can
    // race us between create-dir-all and worktree-add.
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
        base_oid: recorded,
        fresh: true,
        created_at: Utc::now(),
    })
}

/// Pre-flight result of [`create`]: whether the branch / path
/// already exist and how they relate to the current run id.
struct PreFlight {
    /// True when a branch with the would-be name already
    /// exists at `origin/<base>` (i.e. it's ours).
    branch_exists: bool,
    /// True when a branch with the would-be name already
    /// exists AND points somewhere foreign.
    foreign_branch: bool,
    /// True when the worktree path already exists and is a
    /// git worktree whose `branch_name` matches ours.
    owned: bool,
    /// True when the worktree path already exists and is
    /// something else.
    foreign_path: bool,
    /// Path of a foreign entry under `.worktrees/` (any path
    /// other than `worktree_path`). The daemon treats any
    /// such entry as a collision because the worker-home
    /// area belongs to the daemon.
    foreign_worktree_dir: Option<PathBuf>,
    /// Base OID recorded on the existing branch, when
    /// reconciling.
    base_oid: Option<String>,
    /// File mtime of the existing worktree, when reconciling.
    created_at: Option<chrono::DateTime<Utc>>,
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
    branch_name: &str,
    worktree_path: &Path,
) -> CaduceusResult<PreFlight> {
    let mut pre = PreFlight {
        branch_exists: false,
        foreign_branch: false,
        owned: false,
        foreign_path: false,
        foreign_worktree_dir: None,
        base_oid: None,
        created_at: None,
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

    // Does the worktree path already exist? Either as a
    // legitimate worktree (ours) or as a stray directory/file.
    if worktree_path.exists() {
        // `git worktree list` includes the path and the branch
        // for each linked worktree. If our path is listed with
        // our branch it belongs to us; otherwise it is foreign.
        let owned = inspect_path_is_ours(runner, main_path, worktree_path, branch_name).await?;
        if owned {
            pre.owned = true;
            if let Ok(meta) = std::fs::metadata(worktree_path) {
                if let Ok(mtime) = meta.modified() {
                    let dt: chrono::DateTime<Utc> = mtime.into();
                    pre.created_at = Some(dt);
                }
            }
        } else {
            pre.foreign_path = true;
        }
    }

    // Foreign entries under `.worktrees/` are always a
    // collision: the daemon owns the worker-home area and
    // never allows a prior run to leak paths.
    let worktree_dir = main_path.join(".worktrees");
    if worktree_dir.is_dir() {
        let entries = std::fs::read_dir(&worktree_dir).map_err(|err| CaduceusError::Worktree {
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
            pre.foreign_worktree_dir = Some(p);
            break;
        }
    }

    Ok(pre)
}

/// Return true when the worktree at *worktree_path* is registered
/// to *branch_name* in `git worktree list`. Both nil cases (path
/// absent, branch absent) return false — the caller decides what
/// to do.
async fn inspect_path_is_ours(
    runner: &GitRunner,
    main_path: &Path,
    worktree_path: &Path,
    branch_name: &str,
) -> CaduceusResult<bool> {
    let output = runner_run_in(
        runner,
        main_path,
        "worktree-list",
        &["worktree", "list", "--porcelain"],
    )
    .await?;
    if !output.status.eq(&Some(0)) {
        return Ok(false);
    }
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    for line in output.stdout.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(rest.trim().to_string());
            current_branch = None;
        } else if let Some(rest) = line.strip_prefix("branch ") {
            current_branch = Some(rest.trim().trim_start_matches("refs/heads/").to_string());
        } else if line.is_empty() {
            if let (Some(p), Some(b)) = (&current_path, &current_branch) {
                if p == &worktree_path.to_string_lossy() && b == branch_name {
                    return Ok(true);
                }
            }
            current_path = None;
            current_branch = None;
        }
    }
    if let (Some(p), Some(b)) = (&current_path, &current_branch) {
        if p == &worktree_path.to_string_lossy() && b == branch_name {
            return Ok(true);
        }
    }
    Ok(false)
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

/// Tear down a worktree, refusing to remove anything claimed or
/// heartbeat-live.
///
/// The flow per `tasks/4.3-tear-down-safely.md`:
///
/// 1. **Path safety.** Reject any worktree whose `path` is
///    not beneath `<main>/.worktrees/`. This is the daemon's
///    first defence against an attacker-crafted `Worktree`
///    handle pointing at an arbitrary location. The
///    canonicalisation strips trailing slashes; `..`
///    components are *not* followed.
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
    // (1) Path safety. The worktree's main repo is the
    //     parent of its `.worktrees/<run_id>` path. We
    //     canonicalise the parent directory and require
    //     the worktree path to live strictly under it.
    let worktree_path = &handle.path;
    let main_path = worktree_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| CaduceusError::Worktree {
            context: "destroy",
            stderr: format!(
                "refusing to remove {}: path has no main clone ancestor",
                worktree_path.display()
            ),
        })?;
    let worktree_dir = main_path.join(".worktrees");
    let canonical_main = canonicalize_dir(main_path)?;
    let canonical_worktree_dir =
        canonicalize_dir(&worktree_dir).unwrap_or(canonical_main.join(".worktrees"));
    let canonical_path =
        canonicalize_dir(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    if !canonical_path.starts_with(&canonical_worktree_dir) {
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
            main_path,
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
        main_path,
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
        main_path,
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
        main_path,
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
        main_path,
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
