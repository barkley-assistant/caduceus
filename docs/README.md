# docs

This is the long form of the README. If you opened the
README looking for a quick install, click the back
button. If you opened it looking for the manual, you're
in the right place.

The docs are organized by what you, the operator, are
trying to do. Pick the one that matches:

- Install Caduceus on a new host: [`installation.md`](installation.md)
- Configure Caduceus after install: [`configuration.md`](configuration.md)
- Write or modify my `worker-bridge.py`: [`the-bridge.md`](the-bridge.md)
- Recover from a corrupt state file: [`state-recovery.md`](state-recovery.md)
- Understand the public-voice rule: [`public-voice.md`](public-voice.md)
- Read about internal design: [`architecture.md`](architecture.md)
- Understand the Hermes plugin lifecycle:
  [`plugin-lifecycle.md`](plugin-lifecycle.md)
- Understand Hermes integration (cron, gateway, slash commands):
  [`hermes-integration.md`](hermes-integration.md)
- Debug something that broke: [`troubleshooting.md`](troubleshooting.md)
- Skim common questions: [`faq.md`](faq.md)

The migration procedure from the legacy v0 cron processor
or from Caduceus v0.1 itself is in
[`../MIGRATION.md`](../MIGRATION.md) at the repository
root, on purpose. Operators with an outage should not
have to navigate a docs tree.

## To Be Added (Future Documentation)

The following documentation entries are planned but not
yet written. Each entry will land when the corresponding
feature ships:

- A dedicated page on the harness-extension pattern — how
  to fork the reference bridge into a custom bridge while
  reusing the daemon's env-var contract, the
  `worker-result.json` schema, and the supervisor
  discipline. This is part of the v1.0 planning work
  rather than v0.1.
- A `docs/release-notes/` directory with per-release notes
  pulled from `CHANGELOG.md` at release time. Lands with
  the first major release after this README ships.
- A `docs/operations-runbook.md` covering day-2 operations
  beyond recovery: backup rotation, log rotation, state
  dir migration between hosts, and CI for the daemon
  itself. Lands when there's a CI workflow to operate.
