# Hermes Integration

This is the part of Caduceus that talks to Hermes:
cron delivery, the gateway dependency, the chat status
command, and the chat skill. If you installed Caduceus
standalone, you can skip this doc.

## Cron Delivery

`hermes caduceus cron-install` writes a wrapper script
and registers one cron job with Hermes. The wrapper
contains the absolute binary path and ends with
`exec <binary> run`. Hermes's cron subsystem fires the
wrapper every two minutes.

### The Cron Contract

The daemon's cron contract is independent of Hermes.
The daemon expects:

- `caduceus run` (or no-arg `caduceus`, which the CLI
  rewrites to `caduceus run` before clap parsing) is
  the only entry point a cron should use.
- The daemon writes nothing to stdout on success.
- The daemon exits 0 for every cron-contract outcome:
  processed, idle, concurrent, cadence, rate-limited,
  cancelled.
- The daemon exits 1 only for daemon-side failures
  (corrupt state, configuration error, invariant
  violation, unrecovered pipeline failure).
- The daemon's whole-tick `flock` means concurrent
  ticks are safe; the second one exits 0 with the
  `SkippedConcurrent` outcome.

If you write a different cron wrapper (because
Hermes's default does not fit your environment, or
because you are running Caduceus on a system without
Hermes), this is the contract to honour.

## The Gateway Dependency

Hermes cron jobs fire when **the gateway is running**
(or when a configured managed cron provider is active).
If the gateway is down, your `caduceus-pulse.sh`
wrapper sits on disk doing nothing.

This is documented in Hermes, not in Caduceus, because
it is Hermes's contract. Caduceus's setup output prints
a reminder; Caduceus's `hermes caduceus status` output
includes the gateway's status when invoked through
Hermes.

Operators running Caduceus on a long-lived host should
either keep the Hermes gateway up or write their own
cron wrapper that does not depend on it.

## The Chat Status Command

`/caduceus-status` is registered by `__init__.py` via
`ctx.register_command`. It invokes
`<plugin>/bin/caduceus status --json` with:

- an argument array (never a shell string),
- a short timeout,
- bounded output.

The command is a thin wrapper over `caduceus status`;
it adds the chat-safe formatting and the bounded
output. When setup has not built the binary, the
command returns a diagnostic that says "the binary
isn't installed yet; run `hermes caduceus setup`."

## The Chat Skill

`caduceus:caduceus` is registered by `__init__.py`
via `ctx.register_skill("caduceus",
<root>/skills/caduceus/SKILL.md, ...)`. **Plugin
skills are opt-in**: Hermes does not invoke them
automatically from conversational triggers. You invoke
the skill explicitly with
`apply_skill caduceus:caduceus` (or your Hermes
client's equivalent).

The skill body is in `skills/caduceus/SKILL.md` and
covers:

- What the daemon is for and what it is not.
- The operator's most-common commands (`caduceus
  status`, `caduceus queue reset`, `caduceus
  migrate-state`).
- The configuration reference (with a strong pointer
  to `docs/configuration.md` for the full schema).
- The public-voice rule and how to override it.
- The recovery procedure.
- The `worker-bridge.py` contract and how to swap
  harnesses.

The skill is not a substitute for the manual in
`docs/`. The skill is the cheat sheet an operator
keeps open in a chat; the manual is what the operator
reads when they are actually configuring something.

## Hermes Plugin Registry

Caduceus is not currently published to the Hermes
plugin registry. Installation is via
`hermes plugins install barkley-assistant/caduceus
--enable`, which clones the repository directly.

## Troubleshooting Hermes Integration

- `/caduceus-status` returns "binary not installed"
  - Likely cause: `hermes caduceus setup` not run yet
  - Fix: Run setup.
- `/caduceus-status` works but cron never fires
  - Likely cause: Gateway is down
  - Fix: Start the gateway; Caduceus cannot fix this.
- `hermes caduceus cron-install` fails with multiple matches
  - Likely cause: Previous install left stale jobs
  - Fix: Inspect with `hermes cron list`; remove manually; rerun
    `cron-install`.
- `hermes caduceus status` returns success but shows stale data
  - Likely cause: You're reading a stale `state_meta.json`
  - Fix: Check `<state_dir>/processor.log` for recent tick errors.
- `hermes caduceus` subcommand unknown
  - Likely cause: `__init__.py` is not at the plugin root
  - Fix: Check the plugin install path; check that no subdirectory was moved
    instead of the whole repo.
