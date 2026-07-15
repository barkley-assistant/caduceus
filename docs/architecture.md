# Architecture

This is the internal-design doc. If you opened it
looking for "how do I configure Caduceus?", close it
and read `configuration.md` instead. This is for people
who want to understand the daemon's design before they
extend it.

## The Two-Process Model

Caduceus is two processes:

1. **The daemon itself** — a Rust binary that owns
   polling, state, claims, worktrees, timeouts, Git,
   GitHub, retries, and the public-voice rule.
2. **The worker** — your `worker-bridge.py` (or your
   own equivalent), invoked as a child of the Rust
   worker supervisor. The daemon treats the bridge as
   a black box that reads `CADUCEUS_*` env vars and
   exits with a code.

The daemon never imports a harness library. The harness
never imports a GitHub library. They meet at a single
env-var contract and a single `worker-result.json` file.
This is the load-bearing design decision; everything
else falls out of it.

The advantages:

- **Deterministic vs. non-deterministic isolation.** A
  stalled LLM call holds the worker hostage. The
  daemon knows nothing about it. The worker's hard
  timeout is a daemon-side concern, enforced by the
  Rust supervisor, not by anything the harness can
  sabotage.
- **Multi-harness support.** OpenCode, pi, codex,
  claude-code, your own custom script — none of them
  are in the daemon's dependency tree. The daemon has
  no opinion about which LLM you call.
- **Credential hygiene.** The daemon holds the GitHub
  token; the worker has no GitHub credential, ever.
  The worker can still read your files (because
  same-user), but it cannot push to GitHub or comment
  on issues under the daemon's identity.

The cost: the env-var contract is a public surface.
It's versioned in SemVer; see `RELEASING.md`.

## The Worker Supervisor

The daemon's Rust worker supervisor is responsible for:

- Spawning the bridge in its own Unix session (new
  PGID).
- Enforcing `worker_timeout_seconds` via SIGTERM →
  SIGKILL escalation.
- Capturing the bridge's stdout and stderr into a
  bounded transcript file.
- Propagating SIGINT / SIGTERM / daemon-death to the
  worker session.
- Waiting for the worker's exit before reporting back
  to the daemon's finalize step.

The supervisor is the only place in the codebase where
unsafe code is allowed (`process_group(0)`,
`set_child_subreaper`, signal delivery). It is fenced
with `#![forbid(unsafe_code)]` at the crate root and an
explicit `allow(unsafe_code)` on the supervisor module.

The supervisor talks to the daemon over an inherited
stdin/stdout framed protocol (`READY`, `ACK`, `TERM`,
`KILL`, `DONE` opcodes). This protocol is internal; it
is not a public surface. The daemon can change it
without breaking any operator contract.

## The Lock Discipline

Two locks:

- **The daemon lock (`<state_dir>/daemon.lock`)** — a
  whole-tick nonblocking `flock`. A second cron
  invocation exits 0 without polling or claiming.
  This lock covers the entire tick.
- **The state lock (`<state_dir>/state.lock`)** — an
  exclusive `flock` taken by `StateStore` for every
  read-modify-write cycle of `state.json`. Concurrent
  `StateStore` instances pointing at the same state
  directory see the queue as strictly serialised.

The daemon lock is per-host. The state lock is per-
state directory. They are not the same lock.

Operators running `caduceus queue reset` or
`caduceus migrate-state` take the daemon lock so a
tick cannot start while the recovery is in flight.
This is why those commands can fail with "another tick
holds daemon.lock; retry after the next tick
completes."

## The Polling Loop

The daemon does not consume GitHub's events API. It
discovers repositories with paginated
`GET /user/repos?per_page=100&sort=full_name` (or reads
`watched_repos` from config), then performs one
paginated open-issue query per configured trigger
label:
```text
GET /repos/{slug}/issues?state=open&labels={label}&per_page=100
  &sort=updated&direction=desc
```

