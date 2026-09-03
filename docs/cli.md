# CLI reference

Every subcommand of the `caduceus` binary, its flags and defaults,
exit codes, and the `hermes caduceus` wrapper surface. The daemon is
driven by a cron job that runs `caduceus` (rewritten to `caduceus
run`); everything else here is operator tooling.

```text
caduceus run
caduceus status [--json]
caduceus doctor [--json] [--skip-canary] [--canary-image IMG]
caduceus worktree-gc [--older-than-days N] [--dry-run]
caduceus queue show [<owner/repo#n>] [--json]
caduceus queue reset <owner/repo#n> [--dry-run] [--json]
              [--force-finalization-reset]
caduceus queue reprocess <owner/repo#n> [--dry-run]
caduceus queue remove <owner/repo#n> [--dry-run] [--force] [--json]
caduceus migrate-state --from <path> [--dry-run]
caduceus migrate-state --to-sqlite [--dry-run]
caduceus setup [--dry-run]
```

## Conventions

- A bare `caduceus` invocation is rewritten to `caduceus run` before
  parsing, so the cron contract (silent on success) holds.
- Every subcommand resolves its configuration from
  `$CADUCEUS_CONFIG` when set, falling back to the canonical
  resolution chain (`Config::load`).
- `--json` output is a versioned envelope: the queue commands emit
  `schema: "queue/1.0"` with `app_version`, `state_dir`,
  `diagnostic`, and `payload` fields; `status` emits its own
  `version` field.
- Exit codes:

| Command | Exit codes |
|---|---|
| `run` | `0` Processed / Idle / Cancelled; `1` Failed |
| `status` | `0` valid state; `1` corrupt state or queue; `2` no state |
| `doctor` | `0`; `1` when the readiness verdict is `Unavailable` |
| all others | `0` on success; non-zero on error |

## run

Run a single tick: poll GitHub, claim at most `max_issues_per_tick`
entries, supervise workers, finalise results. This is the cron entry
point; a bare `caduceus` invocation is equivalent. Success is silent.

## status [--json]

Report daemon state: queue counts, live workers, recent errors, last
tick. `--json` prints the machine-readable report (own `version`
field). Exits `2` when no state exists yet, `1` on corrupt state or
queue, `0` otherwise.

## doctor [--json] [--skip-canary] [--canary-image IMG] [--canary-command CMD]

Check live OCI production readiness and print a human summary or JSON.
The optional diagnostic canary runs the real executor path against a
pinned image; skip it with `--skip-canary`, or configure it with
`--canary-image` + `--canary-command` (or the
`CADUCEUS_DOCTOR_CANARY_IMAGE` / `CADUCEUS_DOCTOR_CANARY_COMMAND`
environment variables). An informational report is written into the
state directory. Exits `1` when the verdict is `Unavailable`.

## worktree-gc [--older-than-days N] [--dry-run]

Sweep stale worktrees across every watched repository, defaulting to
an age threshold of 30 days. `--dry-run` reports eligible worktrees
without removing them. The sweep takes the daemon lock and refuses to
run while another tick holds it.

## queue show [<owner/repo#n>] [--json]

List every queue entry as a table (key, phase, ticket type, attempts,
generation, age) or print full detail for one entry, including its
finalization checkpoint (branch, run id, stage, PR). Read-only: `show`
snapshots under the shared state lock, never writes, and does not take
the daemon lock, so it is safe to run alongside a live tick. `--json`
emits the `queue/1.0` envelope; a missing entry yields a
`"no_entry"` diagnostic and a non-zero exit.

## queue reset <owner/repo#n> [--dry-run] [--json] [--force-finalization-reset]

Return a `Failed`, `Skipped`, or `NeedsAttention` entry to `Queued`,
clearing the retry counter and run-tracking fields. The saved
finalization checkpoint is preserved by default so a follow-up tick
resumes from the saved branch / PR; `--force-finalization-reset`
drops it after warning about the affected branch and pull request —
the remote branch and PR are never deleted by the daemon. The live
path takes the daemon lock and refuses to run while an active claim
file exists for the entry. `--dry-run` prints the planned change
without mutating anything.

## queue reprocess <owner/repo#n> [--dry-run]

Create a new generation for an issue: the generation counter is
incremented and a terminal entry moves back to `Queued` with the
retry counter cleared and no backoff, making it immediately claimable
on the next tick. Use it to fast-track a retry after fixing the root
cause, or to reopen a finished entry. Refuses only `AwaitingReview`
(human review must complete first). Distinct from `reset`, which
retries the same generation. The live path mutates state under the
queue's exclusive lock but does not take the daemon lock; prefer
running it while no tick is in flight. `--dry-run` prints the current
and would-be generation. No `--json` form.

## queue remove <owner/repo#n> [--dry-run] [--force] [--json]

Drop a queue entry entirely. Only the queue entry is removed: the
worktree, claim file, remote branch, and pull request are left for the
reaper / `worktree-gc` and are never touched under any flag. By
default it refuses `InProgress`, `AwaitingReview`, and `Done` entries;
`--force` relaxes the phase guard only — an entry with a live claim
file is always refused. If the trigger label is still on the issue,
the next poll re-enqueues a fresh entry; that is documented behaviour,
not a bug (remove the label first to keep the issue out of the
queue). The live path takes the daemon lock; `--dry-run` mirrors the
same guards read-only.

## migrate-state --from <path> [--dry-run]

Import a legacy v0 JSON state file into the current schema under
`<state_dir>/state.json`. The import is idempotent (already-present
entries are skipped), takes the daemon lock, validates every record,
and leaves live state unchanged on malformed input. A successful write
creates a timestamped backup in the state directory.

## migrate-state --to-sqlite [--dry-run]

Migrate the JSON queue into the SQLite state store and flip
`state_backend` to `sqlite` in the operator's config. The JSON file is
preserved alongside the SQLite store; rollback is config-side.

`--from` and `--to-sqlite` are mutually exclusive; one is required.

## setup [--dry-run]

Generate minimal non-secret configuration. Requires `$HERMES_HOME`
(the command errors without it). `--dry-run` prints the planned
action without writing. This is the binary's config generator only —
the plugin install flow (binary build, bridge seeding, state
directories) is `hermes caduceus setup`.

## The `hermes caduceus` wrapper

The Hermes plugin exposes eight subcommands. `queue`, `worktree-gc`,
and `migrate-state` forward every trailing token verbatim to the
binary — clap is the single source of truth for their flags.

```text
hermes caduceus setup [--dry-run]
hermes caduceus doctor [--verbose]
hermes caduceus status [--json]
hermes caduceus queue <action> [flags...]
hermes caduceus worktree-gc [flags...]
hermes caduceus migrate-state [flags...]
hermes caduceus cron-install [--dry-run] [--verbose]
hermes caduceus cron-remove [--verbose]
```

Note the two different doctors: the wrapper's `hermes caduceus doctor`
checks plugin health (binary present, bridge seeded, cron job
installed); the binary's `caduceus doctor` checks live OCI readiness.
`cron-install` and `cron-remove` exist only in the wrapper.
