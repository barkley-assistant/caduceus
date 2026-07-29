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
# AGENTS.md says "no trailing period", not "no period anywhere". The previous
# regex used `[^.]*$` which incorrectly rejected valid subjects containing
# period characters in the middle (e.g. filenames like `check-commits.sh`).
# The scope group is REQUIRED per AGENTS.md (`type(<scope>): <description>`
# with `a required, non-empty scope`), so we do not allow `?` on the scope
# group. Permissive pattern + explicit trailing-period check below matches
# the AGENTS.md contract faithfully without false positives.
readonly RE='^(build|chore|ci|docs|feat|fix|perf|refactor|style|test)\([a-z0-9_-]+\)!?: [a-z].*$'
readonly TRAILING_PERIOD_RE='\.$'

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

  if [[ "${subject}" =~ ${TRAILING_PERIOD_RE} ]]; then
    echo "::error file=${hash},line=1,title=Trailing period::Subject ends with a period (AGENTS.md: no trailing period)"
    failures=$((failures + 1))
  fi
done < <(git log --no-merges --format='%H %s' "${range}")

if [ "${failures}" -eq 0 ]; then
  exit 0
else
  exit 1
fi
