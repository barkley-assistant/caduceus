# Caduceus Skill

This content is migrated by Task 0.2 to the explicitly registered `caduceus:caduceus` plugin skill. Hermes plugin skills are opt-in; conversational phrases do not trigger them automatically. Once loaded, it surfaces daemon state and helps configure and debug Caduceus.

## Intended uses

Load this skill when the user asks things like:

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
3. **If the user wants to configure**: walk them through the `caduceus:` section of `~/.hermes/config.yaml`, then verify watched repositories exist at `<workdir_base>/<owner>/<repo>` with a matching noninteractive `origin`. Reference the plugin's defaults.
4. **If the user wants to swap harnesses**: explain that they edit the plugin's `worker-bridge.py` and change one function (`invoke_harness`).
5. **If something is broken**: tail `<state_dir>/processor.log` and `<state_dir>/runs/<run-id>.log` for the affected run. For a terminal failed/skipped entry, show `caduceus queue reset OWNER/REPO#N --dry-run` before proposing the real reset; never edit state directly.

## Boundaries

- **Do not edit daemon state files** directly. Use the documented migration/recovery commands; malformed state is preserved for diagnosis.
- Multiple cron invocations are safe: a host-wide nonblocking lock allows one tick and makes later invocations exit cleanly. Do not bypass that lock with custom tooling.
- **Do not edit user-modifiable config** without confirming with the user first.

## Related commands

- `/caduceus-status` — quick status check from chat

## Reference docs

For the full daemon contract, see `planning/2026-07-12_220000-caduceus-v0.1.md` in the plugin repo.
