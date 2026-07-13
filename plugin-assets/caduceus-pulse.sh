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
set -euo pipefail

# Two lines so users see exactly what would run:
#   exec <absolute-binary-path> run
# Adapter replaces the entire body with the real installed binary path
# during cron-install.
exec caduceus run "$@"