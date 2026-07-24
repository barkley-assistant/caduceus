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
// Repository discovery
// ---------------------------------------------------------------------------

/// Outcome of [`find_main_clone`]: the resolved on-disk clone
/// plus the metadata the daemon needs to create worktrees off
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryInfo {
    /// Absolute path of the on-disk clone. For a non-bare clone
    /// this is the working tree; for a bare clone this is the
    /// bare repository directory.
    pub path: PathBuf,
    /// Default base branch (e.g. `main`, `master`). Resolved from
    /// `refs/remotes/origin/HEAD`; falls back to the repository's
    /// current branch (with a tracing warning) when the remote
    /// HEAD symref is missing.
    pub base_branch: String,
    /// The origin's URL (already normalized for matching). For
    /// host-validation purposes the host is derived from this
    /// value, not the daemon's `api_base`.
    pub remote_url: String,
}

/// Discover the local main clone for *key*. The path is
/// `<workdir_base>/<owner>/<repo>`. The function validates:
/// * the path exists and is part of a git working tree,
/// * the working tree is clean (`git status --porcelain` empty),
/// * the origin's host matches the daemon's `api_base` host
///   (public github.com or the configured enterprise host),
/// * the origin's normalized `owner/repo` matches the issue
///   slug.
///
/// SSH host aliases (e.g. `git@github.com-attacker:...`) are
/// explicitly rejected in v0.1 because their destination cannot
/// be authenticated from the remote string alone.
pub async fn find_main_clone(
    cfg: &Config,
    runner: &GitRunner,
    key: &IssueKey,
) -> CaduceusResult<RepositoryInfo> {
    key.validate()?;
    let path = clone_path(cfg, key);
    if !path.exists() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("clone missing at {}", path.display()),
        });
    }
    if !path.is_dir() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("clone path is not a directory: {}", path.display()),
        });
    }

    // `git rev-parse --git-dir` succeeds only inside a working
    // tree (regular or bare). A missing failure here means the
    // directory is not a git repository.
    let git_dir = git_string(runner, &path, "rev-parse", &["--git-dir"]).await?;
    if git_dir.trim().is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("not a git repository: {}", path.display()),
        });
    }

    // Working tree must be clean: the daemon never operates on a
    // dirty main checkout (operator visible signal that the
    // branch is mid-edit).
    let porcelain = git_string(runner, &path, "status", &["--porcelain"]).await?;
    if !porcelain.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "main checkout is dirty at {}; refusing to operate",
                path.display()
            ),
        });
    }

    // Resolve the origin URL and validate the host / slug match.
    let remote_url = git_string(runner, &path, "config", &["--get", "remote.origin.url"]).await;
    let remote_url = match remote_url {
        Ok(text) => text,
        Err(_) => {
            // `git config --get` exits with status 1 and an
            // empty stderr when the key is missing. Translate
            // that into the documented "no remote configured"
            // error rather than the raw `Git { stderr: "" }`
            // variant.
            return Err(CaduceusError::Worktree {
                context: "discover",
                stderr: format!("no remote.origin.url configured at {}", path.display()),
            });
        }
    };
    let remote_url = remote_url.trim().to_string();
    if remote_url.is_empty() {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!("no remote.origin.url configured at {}", path.display()),
        });
    }
    let (remote_owner, remote_repo) = parse_origin(&remote_url)?;
    let remote_pair = format!(
        "{}/{}",
        remote_owner.to_ascii_lowercase(),
        remote_repo.to_ascii_lowercase()
    );
    let expected_pair = format!(
        "{}/{}",
        key.owner.to_ascii_lowercase(),
        key.repo.to_ascii_lowercase()
    );
    if remote_pair != expected_pair {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "origin {remote_owner}/{remote_repo} does not match issue slug {}/{}",
                key.owner, key.repo
            ),
        });
    }
    validate_origin_host(&remote_url, &cfg.api_base)?;

    // Default base branch. Prefer `refs/remotes/origin/HEAD`;
    // fall back to the local HEAD with a warning.
    let base_branch =
        match git_string(runner, &path, "symbolic-ref", &["refs/remotes/origin/HEAD"]).await {
            Ok(text) => text
                .trim()
                .strip_prefix("refs/remotes/origin/")
                .unwrap_or(text.trim())
                .to_string(),
            Err(_) => match git_string(runner, &path, "symbolic-ref", &["HEAD"]).await {
                Ok(text) => {
                    let raw = text.trim();
                    let branch = raw
                        .strip_prefix("refs/heads/")
                        .or_else(|| raw.strip_prefix("refs/remotes/"))
                        .unwrap_or(raw)
                        .to_string();
                    tracing::warn!(
                        branch = %branch,
                        path = %path.display(),
                        "origin/HEAD missing; falling back to local HEAD"
                    );
                    branch
                }
                Err(_) => {
                    return Err(CaduceusError::Worktree {
                        context: "discover",
                        stderr: format!(
                            "detached HEAD without refs/remotes/origin/HEAD at {}",
                            path.display()
                        ),
                    });
                }
            },
        };

    Ok(RepositoryInfo {
        path,
        base_branch,
        remote_url,
    })
}

