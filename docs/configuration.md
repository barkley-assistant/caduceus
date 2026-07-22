# Configuration

This is the field-by-field reference. It is exhaustive.
The README gives you the 60-second orientation; this doc
gives you the schema.

## Resolution Order

Caduceus resolves its configuration from the first of
these that yields a config:

1. `$CADUCEUS_CONFIG` — an explicit path to a YAML file.
   Used by Hermes's plugin install path; the wrapper sets
   it.
2. `$HERMES_HOME/config.yaml` under the `caduceus:`
   section. `HERMES_HOME` defaults to `~/.hermes`.
3. `~/.config/caduceus/config.yaml` under the `caduceus:`
   section. The XDG-style fallback for standalone
   installs.

Relative `HERMES_HOME` is rejected at config-load time.
Paths expand only a leading `~`; no other shell expansion
is performed.

## The `caduceus:` YAML Shape

The YAML is the same regardless of which resolution path
loaded it:

```yaml
caduceus:
  poll_interval_seconds: 120
  state_dir: "~/.hermes/caduceus-state"
  log_path: "~/.hermes/caduceus-state/processor.log"
  workdir_base: "~/projects"

  watched_repos: []                # or ["owner/repo", ...]

  worker_command:                  # required for standalone installs
    - python3
    - ~/.hermes/caduceus/worker-bridge.py

  worker_timeout_seconds: 3600
  http_timeout_seconds: 60
  git_timeout_seconds: 300
  transcript_max_bytes: 10485760
  run_retention_days: 30
  stale_run_hours: 1

  max_retries_per_issue: 3
  retry_backoff_seconds: 300

  ticket_label_code: "🤖 auto-fix"
  ticket_label_investigation: "🤖 auto-fix-investigate"

  feedback_author_allowlist: []
  comment_ignore_patterns: []
  comment_forbidden_strings: []
  worker_env_allowlist: []

  github_token: null               # prefer CADUCEUS_GITHUB_TOKEN env var
  api_base: "https://api.github.com"

  dry_run: false                   # CADUCEUS_DRY_RUN=1 overrides this
```

## The Field Reference

Every field below is part of the stable surface. See
`RELEASING.md` for what that means in practice.

### `poll_interval_seconds`

**Type:** `u64`. **Default:** `120`. **Must be > 0.**

The minimum execution cadence. Cron may fire faster than
this; the daemon gates ticks through the cadence gate.
The gate will also defer a tick past GitHub's
`X-Poll-Interval` if that header is higher.

### `state_dir`

**Type:** path. **Default:** `$HERMES_HOME/caduceus-state`
for Hermes installs, `~/.local/share/caduceus-state` for
standalone. Mode 0700 at create time.

Location of every on-disk artefact Caduceus owns: the
queue state file, the metadata envelope, claim files,
heartbeats, transcripts, dry-run reports, the daemon lock
file, and the HTTP ETag cache.

### `log_path`

**Type:** path. **Default:** `<state_dir>/processor.log`.

Structured daemon log. Mode 0600. Rotated by external
tooling; the daemon appends.

### `workdir_base`

**Type:** path. **Default:** `~/projects`.

Parent directory under which watched repository clones
live. Caduceus does **not** clone missing repositories —
you must put each watched repo at
`<workdir_base>/<owner>/<repo>` with non-interactive Git
credentials configured. See `installation.md`.

### `watched_repos`

**Type:** `Vec<String>`. **Default:** `[]`.

Explicit `owner/repo` slugs to poll. When empty, the
daemon discovers accessible non-archived repositories via
`GET /user/repos?per_page=100&sort=full_name`. Set this
explicitly when the authenticated account can access
repositories that should not be automated.

### `worker_command`

**Type:** `Vec<String>`. **Default:** points at the
user-owned bridge under
`$HERMES_HOME/caduceus/worker-bridge.py` when the Hermes
plugin path is in play. **Required for standalone
installs.**

The exact argument array the daemon uses to invoke the
worker. Arguments only support the literal token
`${plugin_root}`, which the daemon expands to the plugin
root derived from the installed executable; no other
`${...}` interpolation is honoured.

### `worker_timeout_seconds`

**Type:** `u64`. **Default:** `3600`. **Must be > 0.**

