# /caduceus-status

> Legacy Markdown scaffolding. Task 0.2 migrates this behavior into the root
> Hermes adapter's explicit `ctx.register_command("caduceus-status", ...)`
> registration; Hermes Agent v0.18.2 does not auto-register this directory.

Quick status check for the Caduceus daemon.

## Usage

```
/caduceus-status
```

## Behavior

Runs `caduceus status` against the configured state directory and returns the parsed output as a chat-friendly summary.

If the daemon is currently running and active (e.g., processing an issue), include:

- The currently-running run ID and elapsed time
- A link to the live transcript log (`<state_dir>/runs/<run-id>.log`)

If the daemon is idle (304 Not Modified on last tick), include:

- Last successful run timestamp
- Queue head and queue depth
- Stale-claim reaper last-run timestamp
- Rate-limit remaining

## Example output

```
🟢 Caduceus v0.1.0 — idle

Last run:       2026-07-12T21:50:03Z (304 Not Modified)
Queue phases:   queued=4 in_progress=0 previewed=0 failed=2 skipped=0 done=12
Next head:      your-org/your-repo#338 (2 attempts)

Recent errors:
  - your-org/your-repo#336 — label removed mid-run (1 attempt)
  - your-org/your-repo#334 — label removed mid-run (1 attempt)

Rate limit:     4987 / 5000 remaining (resets 22:14:23Z)
```

## Errors

- **"No live worker"** — normal for the one-shot cron model. Use the last tick outcome and cron status to distinguish idle operation from a scheduling problem.
- **"State corrupt"** — the daemon preserved the malformed file and stopped. Follow `MIGRATION.md` recovery steps; do not edit or delete the state file in place.
- **"State directory missing"** — `~/.hermes/caduceus-state` doesn't exist. Suggest running `hermes cron run caduceus` once to create it, or check the config.

## See also

- `caduceus` skill — for full configuration help
- The `/caduceus-status` command lives in this plugin's `commands/` folder
