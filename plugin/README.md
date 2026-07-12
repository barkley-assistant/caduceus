# Caduceus Hermes Plugin

The Caduceus daemon ships as a Hermes plugin. When installed, this folder's contents are placed at `~/.hermes/profiles/<profile>/plugins/caduceus/` and registered with the active profile.

## What's here

```
plugin/
├── plugin.yaml       # Plugin manifest (read by `hermes plugin install`)
├── SKILL.md          # The "caduceus" skill — triggered by chat mentions
├── commands/
│   └── caduceus-status.md  # The /caduceus-status chat command
├── cron/
│   └── caduceus-pulse.yaml # Default cron profile (every 2 min)
└── bin/
    └── README.md     # The daemon binary gets installed here on plugin install
```

## Install lifecycle

When a user runs `hermes plugin install barkley-assistant/caduceus`, the plugin manager:

1. Copies the contents of this `plugin/` folder to the profile's plugin directory
2. **Builds and installs the Rust daemon binary** from the repo's `Cargo.toml` into `<plugin>/bin/caduceus`
3. Registers the skill, command, and cron profile with the active Hermes profile
4. Creates `<state_dir>` (`~/.hermes/caduceus-state` by default) if it doesn't exist

The binary install happens via the standard Hermes plugin lifecycle hooks (`post_install` in `plugin.yaml`). On `hermes plugin upgrade caduceus`, the binary is rebuilt and re-installed.

## Uninstall

`hermes plugin uninstall caduceus` removes the plugin folder, skill, command, and cron profile. It also stops the cron profile and removes the daemon binary. The user's `state_dir` is preserved (so re-installing later picks up where it left off), but can be manually deleted for a clean slate.

## Editing the bridge

The reference `worker-bridge.py` ships in this folder. After install, it's at:

```
~/.hermes/profiles/<profile>/plugins/caduceus/worker-bridge.py
```

Edit it directly to swap harnesses — your edits persist across plugin upgrades (the plugin manager doesn't overwrite user-modified files by default).

## Status

The plugin manifest (`plugin.yaml`), skill, command, and cron profile are scaffolding for the v0.1 implementation. The actual daemon binary doesn't exist yet — that's Phase 0+ of the planning doc.