#!/usr/bin/env bash
# Caduceus pulse wrapper — template used by `hermes caduceus cron-install`.
#
# This file lives in the plugin's plugin-assets/ directory. The actual
# installed wrapper at `$HERMES_HOME/scripts/caduceus-pulse.sh` is
# generated dynamically by the adapter with the absolute binary path of
# the installed daemon.
#
# Do not run this template directly. Use `hermes caduceus cron-install`
# to install the runtime copy.
set -euo pipefail

exec caduceus run "$@"