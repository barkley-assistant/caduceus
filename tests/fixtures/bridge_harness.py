#!/usr/bin/env python3
"""Deterministic test harness for ``tests/bridge_test.py``."""

from __future__ import annotations

import json
import os
import sys
import time


def main() -> int:
    log_path = os.environ.get("FAKE_HARNESS_LOG")
    if log_path:
        record = {
            "argv": list(sys.argv[1:]),
            "cwd": os.getcwd(),
            "env_keys": sorted(k for k in os.environ if k.startswith("CADUCEUS_")),
            "worktree_prompt_path": os.path.join(
                os.getcwd(), "worker-prompt.md"
            ),
        }
        with open(log_path, "a", encoding="utf-8") as fp:
            fp.write(json.dumps(record, ensure_ascii=False))
            fp.write("\n")

    if os.environ.get("FAKE_HARNESS_SLEEP") == "1":
        try:
            delay = float(os.environ.get("FAKE_HARNESS_DELAY", "5"))
        except ValueError:
            delay = 5.0
        time.sleep(delay)

    stdout = os.environ.get("FAKE_HARNESS_STDOUT", "")
    stderr = os.environ.get("FAKE_HARNESS_STDERR", "")
    if stdout:
        sys.stdout.write(stdout)
        sys.stdout.flush()
    if stderr:
        sys.stderr.write(stderr)
        sys.stderr.flush()

    try:
        exit_code = int(os.environ.get("FAKE_HARNESS_EXIT", "0"))
    except ValueError:
        exit_code = 0

    return exit_code


if __name__ == "__main__":
    sys.exit(main())

