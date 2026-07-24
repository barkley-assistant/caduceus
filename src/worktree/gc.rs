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

/// Worktree GC entry point shared by both `caduceus worktree-gc`
/// `caduceus worktree-gc` — sweep stale worktrees across the
/// configured repositories.
///
/// For each repository in `config.watched_repos`, this
/// enumerates registered worktrees via `git worktree list
/// --porcelain` and removes the ones that:
///
/// * are older than `older_than_days` (measured against the
///   worktree directory's mtime);
/// * are **not** referenced by an active claim file in
///   `<state_dir>/claims/`;
/// * are **not** referenced by a fresh heartbeat file in
///   `<state_dir>/runs/`;
/// * live strictly under `<main>/.worktrees/`.
///
/// It also walks `<main>/.worktrees/` for unregistered
/// orphans and removes the ones that pass the same tests,
/// with the additional safety check that the path is not a
/// symlink. Symlinks are reported and left alone.
///
/// `dry_run = true` reports what would be removed without
/// mutating any state.
///
/// Returns the number of worktrees actually removed (always
/// `0` when `dry_run = true`).
pub async fn gc(config: &Config, older_than_days: u64, dry_run: bool) -> CaduceusResult<u64> {
    let state_dir = &config.state_dir;
    let now = Utc::now();
    let age_cutoff = now - chrono::Duration::days(older_than_days as i64);

    // Step 1: collect the set of worktree paths that are
    // currently in use. A worktree is "in use" if any claim
    // file references it, or if any heartbeat file is
    // recent. We read the directory and build an in-memory
    // set; this is O(n) where n = total claims + heartbeats.
    let in_use = collect_in_use_worktree_paths(state_dir).await?;

    // Step 2: for each repository, run `git worktree list
    // --porcelain` and act on each entry. We compute the
    // repo path from the config the same way `create` does,
    // so the GC is consistent with the worker-side path
    // resolution.
    let mut total_removed: u64 = 0;
    for repo in &config.watched_repos {
        let (owner, name) = parse_watched_repo(repo).ok_or_else(|| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("invalid watched_repos entry: {repo:?}"),
        })?;
        let main_path = config.workdir_base.join(&owner).join(&name);
        if !main_path.is_dir() {
            // Repository not cloned locally — nothing to GC.
            continue;
        }
        let entries = list_worktrees_porcelain(&main_path).await?;
        for entry in &entries {
            // Skip the main clone itself (porcelain includes
            // it as the first entry).
            if entry.path == main_path {
                continue;
            }
            // Path safety: must live under
            // `<main_path>/.worktrees/`.
            let worktrees_dir = main_path.join(".worktrees");
            let canonical_wt = match std::fs::canonicalize(&entry.path) {
                Ok(p) => p,
                Err(_) => entry.path.clone(),
            };
            let canonical_wtdir =
                std::fs::canonicalize(&worktrees_dir).unwrap_or_else(|_| worktrees_dir.clone());
            if !canonical_wt.starts_with(&canonical_wtdir) {
                eprintln!(
                    "caduceus worktree-gc: refusing to remove {}: path escapes {}",
                    entry.path.display(),
                    canonical_wtdir.display()
                );
                continue;
            }
            // Active?
            if in_use.contains(&canonical_wt) {
                continue;
            }
            // Old enough?
            let mtime = match mtime_of(&entry.path) {
                Some(t) => t,
                None => continue, // can't tell → leave alone
            };
            if mtime > age_cutoff {
                continue;
            }
            if dry_run {
                println!(
                    "would remove {} (branch {}, age {} days)",
                    entry.path.display(),
                    entry.branch,
                    older_than_days
                );
                continue;
            }
            // Build a Worktree handle and call Task 4.3's
            // remove(). The branch_name in the handle is
            // advisory only; remove() inspects ref state.
            let wt = Worktree {
                issue: crate::github::issue::IssueKey {
                    owner: owner.clone(),
                    repo: name.clone(),
                    number: 0,
                },
                run_id: entry.branch.clone(),
                branch_name: entry.branch.clone(),
                path: entry.path.clone(),
                base_oid: String::new(),
                fresh: false,
                created_at: mtime,
            };
            match remove(&wt).await {
                Ok(()) => {
                    total_removed += 1;
                    println!(
                        "removed worktree {} (branch {})",
                        entry.path.display(),
                        entry.branch
                    );
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: remove {} failed: {err}",
                        entry.path.display()
                    );
                }
            }
        }

        // Step 3: orphan directories under .worktrees/ that
        // git no longer tracks. We only act on canonical
        // children that are not symlinks, are not in the
        // registered list, are old, and are not in use.
        // Unlike registered worktrees, orphans are removed
        // with a direct `fs::remove_dir_all` because
        // `git worktree remove --force` refuses on a path
        // that git does not know about.
        let orphans = collect_orphan_worktrees(&main_path, &entries, &in_use, age_cutoff);
        for orphan in orphans {
            if dry_run {
                println!("would remove orphan {}", orphan.display());
                continue;
            }
            match std::fs::remove_dir_all(&orphan) {
                Ok(()) => {
                    total_removed += 1;
                    println!("removed orphan {}", orphan.display());
                }
                Err(err) => {
                    eprintln!(
                        "caduceus worktree-gc: orphan remove {} failed: {err}",
                        orphan.display()
                    );
                }
            }
        }
    }
    Ok(total_removed)
}

