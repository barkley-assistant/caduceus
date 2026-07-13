# Caduceus Skill

Resolves as `caduceus:caduceus` after the plugin is loaded. Plugin skills
are opt-in: the skill does not appear in the system prompt's
`<available_skills>` block and conversational phrases do not trigger it
automatically. Load it explicitly through `skill_view("caduceus:caduceus")`
when the user asks about the daemon.

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

1. **Check daemon status** by running `/caduceus-status` from chat (or
   `caduceus status` from a shell). Parse the JSON or human output.
2. **If the user wants to know what's happening**: surface the queue
   contents, last-run timestamps, retry counts, recent errors.
3. **If the user wants to configure**: walk them through the
   `caduceus:` section of `~/.hermes/config.yaml`, then verify watched
   repositories exist at `<workdir_base>/<owner>/<repo>` with a matching
   noninteractive `origin`. Reference the plugin's defaults.
4. **If the user wants to swap harnesses**: explain that they edit the
   plugin's user-owned `worker-bridge.py` (default
   `~/.hermes/caduceus/worker-bridge.py`) and change one function
   (`invoke_harness`).
5. **If something is broken**: tail `<state_dir>/processor.log` and
   `<state_dir>/runs/<run-id>.log` for the affected run. For a terminal
   failed/skipped entry, show `caduceus queue reset OWNER/REPO#N
   --dry-run` before proposing the real reset; never edit state files
   directly.

## Boundaries

- **Do not edit daemon state files** directly. Use the documented
  migration/recovery commands; malformed state is preserved for
  diagnosis.
- Multiple cron invocations are safe: a host-wide nonblocking lock allows
  one tick and makes later invocations exit cleanly. Do not bypass that
  lock with custom tooling.
- **Do not edit user-modifiable config** without confirming with the user
  first.
- The plugin's bridge template (`plugin-assets/worker-bridge.py`) is a
  starting point, not a constraint. Switching harnesses means editing the
  *user-owned* copy under `$HERMES_HOME/caduceus/`; the adapter never
  overwrites it on `setup`.

## Setup

The plugin does not run manifest build/lifecycle hooks. After
installing or updating the plugin, the operator runs:

```bash
hermes caduceus setup
hermes caduceus cron-install
```

`setup` builds the Rust binary with `cargo build --release --locked`,
installs it atomically at `<plugin>/bin/caduceus`, creates the state
directories with mode 0700, and seeds the user-owned bridge under
`$HERMES_HOME/caduceus/`. `cron-install` creates the
`caduceus-pulse.sh` wrapper and a single no-agent 2-minute Hermes cron
job that calls it.

## Removal

Hermes has no plugin-uninstall hook. Operators run:

```bash
hermes caduceus cron-remove
hermes plugins remove caduceus
```

Removal preserves `$HERMES_HOME/caduceus/`, the daemon state directory,
user config, and repositories.