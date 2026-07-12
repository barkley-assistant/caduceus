# Caduceus Hermes Plugin (Optional)

This directory will contain the optional Hermes-side integration plugin for Caduceus. **It is not part of v0.1 of the daemon itself** — the daemon works fully standalone without it.

## Status

The plugin does not exist yet. This README is a placeholder documenting what it will do when implemented (likely v0.2 of the daemon, or shortly after).

## Planned Capabilities

When implemented, the plugin will provide:

1. **Auto-discovery** — Detect an installed `caduceus` binary and register its state with the Hermes profile.
2. **Status integration** — Surface `caduceus status` output in the Hermes TUI status panel, alongside other agent state.
3. **Notification routing** — Forward Caduceus delivery events (issue claimed, PR opened, worker timeout) through the existing Hermes Telegram gateway so they appear in the user's chat without additional configuration.
4. **Cron profile** — Provide a default Hermes cron job entry that runs `caduceus` every 2 minutes, so users don't have to write their own crontab entry.

## Installation (planned)

Once implemented:

```bash
# From your Hermes profile root
cd ~/.hermes/profiles/<your-profile>
git clone https://github.com/barkley-assistant/caduceus.git plugins/caduceus
hermes plugin enable caduceus
```

The plugin's `plugin.yaml` will declare the capabilities above and register the cron profile.

## Architecture

The plugin is a **thin wrapper**. It does not duplicate any daemon logic. Instead:

- It reads `<state_dir>/state.json` directly (the daemon's state file) to surface queue state
- It shells out to `caduceus status` for live runtime metrics
- It reads `<state_dir>/cron.log` to surface recent activity in the TUI

The daemon's binary remains the source of truth. If the plugin is uninstalled, Caduceus continues working identically — you just lose the Hermes-specific surfaces.

## Why Optional?

We chose to make the plugin optional (not required) because:

- Caduceus is genuinely useful without Hermes — anyone with a Unix box and a GitHub repo can use it
- A required Hermes dependency would limit the daemon's open-source audience
- The "controller-worker" architecture means the daemon has zero runtime dependencies beyond `git` and the standard library — a clean, auditable footprint

## Contributing

If you'd like to help build this plugin, see `../CONTRIBUTING.md` and the planning documents in `../planning/`. Plugin implementation is not yet specced — the first step would be to write a `planning/YYYY-MM-DD_HHMMSS-caduceus-hermes-plugin.md` document that follows the same TDD-task style as the daemon's own plan.