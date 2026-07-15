# Caduceus migration guide

This guide explains how to safely move Caduceus between supported releases,
state formats, and installations. Read the release notes for the version you
are installing before making a change; they identify the supported source and
target versions, any required commands, and any version-specific limitations.

> **Do not edit daemon state, metadata, claim files, or transcripts by hand.**
> Caduceus owns those files. Use supported commands so it can take its lock,
> validate input, and install changes atomically. See
> [state recovery](docs/state-recovery.md) for recovery details.

## Migration principles

- Upgrade through documented, supported paths. Do not skip an intermediate
  release when its release notes require a staged upgrade.
- Test an upgrade against a disposable repository before changing a shared or
  production installation.
- Keep only one active processor for a repository and trigger-label set during
  a cutover. A legacy processor is not protected by Caduceus's daemon lock.
- Use a dry run whenever the applicable command supports one, then inspect its
  report before applying changes.
- Keep generated backups until the new installation has completed a successful
  run and its state has been inspected.

## Preflight and backup

Before a migration:

1. Read the target release notes and confirm that the source version and state
   format are supported.
2. Record the active configuration and resolved state directory. Migration
   commands write only within that state directory.
3. Stop scheduled ticks and disable any legacy processor. Wait for an active
   tick to finish before continuing.
4. Copy operator-owned configuration and retain the state backups produced by
   Caduceus. Never replace a live state file by copying over it while the
   daemon may run.
5. Confirm that GitHub and Git credentials are available to the account that
   will run the daemon after the upgrade.

For HTTPS repositories, make the configured credential helper and token
available to both the daemon and its scheduler. For SSH repositories, ensure
the scheduler can access the required SSH configuration and agent. Caduceus
does not pass GitHub credentials into worker or Git environments and never
logs token values.

## Supported state imports

When release notes provide a state-import path, use the command and source
format specified for that release. The **currently shipped** v0.1 binary
supports importing a legacy JSON envelope with an `entries` array:

```text
caduceus migrate-state --from <legacy.json> [--dry-run]
```

The importer takes the daemon lock, validates every record, and adds entries
that are not already present in live state. It does not overwrite conflicting
entries. Malformed input leaves live state unchanged. A successful write uses
the normal atomic-write procedure and creates a timestamped backup in the
state directory.

Run a dry run first:

```text
caduceus migrate-state --from /path/to/legacy.json --dry-run
```

Compare the reported import and skip counts with the source data. If they are
not expected, stop and resolve the discrepancy before applying the migration.
Running the same import again is idempotent: already-present entries are
reported as skipped and are not duplicated.

> **Planned v1.0 command.** The v1.0 contract (`CONTRACTS.md` STATE-002)
> defines a different subcommand for the JSON→SQLite cutover:
> `caduceus migrate-state --to sqlite`. That flag is **not** present in
> any shipped binary. It is implemented by v1.0 Task 3.3 and only
> available in a v1.0 release. When 3.3 lands, this guide is updated to
> describe both surfaces accurately. Until then, treat
> `--from <legacy.json>` as the only supported state-import command.

## Validate and resume

After applying a migration or upgrade:

1. Run `caduceus status` and review the reported state.
2. Confirm that the expected backup exists in the state directory.
3. Run one tick against a disposable repository and verify its logs, GitHub
   access, Git credentials, and worker result.
4. Re-enable scheduling only after the test tick succeeds.
5. Monitor the first scheduled run and retain backups through that observation
   period.

If the installation includes the Hermes plugin, also run `hermes caduceus
doctor` after setup or an upgrade. A missing scheduler capability, required
gateway restart, incomplete configuration, or unavailable provider must be
treated as an actionable setup failure rather than a healthy installation.

## Rollback and recovery

If validation fails, stop scheduling before changing state. Restore only from
a backup created for the affected state directory, then restart the previous
known-good installation. Do not resume a legacy processor until the Caduceus
scheduler is disabled, and do not run both processors against the same work.

Current JSON-state imports preserve prior content as
`<state_dir>/state.json.bak-<timestamp>`. A typical rollback is:

```text
# Stop the Caduceus scheduler first.
cp <state_dir>/state.json.bak-<timestamp> <state_dir>/state.json
# Restart the known-good installation after confirming its configuration.
```

Use this only while the daemon is stopped. Releases that introduce a new state
backend may provide a different rollback procedure; their release notes take
precedence over this example.

When Caduceus detects malformed state, it preserves the rejected bytes as a
timestamped `state.json.corrupt-*` archive and refuses to proceed. Do not edit
that archive or the live state in place. Follow the supported recovery process
in [state recovery](docs/state-recovery.md); it validates replacement data,
archives the corrupt input, and installs a replacement atomically.

## Retrying failed work

Use the queue command to retry a failed item rather than changing state or
removing and re-adding labels:

```text
caduceus status
caduceus queue reset owner/repo#number --dry-run
caduceus queue reset owner/repo#number
```

The normal reset keeps the saved finalization checkpoint so a later tick can
resume safely. `--force-finalization-reset` discards that checkpoint only
after warning about the affected branch and pull request; it never deletes
remote branches or pull requests. See [troubleshooting](docs/troubleshooting.md)
for common reset failures.

## Installation changes and removal

Follow the release-specific install or upgrade instructions when plugin assets
or scheduler integration changes. For Hermes installations, remove scheduling
before removing the plugin:

```text
hermes caduceus cron-remove
hermes plugins remove caduceus
```

This preserves the state directory, user-owned bridge, configuration, watched
repositories, and worktrees for inspection or a later reinstall. Run
`caduceus worktree-gc` when it is safe to clean unused worktrees.

## Troubleshooting

- **Migration reports conflicts or unexpected skips:** do not force an import.
  Compare the source and live entries, then use an explicit queue operation if
  a reset is genuinely required.
- **Credentials fail after upgrade:** verify the daemon and scheduler run as
  the expected account and can reach the configured credential helper or SSH
  agent.
- **Scheduler is unavailable:** inspect `hermes caduceus doctor`, complete any
  required gateway restart, and follow the host's documented scheduler setup.
  Do not edit scheduler state files directly.
- **State is corrupt:** stop scheduling and follow
  [state recovery](docs/state-recovery.md). Preserve the corrupt archive and
  backup for diagnosis.

## Release-specific notes

Every release that changes configuration, state, scheduling, worker execution,
or supported host versions must include migration notes. Those notes must name
the supported starting versions, required preflight checks, commands, backup
and rollback procedure, validation steps, and any irreversible boundary. In a
conflict, the release notes for the target version take precedence over this
general guide.
