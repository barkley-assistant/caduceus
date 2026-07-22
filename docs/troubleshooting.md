# Troubleshooting

The symptom → diagnosis → fix table. The structure is
"what the operator sees" first, "what's actually wrong"
second, "what to do about it" third. The daemon's own
log file (`<state_dir>/processor.log`) is the
authoritative source for what happened; `caduceus status
--json` is the authoritative source for what the daemon
thinks is happening.

## Installation

### `hermes caduceus setup` fails with "rustc not found"

Rust isn't on the path that Hermes's plugin subprocess
sees. Install rustup (`https://rustup.rs/`), ensure
`~/.cargo/bin` is on `PATH` for the user that runs the
plugin, and rerun setup.

### `hermes caduceus setup` fails with "rustc does not meet the minimum version"

The installed Rust toolchain does not satisfy the project's
`Cargo.toml` `rust-version` field. Install a compatible toolchain
via `rustup` and rerun setup.

### `hermes caduceus setup` fails with "Cargo.lock would need updates"

The daemon is built with `--locked`, so `Cargo.lock`
cannot be modified by setup. This is on purpose. If
`Cargo.lock` needs updating, run `cargo update` locally,
commit the result as a `build(deps): …` commit, and
rerun setup.

<h3>
Standalone install: `caduceus run` exits 1 with "worker_command is required for
standalone"
</h3>

Standalone installs must set `worker_command`
explicitly; the daemon refuses to start without it.
Add it to your config under the `caduceus:` section;
see `configuration.md`.

## Runtime

### `caduceus status` reports `state_corrupt: true`

`<state_dir>/state.json` (or `state_meta.json`) failed
validation. **Do not edit the file in place.** See
`state-recovery.md` for the recovery procedure.

### `caduceus status` reports `last_http_status: 401`

The GitHub token is invalid or does not have the
required permissions. Check `<state_dir>/processor.log`
for the exact error. Confirm:

- `CADUCEUS_GITHUB_TOKEN` / `GITHUB_TOKEN` /
  `gh auth token` is set.
- The token has `Metadata: read`, `Contents: read/write`,
  `Issues: read/write`, `Pull requests: read/write`.
- The token has not expired.

### `caduceus status` reports `next_allowed_poll_at` is in the future

GitHub rate-limited the daemon. The daemon respects
`X-RateLimit-Reset` and will not poll before then.
This is correct behaviour; do not bypass it.

### Cron fires but `caduceus run` exits 1 with "another tick holds daemon.lock"

This is `SkippedConcurrent`, which is exit 0 normally.
Exit 1 with this message means the daemon-side lock
acquisition itself failed (filesystem permissions,
etc.). Check that `<state_dir>` is mode 0700 and owned
by the cron-running user.

### A worker hangs and never finishes

The Rust supervisor enforces `worker_timeout_seconds`.
Timed-out workers get SIGTERM, then SIGKILL after a
short grace period. Check `<state_dir>/runs/<run_id>.log`
for the bridge's transcript, and the daemon's log for
the timeout line.

If the timeout fires consistently, either the harness
is genuinely too slow for the issue (raise
`worker_timeout_seconds`), or the harness is hanging
itself (debug with `worker-bridge.py` invoked manually
in the worktree).

### An issue never gets claimed

The daemon's polling cadence (`poll_interval_seconds`)
plus the per-issue eligibility check (`next_attempt_at
<= now`) plus the issue's current phase all gate
claiming. The relevant fields in
`caduceus status --json`:

- `next_head` — the oldest queued issue.
- `next_head_earliest_eligibility` — when that issue
  is claimable.
- `phases.<phase>` counts — if `phases.done` is high
  and `phases.queued` is zero, you have nothing to
  claim.

If `phases.queued` is non-zero and `next_head` is
null, the queue is malformed; file a bug with the
JSON output.

### An issue gets claimed repeatedly and always fails with the same error

The retry budget (`max_retries_per_issue`) caps worker
failures. After three worker-attributable failures, the
issue is `Failed`. Investigate the transcript; do not
just reset the queue and re-queue it without
understanding the failure.
`caduceus queue reset owner/repo#number --dry-run`
shows what would change without applying.

### `caduceus queue reset owner/repo#number` fails with "entry is `Failed`"

That is the correct behaviour. Reset moves a terminal
entry back to `Queued`. Check `phases.failed` and the
issue's `last_error`; if you genuinely want to retry
it, `caduceus queue reset owner/repo#number` works on
`Failed` entries too. The `--dry-run` flag previews
the reset.

## The Public-Voice Rule

<h3>
My comment was rejected for "caduceus" but my issue does not mention caduceus
</h3>

The substring match is case-insensitive. A comment
that contains the letters `c-a-d-u-c-e-u-s` in any
case, in any order, with anything between or around
them, matches. Check the comment text carefully;
common false positives:

- The bridge wrote "caduceus" into the summary by
  mistake (the harness template included it).
- The harness's prompt file used the daemon's name
  and the harness echoed it back into
  `worker-result.json`.
- The issue's own body contained the string (the
  daemon does not filter inbound text, only
  outbound).

Override the default list if your case is
legitimate, or fix the bridge to not include the
daemon's name in outbound text.

## The Cron

### Cron fires but nothing happens

Check `<state_dir>/processor.log`. If the daemon is
silently exiting 0 with no log entries, your wrapper
script may be invoking the wrong binary path or with
the wrong args. Recheck `hermes caduceus cron-install`
output.

### Cron never fires

The Hermes gateway is down, or your managed cron
provider is not active. Caduceus does not own cron
delivery; the gateway does.

### I want a different cadence than every 2 minutes

The daemon's `poll_interval_seconds` is the minimum
cadence; cron can fire faster and the daemon will gate
through the cadence gate. To actually slow cron down,
edit the cron job to fire every N minutes instead of
2. There is no Hermes-side knob for this without
re-running `cron-install` against a different interval.

## The Worker

### The bridge exits 2 with "missing required env var"

The daemon isn't passing one of the `CADUCEUS_*` vars.
Check `caduceus status` for daemon-side errors; check
`<state_dir>/processor.log` for env-var-related issues.

### The bridge exits 137 (signal 9)

The daemon's hard timeout fired. Either raise
`worker_timeout_seconds` or fix the bridge to exit on
its own.

### The daemon reads `worker-result.json` but it's empty

The harness wrote 0 bytes. The harness contract
requires the file to be ≥1 byte. The bridge should
validate this before exiting 0.

## Getting More Help

Before filing an issue, gather:

- `caduceus status --json` output.
- `<state_dir>/processor.log` tail.
- The affected issue's `last_error` from
  `status --json`.
- The bridge's transcript at
  `<state_dir>/runs/<run_id>.log` (read-only; do not
  delete).

File at the project's GitHub issues. Do not include
secrets.
