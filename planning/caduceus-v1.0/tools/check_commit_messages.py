#!/usr/bin/env python3
"""Validate Conventional Commits subject shape for v1.0.

Implements ``CONTRACTS.md`` ``CI-003``: every commit and merge
commit MUST follow Conventional Commits 1.0.0 as
``<type>(<scope>): <description>``. Type and the required
non-empty scope are lowercase. The imperative description has no
trailing period, and the complete subject is at most 80
characters. The example ``feat(lang): add Polish language
example`` is the canonical valid shape.

The script supports two modes that GitHub Actions needs:

- ``--range <base>..<head>``: validate every commit in the PR's
  push range. Used when the repo preserves commits on merge.
- ``--squash-title "<subject>"``: validate the single squash
  title. Used when the repo enforces squash-merge. The title is
  expected to be the PR head commit subject; the script
  validates it the same way it validates a commit subject.

Exit codes:

- 0: every subject in the input is valid.
- 1: at least one subject is invalid; the report names the
  offending subject and the rule it broke.
- 2: the script could not read the input (missing range, no
  commits in the range, or the script is not inside a git
  repository when ``--range`` is used).
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys
from typing import List, Tuple


MAX_LENGTH = 80
VALID_TYPES = {
    "feat",
    "fix",
    "refactor",
    "chore",
    "build",
    "test",
    "docs",
    "perf",
    "revert",
    "ci",
    "style",
}
SUBJECT_RE = re.compile(
    r"^(?P<type>[a-z]+)\((?P<scope>[a-z][a-z0-9_-]*)\): "
    r"(?P<description>[^.\s].*[^.])$"
)
TRAILING_PERIOD_RE = re.compile(r"\.$")


def validate_subject(subject: str) -> List[str]:
    """Return the list of rule violations for one commit subject.

    An empty list means the subject is valid.
    """
    errors: List[str] = []
    if len(subject) > MAX_LENGTH:
        errors.append(
            f"subject is {len(subject)} chars; the maximum is {MAX_LENGTH}"
        )

    match = SUBJECT_RE.match(subject)
    if not match:
        # The subject does not match the strict shape. Diagnose the
        # most likely cause so the contributor can fix it without
        # trial-and-error.
        if not SUBJECT_RE.match(subject) and (
            " " not in subject
            or "(" not in subject
            or ")" not in subject
            or ": " not in subject
        ):
            errors.append(
                "subject does not match '<type>(<scope>): <description>'; "
                "the type and scope are required, both lowercase, and the "
                "subject must include ': ' before the description"
            )
        else:
            errors.append(
                "subject does not match '<type>(<scope>): <description>'; "
                "type and scope are required and must be lowercase; the "
                "description must be imperative, start with a non-period "
                "character, and end without a period"
            )
        return errors

    commit_type = match.group("type")
    scope = match.group("scope")
    description = match.group("description")

    if commit_type not in VALID_TYPES:
        errors.append(
            f"type '{commit_type}' is not in the project allowlist "
            f"({', '.join(sorted(VALID_TYPES))})"
        )
    if not scope:
        errors.append("scope is required and must be non-empty")
    if TRAILING_PERIOD_RE.search(description):
        errors.append("description must not end with a period")
    return errors


def subjects_in_range(base: str, head: str) -> List[str]:
    """Return the list of commit subjects in ``base..head``.

    Uses ``git log`` so the script works on any host with git
    installed. The function raises ``RuntimeError`` when git is
    not available or the range is invalid.
    """
    try:
        proc = subprocess.run(
            ["git", "log", "--no-merges", "--format=%s", f"{base}..{head}"],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as exc:
        raise RuntimeError("git is not installed") from exc
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(
            f"git log {base}..{head} failed: {exc.stderr.strip()}"
        ) from exc
    subjects = [line for line in proc.stdout.splitlines() if line]
    return subjects


def validate_range(base: str, head: str) -> Tuple[bool, List[str]]:
    subjects = subjects_in_range(base, head)
    if not subjects:
        return False, [
            f"no commits found in {base}..{head}; the squash-title path "
            "may be needed (--squash-title)"
        ]
    return _validate_subjects(subjects, label=f"range {base}..{head}")


def validate_squash_title(title: str) -> Tuple[bool, List[str]]:
    return _validate_subjects([title], label="squash title")


def _validate_subjects(subjects: List[str], *, label: str) -> Tuple[bool, List[str]]:
    failures: List[str] = []
    for subject in subjects:
        errors = validate_subject(subject)
        if errors:
            failures.append(f"[{label}] subject: {subject!r}")
            failures.extend(f"  - {err}" for err in errors)
    return (not failures), failures


def main() -> int:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--range", help="git rev range like <base>..<head>")
    group.add_argument(
        "--squash-title", help="the single squash title to validate"
    )
    args = parser.parse_args()

    if args.range is not None:
        try:
            base, head = args.range.split("..", 1)
        except ValueError:
            print(
                f"--range must be '<base>..<head>'; got {args.range!r}",
                file=sys.stderr,
            )
            return 2
        try:
            ok, failures = validate_range(base, head)
        except RuntimeError as exc:
            print(f"check_commit_messages: {exc}", file=sys.stderr)
            return 2
    else:
        ok, failures = validate_squash_title(args.squash_title)

    if ok:
        print("check_commit_messages: PASS")
        return 0

    print("check_commit_messages: FAIL", file=sys.stderr)
    for line in failures:
        print(f"  {line}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
