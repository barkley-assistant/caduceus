#!/usr/bin/env bash
# Caduceus pulse wrapper — TEMPLATE installed by `hermes caduceus cron-install`.
#
# This file lives in the plugin's plugin-assets/ directory and is used by
# setup/cron-install as a content reference. The actually-installed
# wrapper at `$HERMES_HOME/scripts/caduceus-pulse.sh` is generated
# dynamically by the adapter (it embeds the absolute binary path of the
# installed daemon and uses `exec` so the cron process is replaced by
# the daemon, not forked from a shell).
#
# Do not run this template directly. Always use `hermes caduceus
# cron-install` (or the equivalent direct invocation of the adapter's
# CLI) to install the runtime copy.
#
# Notes for the operator:
#   * The runtime wrapper is owned by the Hermes install, not by this
#     repo. `hermes caduceus cron-remove` removes it idempotently.
#   * `exec <absolute-binary-path> run` replaces the shell process with
#     the daemon so the cron process tree has no shell ancestor — that
#     keeps the worker supervisor's session/timeout plan intact.
#   * The wrapper does not pass any environment variables of its own;
#     the daemon re-reads its config via the normal resolution chain.
#   * The Hermes gateway (or a configured managed cron provider) must
#     be running for this script to fire on schedule.
set -euo pipefail

# Two lines so users see exactly what would run:
#   exec <absolute-binary-path> run
# Adapter replaces the entire body with the real installed binary path
# during cron-install.
exec caduceus run "$@"