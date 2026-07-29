#!/usr/bin/env bash
# scripts/check-commits.sh — Validate commit subjects against the
# conventional-commit policy (CONTRACTS.md CI-003).
#
# Usage:
#   bash scripts/check-commits.sh <git-range>
#
# Exit codes:
#   0 — all subjects pass
#   1 — one or more subjects violate the policy
#   2 — usage error

set -euo pipefail

readonly MAX_LEN=80
readonly RE='^(build|chore|ci|docs|feat|fix|perf|refactor|style|test)(\([a-z0-9_-]+\))!?: [a-z][^.]*$'

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  echo "usage: check-commits.sh <git-range>" >&2
  exit 2
fi

range="$1"
failures=0

while IFS= read -r line; do
  hash="${line%% *}"
  subject="${line#* }"

  if [ "${#subject}" -gt "${MAX_LEN}" ]; then
    echo "::error file=${hash},line=1,title=Commit length::Subject exceeds 80 characters (${#subject} chars)"
    failures=$((failures + 1))
  fi

  if [[ ! "${subject}" =~ ${RE} ]]; then
    echo "::error file=${hash},line=1,title=Conventional commit::Subject does not match <type>(<scope>): <description> pattern"
    failures=$((failures + 1))
  fi
done < <(git log --no-merges --format='%H %s' "${range}")

if [ "${failures}" -eq 0 ]; then
  exit 0
else
  exit 1
fi