/// Compute the on-disk path of the main clone for *key*. The
/// components come straight from the (validated) [`IssueKey`];
/// path traversal / special characters are rejected by
/// [`IssueKey::validate`], so no additional sanitisation is
/// needed here.
pub fn clone_path(cfg: &Config, key: &IssueKey) -> PathBuf {
    cfg.workdir_base.join(&key.owner).join(&key.repo)
}

/// Parse `origin.url` into `(owner, repo)`. Supports SSH
/// (`git@host:owner/repo.git`), HTTPS
/// (`https://host/owner/repo.git`), and `git://` URLs. SSH host
/// aliases like `git@github.com-attacker:owner/repo.git` parse
/// successfully but are rejected later by
/// [`validate_origin_host`].
pub fn parse_origin(remote_url: &str) -> CaduceusResult<(String, String)> {
    let remote_url = remote_url.trim();
    // SSH form: `[user@]host:owner/repo[.git]`. We do NOT use a
    // URL parser here because the colon makes it ambiguous with
    // scheme-bearing URLs.
    if let Some((_, after_colon)) = remote_url.split_once(':') {
        // Make sure the prefix is not a scheme like `https:` —
        // those always start with `scheme://`.
        if !remote_url.contains("://") {
            let path = after_colon.trim_start_matches('/');
            let stripped = path.trim_end_matches(".git").trim_end_matches('/');
            let (owner, repo) =
                stripped
                    .split_once('/')
                    .ok_or_else(|| CaduceusError::Worktree {
                        context: "discover",
                        stderr: format!("origin SSH URL has no owner/repo: {remote_url}"),
                    })?;
            return Ok((owner.to_string(), repo.to_string()));
        }
    }
    // HTTPS / git:// form: parse via `url::Url`.
    let url = Url::parse(remote_url).map_err(|err| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL not parseable: {remote_url} ({err})"),
    })?;
    let mut segments = url
        .path_segments()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL has no path: {remote_url}"),
        })?
        .filter(|s| !s.is_empty());
    let owner = segments.next().ok_or_else(|| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL missing owner: {remote_url}"),
    })?;
    let repo = segments
        .next()
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL missing repo: {remote_url}"),
        })?
        .trim_end_matches(".git");
    Ok((owner.to_string(), repo.to_string()))
}

/// Validate that the origin host matches the daemon's `api_base`
/// host. Public github.com → origin host must equal `github.com`.
/// Enterprise → origin host must equal the api_base host verbatim.
pub fn validate_origin_host(remote_url: &str, api_base: &str) -> CaduceusResult<()> {
    let api_url = Url::parse(api_base)
        .map_err(|err| CaduceusError::Config(format!("invalid api_base {api_base}: {err}")))?;
    let api_host = api_url
        .host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| CaduceusError::Config(format!("api_base has no host: {api_base}")))?;
    let origin_host = origin_host(remote_url)?;
    // The contract distinguishes two cases:
    //   1. Public github.com — api_base is the canonical
    //      https://api.github.com URL (the default). The
    //      operator's clones target `github.com`, not
    //      `api.github.com`, so the origin host check is
    //      against `github.com`.
    //   2. GitHub Enterprise — api_base points at
    //      `https://<enterprise>/` (e.g. ghe.example.com); the
    //      origin host must equal that same hostname verbatim.
    let is_public = api_host == "api.github.com"
        || api_host == "github.com"
        || api_base.trim_end_matches('/') == "https://api.github.com";
    if is_public {
        if origin_host != "github.com" {
            return Err(CaduceusError::Worktree {
                context: "discover",
                stderr: format!(
                    "origin host {origin_host:?} does not match public api_base host github.com"
                ),
            });
        }
    } else if origin_host != api_host {
        return Err(CaduceusError::Worktree {
            context: "discover",
            stderr: format!(
                "origin host {origin_host:?} does not match api_base host {api_host:?}"
            ),
        });
    }
    Ok(())
}

