# Legacy Hermes Plugin Scaffolding

This directory is pre-implementation scaffolding from an earlier, unsupported packaging assumption. Hermes Agent v0.18.2 does not consume the custom manifest fields, Markdown command directory, cron-profile YAML, binary declaration, or lifecycle hooks represented here.

Task 0.2 of the implementation plan migrates the Hermes surface to the repository root:

```text
plugin.yaml
__init__.py
skills/caduceus/SKILL.md
plugin-assets/worker-bridge.py
plugin-assets/caduceus-pulse.sh
```

The root adapter will register the namespaced skill, `/caduceus-status`, and `hermes caduceus` CLI explicitly through Hermes' `ctx` API. `hermes caduceus setup` will build the Rust binary and seed a user-owned bridge under `$HERMES_HOME/caduceus/`; `cron-install` will create a real no-agent Hermes cron job backed by a Bash script under `$HERMES_HOME/scripts/`.

Do not install this `plugin/` subdirectory or implement its old lifecycle claims. The binding contract is `planning/2026-07-12_220000-caduceus-v0.1.md`, Amendment 0.2 and “Hermes plugin compatibility contract.”
