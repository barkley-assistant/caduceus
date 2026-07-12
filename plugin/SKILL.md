# Caduceus Skill

The `caduceus` skill is triggered when you mention GitHub issues, auto-fix, PR automation, or queue state in chat. It surfaces the current Caduceus daemon state and helps you configure and debug it.

## Triggers

This skill fires when the user says things like:

- "What issues is caduceus working on?"
- "Show me the caduceus queue"
- "Why isn't caduceus picking up issue #338?"
- "How do I configure caduceus for repo X?"
- "Set up auto-fix for org Y"
- "Swap caduceus to use pi instead of opencode"

## Workflow

When triggered, this skill should:

1. **Check daemon status** by running `caduceus status` (or `/caduceus-status` from chat). Parse the output.
2. **If the user wants to know what's happening**: surface the queue contents, last-run timestamps, retry counts, recent errors.
3. **If the user wants to configure**: walk them through the `caduceus:` section of `~/.hermes/config.yaml`. Reference the plugin's defaults.
4. **If the user wants to swap harnesses**: explain that they edit the plugin's `worker-bridge.py` and change one function (`invoke_harness`).
5. **If something is broken**: tail `<state_dir>/processor.log` and `<state_dir>/runs/<run-id>.log` for the affected run.

## Boundaries

- **Do not edit daemon state files** directly. The state files use `flock` and atomic-claim primitives; editing them manually will corrupt the queue.
- **Do not run multiple caduceus processes** against the same `state_dir`. The atomic claims make this safe-ish but the heartbeat and cron profile assume one daemon.
- **Do not edit user-modifiable config** without confirming with the user first.

## Related commands

- `/caduceus-status` — quick status check from chat

## Reference docs

For the full daemon contract, see `planning/2026-07-12_220000-caduceus-v0.1.md` in the plugin repo.