/// Extract the host component of an origin URL. SSH forms
/// (`git@host:owner/repo`) yield `host` (or the alias-like form
/// `host-attacker`). URL forms yield `Url::host_str()`.
fn origin_host(remote_url: &str) -> CaduceusResult<String> {
    let remote_url = remote_url.trim();
    if let Some((before_colon, after)) = remote_url.split_once(':') {
        if !remote_url.contains("://") {
            // SSH form: host lives before the colon. The
            // optional `user@` prefix is stripped — keep the
            // segment after the LAST `@`.
            let host = match before_colon.rsplit_once('@') {
                Some((_, host)) => host,
                None => before_colon,
            };
            // Reject SSH aliases outright (v0.1 cannot verify
            // their destination from the remote string).
            if host.contains('-') && !host.ends_with("github.com") {
                return Err(CaduceusError::Worktree {
                    context: "discover",
                    stderr: format!("origin SSH host alias is not allowed: {host}"),
                });
            }
            let _ = after;
            return Ok(host.to_ascii_lowercase());
        }
    }
    let url = Url::parse(remote_url).map_err(|err| CaduceusError::Worktree {
        context: "discover",
        stderr: format!("origin URL not parseable: {remote_url} ({err})"),
    })?;
    url.host_str()
        .map(|h| h.to_ascii_lowercase())
        .ok_or_else(|| CaduceusError::Worktree {
            context: "discover",
            stderr: format!("origin URL has no host: {remote_url}"),
        })
}

/// Run `git <args>` inside *cwd* and return stdout (post-truncate,
/// post-redact) as a `String`. Errors that include stderr are
/// preserved verbatim so the structured logger can render them.
async fn git_string(
    runner: &GitRunner,
    cwd: &Path,
    op: &'static str,
    args: &[&str],
) -> CaduceusResult<String> {
    let mut owned_args: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 1);
    owned_args.push(std::ffi::OsString::from(op));
    for s in args {
        owned_args.push(std::ffi::OsString::from(*s));
    }
    let borrowed: Vec<&OsStr> = owned_args.iter().map(|s| s.as_os_str()).collect();
    // Use the runner so this code path goes through the same
    // process-group / timeout / redaction logic as the rest of
    // the daemon. The op label becomes the first git subcommand
    // argument (e.g. `git rev-parse --git-dir`).
    let output = runner
        .run_in(&runner_inner_cfg(), op, &borrowed, Some(cwd))
        .await?;
    if output.cancelled {
        return Err(CaduceusError::Cancelled);
    }
    if output.timed_out {
        return Err(CaduceusError::Git {
            operation: op,
            stderr: output.stderr,
        });
    }
    if let Some(status) = output.status {
        if status != 0 {
            return Err(CaduceusError::Git {
                operation: op,
                stderr: output.stderr,
            });
        }
    } else {
        // No exit status (killed by signal). Surface as a
        // typed error so the structured logger attributes the
        // failure correctly.
        return Err(CaduceusError::Git {
            operation: op,
            stderr: if output.stderr.is_empty() {
                "git exited via signal".to_string()
            } else {
                output.stderr
            },
        });
    }
    if output.stdout.trim().is_empty() {
        Ok(String::new())
    } else {
        Ok(output.stdout.trim_end().to_string())
    }
}

/// Build a minimal `Config` for runner-only paths (the
/// `git_string` helper). The Config's `git_timeout_seconds`
/// field is the only one the runner consults; everything else
/// is filler so we can pass a reference into the runner.
pub(crate) fn runner_inner_cfg() -> Config {
    Config::minimal_workdir_for_runner_tests()
}
