#!/usr/bin/env python3
"""Deterministic review worker harness (caduceus #327, DAR §6.2, §7).

The review counterpart of ``bridge_harness.py``: the seam a PR-review
worker occupies when the daemon spawns it (TrustedHost exec or OCI).

Responsibilities (plan D4):

* Assert the rendered ``worker-prompt.md`` carries the six DAR §7
  sections in fixed order — a stable, deterministic check that
  survives prompt-body edits but breaks on section reordering.
* Assert the §2 schema heading's ``ReviewResult v<N>`` version and
  inject that exact version into the scripted result document — the
  daemon renders the version from its own accepted
  ``REVIEW_SCHEMA_VERSION``, so a harness ahead of the daemon cannot
  produce results the daemon rejects (DAR §7).
* Write a scripted result document selected by ``FAKE_REVIEW_RESULT``:
  ``pass``, ``fail`` (verdict-consistent FAIL with one blocking
  finding), ``inconsistent`` (FAIL with zero blocking — the daemon's
  validator rejects it as an execution failure, DAR §8), ``malformed``
  (invalid JSON), or ``oversized`` (exceeds the daemon's
  ``MAX_REVIEW_RESULT_FILE_BYTES`` read cap).

Exit codes: 0 for every scripted result except ``malformed`` (2, the
bridge execution-failure convention). Section-order or schema-version
assertions fail loudly (raise) — they pin the prompt contract.
"""

from __future__ import annotations

import json
import os
import re
import sys
from pathlib import Path

# The six fixed-order section headings (DAR §7). Section 2 carries the
# daemon-injected schema version; the harness matches its prefix and
# reads the version out of the full heading.
SECTION_HEADINGS = [
    "## 1. Daemon instructions and review policy",
    "## 2. Output schema (ReviewResult v",
    "## 3. Pull request metadata (untrusted)",
    "## 4. Review diff over merge base (untrusted)",
    "## 5. Repository context (untrusted)",
    "## 6. PR discussion (untrusted)",
]

# The daemon's result-file read cap (worker_contract.rs
# MAX_REVIEW_RESULT_FILE_BYTES). The `oversized` script exceeds this;
# the constant is mirrored here because the harness runs in the worker
# process and cannot import the Rust source. Keep in sync.
MAX_REVIEW_RESULT_FILE_BYTES = 4 << 20  # 4 MiB


def find_prompt_path() -> Path:
    """Locate ``worker-prompt.md``.

    The daemon mounts the prompt in the worktree; the worktree path is
    announced via ``CADUCEUS_WORKTREE_PATH``. ``CADUCEUS_PROMPT_PATH``
    overrides for tests that keep the prompt elsewhere.
    """
    override = os.environ.get("CADUCEUS_PROMPT_PATH")
    if override:
        return Path(override)
    worktree = os.environ.get("CADUCEUS_WORKTREE_PATH")
    if not worktree:
        raise SystemExit(
            "review harness requires CADUCEUS_WORKTREE_PATH "
            "(or CADUCEUS_PROMPT_PATH) in the environment"
        )
    return Path(worktree) / "worker-prompt.md"


def assert_section_order(prompt: str) -> None:
    """Every §1–§6 heading appears, in increasing document order."""
    positions = []
    for heading in SECTION_HEADINGS:
        pos = prompt.find(heading)
        if pos < 0:
            raise SystemExit(f"review harness: missing prompt section: {heading!r}")
        positions.append(pos)
    if positions != sorted(positions):
        raise SystemExit(
            "review harness: prompt section order violated "
            f"(DAR §7 fixed order required); positions={positions}"
        )


def schema_version_from_prompt(prompt: str) -> int:
    """Read the daemon-injected ``ReviewResult v<N>`` from §2."""
    match = re.search(r"## 2\. Output schema \(ReviewResult v(\d+)\)", prompt)
    if not match:
        raise SystemExit(
            "review harness: schema version not found in §2 "
            "(expected '## 2. Output schema (ReviewResult v<N>)')"
        )
    return int(match.group(1))


def scripted_result(kind: str, schema_version: int) -> dict:
    """Build the scripted ReviewResult document."""
    return {
        "schema_version": schema_version,
        "status": "success",
        "review": {
            "verdict": "pass" if kind == "pass" else "fail",
            "summary": (
                "Scripted PASS: no blocking findings."
                if kind == "pass"
                else "Scripted FAIL: deterministic blocking finding."
            ),
            "findings": (
                []
                if kind == "pass"
                else [
                    {
                        "severity": "blocking",
                        "title": "Scripted blocking finding",
                        "body": "Deterministic finding from the review harness.",
                        "path": "src/lib.rs",
                        "line": 1,
                        "remediation": "Fix the scripted defect.",
                    }
                ]
            ),
        },
    }


def write_result(result_path: Path, kind: str, schema_version: int) -> int:
    if kind == "malformed":
        result_path.write_text("<not valid json>", encoding="utf-8")
        return 2
    if kind == "oversized":
        # Exceed MAX_REVIEW_RESULT_FILE_BYTES so the daemon's
        # read-cap rejects it before validation.
        result_path.write_bytes(b"x" * (MAX_REVIEW_RESULT_FILE_BYTES + 1))
        return 0
    if kind == "inconsistent":
        # FAIL verdict with zero blocking findings — the daemon's
        # verdict-consistency validator rejects this (DAR §8).
        body = {
            "schema_version": schema_version,
            "status": "success",
            "review": {
                "verdict": "fail",
                "summary": "Scripted inconsistent FAIL (no blocking findings).",
                "findings": [],
            },
        }
    else:
        body = scripted_result(kind, schema_version)
    result_path.write_text(json.dumps(body), encoding="utf-8")
    return 0


def main() -> int:
    prompt_path = find_prompt_path()
    prompt = prompt_path.read_text(encoding="utf-8")

    assert_section_order(prompt)
    schema_version = schema_version_from_prompt(prompt)

    kind = os.environ.get("FAKE_REVIEW_RESULT", "pass")
    if kind not in {"pass", "fail", "inconsistent", "malformed", "oversized"}:
        raise SystemExit(f"review harness: unknown FAKE_REVIEW_RESULT {kind!r}")

    result_override = os.environ.get("CADUCEUS_RESULT_PATH")
    if result_override:
        result_path = Path(result_override)
    else:
        worktree = os.environ.get("CADUCEUS_WORKTREE_PATH")
        if not worktree:
            raise SystemExit(
                "review harness requires CADUCEUS_RESULT_PATH or CADUCEUS_WORKTREE_PATH"
            )
        result_path = Path(worktree) / "worker-result.json"

    return write_result(result_path, kind, schema_version)


if __name__ == "__main__":
    sys.exit(main())
