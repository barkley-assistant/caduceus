#!/usr/bin/env bash
# hermes_bootstrap.sh — Copy the Caduceus plugin tree into an isolated
# $HERMES_HOME. Invoked by tests/hermes_lifecycle_test.rs.
#
# Usage:
#   CARGO_MANIFEST_DIR=<repo-root> bash hermes_bootstrap.sh <hermes-home>
#
# Filters out target/, tests/, planning/, .git/, and dotfiles to keep
# the copy small (avoiding 100+ MB of build artifacts) and clean
# (avoiding stale build state that would break `hermes caduceus setup`).
# Mirrors the Python install_plugin fixture in tests/conftest.py:67-96.

set -euo pipefail

# --- validate inputs -------------------------------------------------------
: "${CARGO_MANIFEST_DIR:?CARGO_MANIFEST_DIR must be set}"
HERMES_HOME="${1:?usage: hermes_bootstrap.sh <hermes-home>}"

PLUGIN_SRC="${CARGO_MANIFEST_DIR}"
PLUGIN_DST="${HERMES_HOME}/plugins/caduceus"

mkdir -p "$(dirname "${PLUGIN_DST}")"

# --- copy plugin tree, filtering excluded dirs -----------------------------
# rsync is the cleanest way to do filtered copy in shell.
# Fall back to cp+r/find if rsync is unavailable.
if command -v rsync &>/dev/null; then
    rsync -a \
        --exclude='target/' \
        --exclude='tests/' \
        --exclude='planning/' \
        --exclude='.git/' \
        --exclude='.*' \
        "${PLUGIN_SRC}/" "${PLUGIN_DST}/"
else
    # Fallback: cp all then remove excluded dirs.
    cp -a "${PLUGIN_SRC}/." "${PLUGIN_DST}/"
    find "${PLUGIN_DST}" -maxdepth 1 -name '.*' ! -name '.' ! -name '..' -exec rm -rf {} + 2>/dev/null || true
    rm -rf "${PLUGIN_DST}/target"
    rm -rf "${PLUGIN_DST}/tests"
    rm -rf "${PLUGIN_DST}/planning"
    rm -rf "${PLUGIN_DST}/.git"
fi

# --- verify the minimum set ------------------------------------------------
if [ ! -f "${PLUGIN_DST}/plugin.yaml" ]; then
    echo "hermes_bootstrap.sh: plugin.yaml not found at ${PLUGIN_DST}/plugin.yaml" >&2
    exit 1
fi

echo "hermes_bootstrap: plugin tree copied to ${PLUGIN_DST}"