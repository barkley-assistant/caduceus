#!/usr/bin/env bash
# tests/ci/test-check-commits.sh — Verify scripts/check-commits.sh enforces
# the conventional-commit subject policy.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="${REPO_ROOT}/scripts/check-commits.sh"
ORIGINAL_DIR="$(pwd)"

pass_count=0
fail_count=0

cleanup() {
  cd "${ORIGINAL_DIR}"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  fail_count=$((fail_count + 1))
}

pass() {
  echo "PASS: $*"
  pass_count=$((pass_count + 1))
}

run_case() {
  local expected_exit="$1"
  local subject="$2"
  local description="$3"
  local dir
  local actual_exit=0

  dir="$(mktemp -d)"
  (
    cd "${dir}"
    git init -q
    git -c user.name="Test User" -c user.email="test@example.com" \
      commit --allow-empty -q -m "initial: baseline"
    git -c user.name="Test User" -c user.email="test@example.com" \
      commit --allow-empty -q -m "${subject}"
    "${SCRIPT}" HEAD~1..HEAD 2>/dev/null
  ) || actual_exit=$?
  rm -rf "${dir}"

  if [ "${actual_exit}" -eq "${expected_exit}" ]; then
    pass "${description} (exit ${actual_exit})"
  else
    fail "${description}: expected exit ${expected_exit}, got ${actual_exit}"
  fi
}

run_usage_case() {
  local expected_exit="$1"
  local args="$2"
  local description="$3"
  local actual_exit=0

  # shellcheck disable=SC2086
  "${SCRIPT}" ${args} >/dev/null 2>/dev/null || actual_exit=$?

  if [ "${actual_exit}" -eq "${expected_exit}" ]; then
    pass "${description} (exit ${actual_exit})"
  else
    fail "${description}: expected exit ${expected_exit}, got ${actual_exit}"
  fi
}

if [ ! -f "${SCRIPT}" ]; then
  echo "FAIL: ${SCRIPT} does not exist" >&2
  exit 1
fi

# Good cases
run_case 0 "fix(ci): add validation to commit-policy" "valid subject with scope"
run_case 0 "feat(api)!: breaking api change" "valid breaking change subject"
run_case 0 "fix(scheduler): add max_issues_per_tick (closes #108)" "valid subject with closes ref"
run_case 0 "feat(ci): add check-commits.sh script for commit subject validation" "valid subject with .sh filename"

# Bad cases
run_case 1 "Refactor: remove planning scaffolding and simplify Rust implementation across src/ (#85)" "historical violation: uppercase type, missing scope, length"
run_case 1 "fix: simple fix" "missing scope"
run_case 1 "fix(ci): add validation." "trailing period"
run_case 1 "Fix(ci): add validation" "uppercase type"

# Usage cases
run_usage_case 2 "" "missing argument"
run_usage_case 2 "'a b'" "too many arguments"

echo ""
echo "Results: ${pass_count} passed, ${fail_count} failed"

if [ "${fail_count}" -ne 0 ]; then
  exit 1
fi