Hard timeout cap enforced by the Rust worker supervisor
before forcefully terminating the worker session.
Timed-out workers have their session killed, the
transcript truncated, and the claim released.

### `http_timeout_seconds`

**Type:** `u64`. **Default:** `60`. **Must be > 0.**

Total timeout for each GitHub HTTP request. The connect
timeout is 10 seconds. A 60-second total budget is
generous for GitHub's documented response times.

### `git_timeout_seconds`

**Type:** `u64`. **Default:** `300`. **Must be > 0.**

Timeout for fetch, push, and other Git subprocesses.
Interactive credential prompts are disabled; if the
helper prompts, the subprocess times out and the daemon
treats it as a transient infrastructure failure (no
retry-budget cost).

### `transcript_max_bytes`

**Type:** `u64`. **Default:** `10485760` (10 MiB).

Maximum transcript bytes retained per run. The daemon
drains output continuously and writes the cap-aware
transcript to the per-run log; output past the cap is
dropped with a marker line. Cron never sees transcript
output.

### `run_retention_days`

**Type:** `u64`. **Default:** `30`. **Must be > 0.**

Retain inactive transcripts/results/reports for this
many days. Active runs (heartbeat present) and runs
whose queue entry has a `FinalizationCheckpoint` are
exempt from retention — you don't lose a resumption
target because of GC.

### `stale_run_hours`

**Type:** `u64`. **Default:** `1`. **Must be > 0.**

Automatic crash-recovery threshold. Active issue claims
older than this on the next tick are reaped; the reaped
issue returns to `Queued` and is re-claimable.
Heartbeats older than 90 seconds are already considered
stale at runtime; this is the slower belt-and-suspenders
bound.

### `max_retries_per_issue`

**Type:** `u32`. **Default:** `3`. **Must be > 0.**

Worker-attributable failed attempts allowed before the
issue transitions to `Failed`. GitHub / Git transport /
local I/O / rate-limit / operator-cancellation failures
do not consume the worker budget. With the default of
3: failure 1 → requeued with `next_attempt_at = now +
retry_backoff_seconds`; failure 2 → same; failure 3 →
`Failed` and stops being claimed automatically.

### `retry_backoff_seconds`

**Type:** `u64`. **Default:** `300`. **Must be > 0.**

Delay before retrying a worker-attributable failure.
Applies to transitions 1→2 and 2→3 only.

### `ticket_label_code` / `ticket_label_investigation`

**Type:** `String`. **Defaults:** `🤖 auto-fix` and
`🤖 auto-fix-investigate`.

The labels the daemon polls for. Both must be present on
the target repositories (the daemon doesn't auto-create
them; you do, via `gh label create`). If you change them,
do it in both Caduceus's config and the target repos at
the same time, otherwise the daemon will not pick up
issues until you re-add the new label.

### `feedback_author_allowlist`

**Type:** `Vec<String>`. **Default:** `[]`.

Each entry is a GitHub login or `id:<numeric>`. Numeric
IDs are recommended for security-sensitive contexts
because they survive username renames — a user who
renames their account to bypass an allowlist still
matches the numeric ID. The daemon extracts the numeric
ID from each comment's `user.id` field at fetch time; no
extra API call is required.

### `comment_ignore_patterns`

**Type:** `Vec<String>`. **Default:** `[]` (an empty list
means no inbound-comment filtering; the daemon does not
ship a default bot list at the configuration layer).

Ordered list of Rust `regex` expressions matched against
each comment author's login. Matching uses the regex
crate's default case-sensitive, unanchored `is_match`
semantics. An expression may opt into case-insensitivity
with its own `(?i)` flag. If any expression matches, that
author is excluded from both `issue_comments` and
`trusted_comments`. **Explicit values replace the
defaults entirely.** To keep the defaults, set the list
back to the empty list explicitly.

> The README mentions a default bot pattern list
> (`dependabot[bot]`, `github-actions[bot]`) but the
> implementation in this repository does not include
> it. Operators who want that filtering should add the
> patterns explicitly to their config.

### `comment_forbidden_strings`

**Type:** `Vec<String>`. **Default:** `[]` (the daemon
ships with no outbound forbidden strings; the rule
exists but the curated list is operator-supplied).

