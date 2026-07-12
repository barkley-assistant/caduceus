# /caduceus-status

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
Queue depth:    4 issues
Next head:      your-org/your-repo#338 (2 attempts)

Recent errors:
  - your-org/your-repo#336 — label removed mid-run (1 attempt)
  - your-org/your-repo#334 — label removed mid-run (1 attempt)

Rate limit:     4987 / 5000 remaining (resets 22:14:23Z)
```

## Errors

- **"Daemon not running"** — `caduceus status` failed because the daemon process isn't responding. Suggest checking `<state_dir>/processor.log` and the cron profile (`hermes cron status caduceus`).
- **"State directory missing"** — `~/.hermes/caduceus-state` doesn't exist. Suggest running `hermes cron run caduceus` once to create it, or check the config.

## See also

- `caduceus` skill — for full configuration help
- The `/caduceus-status` command lives in this plugin's `commands/` folder