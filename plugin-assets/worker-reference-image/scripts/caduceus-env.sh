#!/bin/sh
# Caduceus OCI worker contract helper (WR-2).
#
# Reads the canonical CADUCEUS_* variable set from the environment and
# prints it. The set is the frozen worker-environment contract:
# src/executor/sandbox_spec.rs::CANONICAL_ENV_KEYS (mirrored by the
# fixture-parity test).
#
#   caduceus-env.sh               print each variable as NAME=VALUE,
#                                 one per line (multi-line values are
#                                 collapsed to spaces)
#   caduceus-env.sh --names-only  print only the sorted variable names
#
# If any canonical variable is unset, the helper exits non-zero and
# names the missing variable(s) on stderr.
#
# Strict POSIX sh: no bashisms, no GNU-only flags, only utilities
# bundled with busybox.

# Canonical CADUCEUS_* names, one per line. Keep in sync with
# CANONICAL_ENV_KEYS in src/executor/sandbox_spec.rs; the
# fixture-parity test (tests/architecture/oci_fixture_parity_test.rs)
# compares --names-only output against that Rust constant.
CANONICAL_VARS="
CADUCEUS_BRANCH_NAME
CADUCEUS_CONTEXT_JSON
CADUCEUS_ISSUE_BODY
CADUCEUS_ISSUE_ID
CADUCEUS_ISSUE_LABELS_JSON
CADUCEUS_ISSUE_NUMBER
CADUCEUS_ISSUE_REPO
CADUCEUS_ISSUE_TITLE
CADUCEUS_RESULT_PATH
CADUCEUS_RUN_ID
CADUCEUS_WORKTREE_PATH
"

names_only=false
case "${1:-}" in
  --names-only) names_only=true ;;
  "")
    ;;
  *)
    echo "usage: caduceus-env.sh [--names-only]" >&2
    exit 2
    ;;
esac

missing=""
for name in $CANONICAL_VARS; do
  if ! eval "[ -n \"\${$name+x}\" ]"; then
    missing="$missing $name"
  fi
done

if [ -n "$missing" ]; then
  echo "caduceus-env.sh: missing canonical variables:$missing" >&2
  exit 1
fi

if $names_only; then
  printf '%s\n' $CANONICAL_VARS | sort
else
  for name in $CANONICAL_VARS; do
    eval "value=\${$name}"
    # Collapse newlines to spaces so every variable prints on exactly
    # one line even when a canonical value is multi-line in a real
    # worker run (e.g. CADUCEUS_ISSUE_BODY; WR-2 scenario "Print all
    # canonical variables").
    value=$(printf '%s' "$value" | tr '\n' ' ')
    printf '%s=%s\n' "$name" "$value"
  done
fi