Results are merged by case-insensitive issue key.
Pull-request objects are excluded by the presence of
the `pull_request` field. Trigger labels are still
verified from each returned object's label array
rather than trusting the query alone — GitHub's labels
query is a best-effort filter, not an authoritative one.
An issue present in both the code and investigation
result sets is reported as ambiguous and is not
enqueued until an operator removes one of the labels.

Every GET page has a persisted ETag entry in
`<state_dir>/cache/http.json`. A 304 reuses the last
successfully parsed body stored with that ETag. Cache
writes are atomic. Invalid cache JSON or an invalid
ETag drops only the affected cache entry and refetches
unconditionally.

All requests set `User-Agent: caduceus/<version>`,
`Accept: application/vnd.github+json`, and
`X-GitHub-Api-Version: 2022-11-28`. All non-2xx/304
statuses become typed errors. `Link` pagination is
followed within a configurable hard maximum of 20
pages per endpoint; exceeding it is an error rather than
silent truncation.

## Why We Shell Out to Git Instead of Using libgit2

Two reasons:

1. **Credential divergence.** libgit2 does not speak
   SSH agent or your `gh` CLI credential helper the
   same way `git` does. Operators would have to
   configure credentials twice — once for normal Git
   push, once for Caduceus — and the second
   configuration is harder to test. Shelling out to
   Git means one credential config is the credential
   config.
2. **Behavioural fidelity.** The daemon needs Git's
   actual push semantics, not a faithful
   reimplementation. Using libgit2 means we own every
   quirk: how it handles non-fast-forwards, how it
   negotiates HTTP/2, how it behaves with `--depth=1`
   clones. Shelling out to Git means we get Git's
   quirks for free.

The cost: we must shell out with an argument array
(never a shell string), enforce timeouts via the Rust
supervisor, and parse Git's stderr to detect auth
failures vs. non-fast-forward vs. everything else.
That's a real cost. The alternative is worse.

## The Public-Voice Validator

Lives in `src/voice.rs`. Takes a `&str` and the
configured `comment_forbidden_strings`. Returns either
`Ok(())` or a structured refusal that names the
matching string.

The validator runs before every outbound GitHub
mutation: comments, PR titles, PR bodies, dry-run
report titles. Its refusal is recorded as a daemon-side
error, and the issue returns to `Queued` without
consuming the retry budget.

The validator is not configurable beyond the
`comment_forbidden_strings` list itself. There is no
per-run override, no per-repo override, no "I know
what I'm doing" flag. See `public-voice.md`.

## The Failure-Class Mapping

The orchestrator classifies failures into:

- **Worker** — the bridge exited non-zero, or its
  `worker-result.json` failed validation. Consumes
  the retry budget per `max_retries_per_issue`.
- **Infrastructure** — GitHub API errors, Git
  transport errors, local I/O errors, rate-limit
  responses, operator-cancellation signals. Does not
  consume the retry budget.
- **Corruption** — `state.json` or `state_meta.json`
  is malformed. The daemon refuses to start; recovery
  is documented in `state-recovery.md`.
- **Configuration** — config-load errors. The daemon
  refuses to start; fix the config and retry.
- **Invariant** — something the daemon's own logic
  considers unrecoverable (e.g., a claim file points
  at a worktree path that does not exist after a
  manual reaper ran). The daemon logs and exits
  non-zero.

The orchestrator maps these to cron-contract outcomes
(Processed, Idle, Concurrent, Cancelled, RateLimited,
Failed), and the CLI maps those to exit codes. The
cron contract is exit 0 for processed / idle /
concurrent / cadence / rate-limited / cancelled; exit
1 for failed / corrupt / configuration / invariant.

## What the Daemon Does Not Own

- The worker's stdout/stderr contents (other than
  the bounded transcript the supervisor captures).
- The worker's Git operations inside the worktree
  (the bridge does its own `git add`, `git commit`
  etc. using whatever tools it likes).
- The worker's network access (the worker has the
  daemon's `worker_env_allowlist` plus the inherited
  defaults; what the worker does with that access is
  the bridge's concern, not the daemon's).
- The GitHub account's permissions. The daemon uses
  whatever the configured token grants.

These are deliberate. The daemon is small on purpose.