Ordered list of non-empty terms. Every outbound GitHub
comment, pull-request title, and pull-request body is
rejected before its corresponding API mutation when any
term matches by case-insensitive Unicode substring.
**Explicit values replace the defaults entirely.** See
[`public-voice.md`](public-voice.md) for the rationale,
the canonical list home, and how to override.

### `worker_env_allowlist`

**Type:** `Vec<String>`. **Default:** the curated
allowlist covering the harness's expected runtime
environment (see below).

Ordered list of exact variable names or `*`-suffix
prefixes. An entry is either an exact variable name
(`PATH`) or one terminal `*` prefix pattern
(`OPENAI_*`). Any other wildcard placement, empty entry,
`=`, NUL, or nonportable variable name is a configuration
error.

The inherited allowlist defaults to `PATH`, `HOME`,
`USER`, `SHELL`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`, plus
variables matching `OPENAI_*`, `ANTHROPIC_*`,
`OPENROUTER_*`, and `OPENCODE_*`. **GitHub credential
names are always denied even if you add them to the
allowlist.** This is not a configuration option. The
daemon's worker environment will never contain a GitHub
token.

### `github_token`

**Type:** `Option<String>`. **Default:** `null`.

Optional explicit GitHub API token. Prefer the env-var
fallbacks (`CADUCEUS_GITHUB_TOKEN`, `GITHUB_TOKEN`,
`gh auth token`) to avoid plaintext config secrets. When
set, this value overrides the env-var chain. Empty
values are ignored. Errors never include token contents.

### `api_base`

**Type:** `String`. **Default:** `https://api.github.com`.

GitHub REST API base URL. The daemon restricts `api_base`
to two known forms:

- The literal `https://api.github.com` for GitHub.com.
- An `https://` URL whose host matches the GHES host
  pattern for GitHub Enterprise Server.

Anything else — `http://`, arbitrary subdomains, custom
CA bundles, corporate proxies with path prefixes,
non-GitHub REST surfaces — is rejected at
`Config::load` with a configuration error. Endpoint
validation is a positive allowlist, not a forbidden-string
filter; do not rely on `comment_forbidden_strings` or any
other string-match to detect a non-GitHub endpoint. If you
need to point Caduceus at an internal GitHub proxy or
shim, open an issue so the allowlist can be extended
deliberately.

Don't set this to a GitHub instance you don't have a
token for.

### `dry_run`

**Type:** `bool`. **Default:** `false`.

`CADUCEUS_DRY_RUN` overrides YAML when its value is one
of `1`, `true`, `yes`; `0`, `false`, `no` disables it;
other values are errors at load time.

Dry-run does everything except commit / push / comment /
label-mutate / PR / issue-close. It writes a
`<state_dir>/runs/<run_id>.dry-run.md` before teardown.
A successful dry-run transitions the entry to
`Previewed`; while dry-run remains enabled, rediscovery
is a no-op. On the first non-dry tick, a still-labeled
`Previewed` entry is atomically promoted back to
`Queued`, so previewing never prevents the eventual real
run.

## Environment Variables

Caduceus reads these at process start. Set them in your
shell, your systemd unit, or your Hermes plugin wrapper.

- `$CADUCEUS_CONFIG` — Overrides the config-resolution chain with an explicit
  YAML path.
- `$CADUCEUS_DRY_RUN` — `1` / `true` / `yes` forces dry-run; `0` / `false` /
  `no` disables.
- `$CADUCEUS_GITHUB_TOKEN` — GitHub token; preferred over `github_token` in
  YAML.
- `$GITHUB_TOKEN` — GitHub token fallback.
- `gh auth token` — GitHub token last-resort (parsed via the `gh` CLI).
- `$HERMES_HOME` — Overrides `~/.hermes` for the config resolution path.
  Relative values rejected.

The daemon's own logs redact any token-shaped value that
appears in a variable name. Operators do not need to
manually redact their own env dumps; the daemon does it.

## What Config Can Change Between Ticks

Most fields are read at config-load time and held for
the lifetime of the process. Changing them requires a
daemon restart (cron restarts every 2 minutes, so a
config edit takes effect within 2 minutes of saving).
Exceptions:

- `dry_run` and `CADUCEUS_DRY_RUN` are read every tick.
  Toggling it is a one-tick operation.

Nothing else is hot-reloaded. Don't try.