/// One entry of `git worktree list --porcelain`. We extract
/// just the path and the branch (the porcelain format
/// contains more keys, but those are enough for GC).
#[derive(Debug, Clone)]
struct WorktreeListEntry {
    path: PathBuf,
    branch: String,
}

/// Read `git worktree list --porcelain` for *main_path* and
/// return each entry's path and branch.
async fn list_worktrees_porcelain(main_path: &Path) -> CaduceusResult<Vec<WorktreeListEntry>> {
    let runner = build_runner();
    let shim_cfg = runner_inner_cfg();
    let output = runner_run_in_std(
        runner,
        main_path,
        "worktree-list",
        &["worktree", "list", "--porcelain"],
        &shim_cfg,
    )
    .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out || output.status != Some(0) {
        return Err(CaduceusError::Worktree {
            context: "gc",
            stderr: format!("git worktree list --porcelain failed: {}", output.stderr),
        });
    }
    let mut entries = Vec::new();
    let mut current: Option<WorktreeListEntry> = None;
    for line in output.stdout.lines() {
        if line.is_empty() {
            if let Some(e) = current.take() {
                entries.push(e);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            // Start a new entry.
            if let Some(e) = current.take() {
                entries.push(e);
            }
            current = Some(WorktreeListEntry {
                path: PathBuf::from(rest.trim()),
                branch: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("branch ") {
            if let Some(e) = current.as_mut() {
                // `refs/heads/<branch>` — strip the prefix.
                let branch = rest.trim();
                let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                e.branch = branch.to_string();
            }
        }
    }
    if let Some(e) = current.take() {
        entries.push(e);
    }
    Ok(entries)
}

/// Collect canonical worktree paths that are currently
/// referenced by an active claim file or by a fresh
/// heartbeat. The result is used to exclude worktrees from
/// GC. Freshness for heartbeats is the mtime within the
/// last hour (twice the documented heartbeat interval).
async fn collect_in_use_worktree_paths(
    state_dir: &Path,
) -> CaduceusResult<std::collections::HashSet<PathBuf>> {
    let mut paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Claims.
    let claims_dir = state_dir.join("claims");
    if claims_dir.is_dir() {
        let entries = std::fs::read_dir(&claims_dir).map_err(|err| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("read_dir {}: {err}", claims_dir.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(".claim"))
                .unwrap_or(false)
            {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(body) =
                    serde_json::from_slice::<crate::state::queue::ClaimFileBody>(&bytes)
                {
                    if let Some(wt) = body.worktree_path {
                        if let Ok(c) = std::fs::canonicalize(&wt) {
                            paths.insert(c);
                        } else {
                            paths.insert(wt);
                        }
                    }
                }
            }
        }
    }
    // Heartbeats. A heartbeat is fresh if its mtime is
    // within the last hour; the documented heartbeat
    // interval is 30 minutes, so a one-hour cutoff tolerates
    // a single missed tick. The run_id is the basename
    // without the `.heartbeat` extension; the worktree
    // path is `<workdir_base>/<owner>/<repo>/.worktrees/<run_id>`.
    // We resolve to a concrete path on disk if it exists so
    // the GC's path-canonicalisation check works.
    let runs = state_dir.join("runs");
    if runs.is_dir() {
        let heartbeat_fresh_cutoff = Utc::now() - chrono::Duration::hours(1);
        let entries = std::fs::read_dir(&runs).map_err(|err| CaduceusError::Worktree {
            context: "gc",
            stderr: format!("read_dir {}: {err}", runs.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !name.ends_with(".heartbeat") {
                continue;
            }
            let mtime = match mtime_of(&path) {
                Some(t) => t,
                None => continue,
            };
            if mtime < heartbeat_fresh_cutoff {
                continue;
            }
            // The run_id is the basename minus the
            // `.heartbeat` extension. We add *all* matching
            // worktree paths under any watched repo so the
            // GC's path-canonicalisation check works. The
            // path is the only known mapping for a heartbeat
            // because the heartbeat file itself is just a
            // timestamp.
            let run_id = name.trim_end_matches(".heartbeat");
            for repo in watched_repo_paths() {
                let candidate = repo.join(".worktrees").join(run_id);
                if candidate.is_dir() {
                    if let Ok(c) = std::fs::canonicalize(&candidate) {
                        paths.insert(c);
                    } else {
                        paths.insert(candidate);
                    }
                }
            }
        }
    }
    Ok(paths)
}

/// The list of `<workdir_base>/<owner>/<repo>` paths we
/// know about, computed from the daemon's `workdir_base` +
/// `watched_repos`. We read the config from the active
/// process — the GC runs under `DaemonLock`, so the daemon
/// is the only process active. If the config cannot be
/// loaded (e.g. on a fresh state dir with no daemon config
/// yet), we fall back to walking the daemon's documented
/// state path. The function is a best-effort aid for the
/// in-use set; the authoritative heartbeat-to-worktree
/// mapping lives in the queue state and is consulted via
/// the claim scan above.
fn watched_repo_paths() -> Vec<PathBuf> {
    let cfg = match crate::infra::config::Config::load() {
        Ok(c) => c,
        Err(_) => match std::env::var_os("CADUCEUS_CONFIG") {
            Some(p) => match crate::infra::config::Config::load_from(std::path::Path::new(&p)) {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            },
            None => return Vec::new(),
        },
    };
    cfg.watched_repos
        .iter()
        .filter_map(|s| s.split_once('/'))
        .map(|(owner, repo)| cfg.workdir_base.join(owner).join(repo))
        .collect()
}

/// Find unregistered directories under
/// `<main>/.worktrees/` that the GC may consider for
/// removal. Each path is a regular directory (not a
/// symlink), not in the registered set, not in the
/// in-use set, and old enough.
fn collect_orphan_worktrees(
    main_path: &Path,
    registered: &[WorktreeListEntry],
    in_use: &std::collections::HashSet<PathBuf>,
    age_cutoff: DateTime<Utc>,
) -> Vec<PathBuf> {
    let worktrees_dir = main_path.join(".worktrees");
    if !worktrees_dir.is_dir() {
        return Vec::new();
    }
    let mut registered_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for entry in registered {
        if let Ok(c) = std::fs::canonicalize(&entry.path) {
            registered_paths.insert(c);
        } else {
            registered_paths.insert(entry.path.clone());
        }
    }
    let mut orphans = Vec::new();
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(rd) => rd,
        Err(_) => return orphans,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks. The daemon never follows them.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            eprintln!(
                "caduceus worktree-gc: refusing orphan {}: is a symlink",
                path.display()
            );
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        // Skip `.lock` and other dotfiles that git uses.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if registered_paths.contains(&canonical) {
            continue;
        }
        if in_use.contains(&canonical) {
            continue;
        }
        let mtime = match mtime_of(&canonical) {
            Some(t) => t,
            None => continue,
        };
        if mtime > age_cutoff {
            continue;
        }
        orphans.push(path);
    }
    orphans
}

/// mtime as a `DateTime<Utc>`, or `None` if it cannot be
/// determined.
fn mtime_of(path: &Path) -> Option<DateTime<Utc>> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.into())
}

/// Parse a `watched_repos` entry like `owner/repo` into
/// `(owner, repo)`.
fn parse_watched_repo(s: &str) -> Option<(String, String)> {
    let (owner, repo) = s.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

// ---------------------------------------------------------------------------
// Stub for older callers; the canonical entry point is `gc(config, ...)`.
// Kept for source-compatibility with Task 3.3's reaper call site.
// ---------------------------------------------------------------------------
#[doc(hidden)]
pub fn gc_legacy_state_dir(
    _state_dir: &Path,
    _older_than_days: u64,
    _dry_run: bool,
) -> CaduceusResult<u64> {
    Err(CaduceusError::Worktree {
        context: "gc",
        stderr: "use gc(config, ...); the state-dir-only signature was retired in 4.5".to_string(),
    })
}
