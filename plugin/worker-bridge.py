#!/usr/bin/env python3
"""Caduceus harness bridge — reference implementation.

This file ships in the Caduceus Hermes plugin. After install, it's at:
    ~/.hermes/profiles/<profile>/plugins/caduceus/worker-bridge.py

The plugin manager preserves your edits across upgrades — fork this freely
to plug in a different harness (pi, codex, claude-code, anything).

The bridge does only two things:
1. Translate CADUCEUS_* env vars into the harness's CLI flags.
2. Propagate the harness's exit code so Caduceus's worker_timeout_seconds
   and transcript capture work correctly.

Everything else — worktree provisioning, polling, atomic claims,
finalize, comment posting — stays in Caduceus.

Edit `invoke_harness()` to swap harnesses. Nothing else needs to change.
"""

import os
import subprocess
import sys
import time
import threading
from pathlib import Path


def invoke_harness(worktree: Path, prompt_file: Path, run_id: str, labels: list[str]) -> int:
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
    worktree = Path(os.environ["CADUCEUS_WORKTREE_PATH"])
    prompt_file = worktree / "worker-prompt.md"
    run_id = os.environ["CADUCEUS_RUN_ID"]
    labels = [l for l in os.environ.get("CADUCEUS_ISSUE_LABELS", "").split(",") if l]

    if not prompt_file.exists():
        print(f"prompt file missing: {prompt_file}", file=sys.stderr)
        return 2

    # Write a heartbeat file every 30s so `caduceus status` can show live workers
    state_dir = Path(os.environ.get("CADUCEUS_STATE_DIR", "~/.hermes/caduceus-state")).expanduser()
    heartbeat_path = state_dir / "runs" / f"{run_id}.heartbeat"
    stop = threading.Event()

    def beat():
        while not stop.is_set():
            heartbeat_path.write_text(str(time.time()))
            stop.wait(30)

    threading.Thread(target=beat, daemon=True).start()

    try:
        return invoke_harness(worktree, prompt_file, run_id, labels)
    finally:
        stop.set()
        heartbeat_path.unlink(missing_ok=True)


if __name__ == "__main__":
    sys.exit(main())