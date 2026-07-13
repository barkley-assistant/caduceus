#!/usr/bin/env python3
"""Caduceus harness bridge — reference implementation.

This legacy source file becomes the Task 0.2 bridge template. Explicit plugin
setup seeds a user-owned copy at $HERMES_HOME/caduceus/worker-bridge.py and
never overwrites that copy during source updates.

The bridge does only two things:
1. Translate CADUCEUS_* env vars into the harness's CLI flags.
2. Propagate the harness's exit code so Caduceus's worker_timeout_seconds
   and transcript capture work correctly.

Everything else — worktree provisioning, polling, atomic claims,
finalize, comment posting — stays in Caduceus.

Edit `invoke_harness()` to swap harnesses. Nothing else needs to change.
"""

import json
import os
import subprocess
import sys
from pathlib import Path


def invoke_harness(
    worktree: Path,
    prompt_file: Path,
    run_id: str,
    labels: list[str],
    branch_name: str,
) -> int:
    """Run the configured harness. Return its exit code.

    Default: OpenCode with the gentle-orchestrator agent. The agent does
    the SDD workflow internally; Caduceus doesn't care about that — it
    just needs the harness to write worker-result.json and exit 0 on
    success.
    """
    return subprocess.run([
        "opencode", "run",
        "--agent", "gentle-orchestrator",
        "-f", str(prompt_file),
        "--", "Run the workflow per the attached prompt file.",
    ], cwd=worktree).returncode


def main() -> int:
    required = [
        "CADUCEUS_ISSUE_NUMBER",
        "CADUCEUS_ISSUE_TITLE",
        "CADUCEUS_ISSUE_BODY",
        "CADUCEUS_ISSUE_REPO",
        "CADUCEUS_CONTEXT_JSON",
        "CADUCEUS_WORKTREE_PATH",
        "CADUCEUS_RUN_ID",
        "CADUCEUS_ISSUE_LABELS_JSON",
        "CADUCEUS_BRANCH_NAME",
    ]
    missing = [name for name in required if name not in os.environ]
    if missing:
        print(f"missing required environment: {', '.join(missing)}", file=sys.stderr)
        return 2

    worktree = Path(os.environ["CADUCEUS_WORKTREE_PATH"])
    prompt_file = worktree / "worker-prompt.md"
    run_id = os.environ["CADUCEUS_RUN_ID"]
    branch_name = os.environ["CADUCEUS_BRANCH_NAME"]

    try:
        labels = json.loads(os.environ["CADUCEUS_ISSUE_LABELS_JSON"])
        if not isinstance(labels, list) or not all(isinstance(label, str) for label in labels):
            raise ValueError("expected a JSON array of strings")
    except (json.JSONDecodeError, ValueError) as exc:
        print(f"invalid CADUCEUS_ISSUE_LABELS_JSON: {exc}", file=sys.stderr)
        return 2

    if not prompt_file.exists():
        print(f"prompt file missing: {prompt_file}", file=sys.stderr)
        return 2

    # Heartbeats, timeout enforcement, transcripts, and process-tree cleanup
    # are owned by the Rust daemon. The bridge only invokes the harness.
    try:
        return invoke_harness(worktree, prompt_file, run_id, labels, branch_name)
    except FileNotFoundError as exc:
        print(f"harness executable not found: {exc.filename}", file=sys.stderr)
        return 127
    except OSError as exc:
        print(f"unable to start harness: {exc}", file=sys.stderr)
        return 126


if __name__ == "__main__":
    sys.exit(main())
